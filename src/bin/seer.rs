// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `seer`: minimal interactive log viewer.
//!
//! Builds a [`seer::Engine`] from the file paths on the command line,
//! eagerly drains [`seer::Engine::query_events`] into pre-formatted rows,
//! then renders a single scrollable pane with vim-style key bindings.
//! Future iterations will lazy-load and wire in filters, bookmarks, and
//! tabs; this is the smallest end-to-end exercise of the parse → engine
//! → render path.

use camino::Utf8PathBuf;
use clap::Parser;
use ratatui::Frame;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use seer::{Engine, Filter, SourceError};
use std::time::Duration;

#[derive(Parser)]
#[command(about = "interactive log explorer")]
struct Args {
    /// One or more bunyan log files to read, in order.
    #[arg(required = true)]
    files: Vec<Utf8PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let mut engine = Engine::new();
    for path in &args.files {
        engine.add_file_source(path)?;
    }
    let filter = Filter::default();
    let rows: Vec<String> =
        engine.query_events(&filter).map(format_row).collect();

    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    let mut app = App::new(rows);
    while !app.quit {
        terminal.draw(|frame| render(frame, &mut app))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key);
        }
    }
    Ok(())
}

/// Restores the terminal on drop so panics and `?`-returns don't leave
/// the user's shell in raw mode / alt-screen.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

fn format_row(r: Result<seer::Event, SourceError>) -> String {
    match r {
        Ok(e) => format!(
            "{} [{}] {}/{}/{}: {}",
            e.time.to_rfc3339(),
            e.level,
            e.name,
            e.hostname,
            e.pid,
            e.msg,
        ),
        Err(err) => format!("error: {err}"),
    }
}

/// All TUI state.  Pure with respect to I/O so [`App::handle_key`] can
/// be unit-tested by feeding synthetic key events.
struct App {
    rows: Vec<String>,
    /// Index of the row currently at the top of the viewport.
    viewport_top: usize,
    /// Updated on each [`render`] call from the actual frame size.
    viewport_height: u16,
    quit: bool,
}

impl App {
    fn new(rows: Vec<String>) -> Self {
        Self {
            rows,
            viewport_top: 0,
            viewport_height: 0,
            quit: false,
        }
    }

    /// Largest valid `viewport_top`: the row index that places the last
    /// row of `rows` flush with the bottom of the viewport.
    fn max_top(&self) -> usize {
        self.rows.len().saturating_sub(self.viewport_height as usize)
    }

    fn scroll_down(&mut self, n: usize) {
        let max = self.max_top();
        self.viewport_top = (self.viewport_top + n).min(max);
    }

    fn scroll_up(&mut self, n: usize) {
        self.viewport_top = self.viewport_top.saturating_sub(n);
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Windows reports Press, Repeat, and Release; ignore the latter
        // two so a single keystroke isn't doubled.
        if key.kind != KeyEventKind::Press {
            return;
        }
        let page = self.viewport_height as usize;
        let half_page = page / 2;
        match key {
            KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.quit = true;
            }
            KeyEvent {
                code: KeyCode::Char('j') | KeyCode::Down,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.scroll_down(1);
            }
            KeyEvent {
                code: KeyCode::Char('k') | KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.scroll_up(1);
            }
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.scroll_down(half_page);
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.scroll_down(page);
            }
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.scroll_up(half_page);
            }
            KeyEvent {
                code: KeyCode::Char('g') | KeyCode::Home,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.viewport_top = 0;
            }
            // Different terminals report `G` with NONE or SHIFT; accept
            // both.  Don't accept CONTROL/ALT — those are unrelated
            // bindings the user might add later.
            KeyEvent {
                code: KeyCode::Char('G'), modifiers, ..
            } if modifiers == KeyModifiers::NONE
                || modifiers == KeyModifiers::SHIFT =>
            {
                self.viewport_top = self.max_top();
            }
            KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.viewport_top = self.max_top();
            }
            _ => {}
        }
    }
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let [content_area, footer_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    app.viewport_height = content_area.height;
    // Re-clamp in case the viewport just shrank past the previous top.
    let max_top = app.max_top();
    if app.viewport_top > max_top {
        app.viewport_top = max_top;
    }

    let total = app.rows.len();
    let top = app.viewport_top;
    let bottom = (top + content_area.height as usize).min(total);

    let lines: Vec<Line<'_>> = app.rows[top..bottom]
        .iter()
        .map(|s| Line::raw(s.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(lines), content_area);

    let footer = if total == 0 {
        "q quit · 0/0".to_string()
    } else {
        format!("q quit · {}-{} of {}", top + 1, bottom, total)
    };
    frame.render_widget(Paragraph::new(footer), footer_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn shift(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
    }

    fn app(rows: usize, height: u16) -> App {
        let rows = (0..rows).map(|i| format!("row {i}")).collect();
        let mut a = App::new(rows);
        a.viewport_height = height;
        a
    }

    #[test]
    fn q_quits() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('q')));
        assert!(a.quit);
    }

    #[test]
    fn esc_quits() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Esc));
        assert!(a.quit);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut a = app(10, 5);
        a.handle_key(ctrl('c'));
        assert!(a.quit);
    }

    #[test]
    fn j_and_down_scroll_down_one() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.viewport_top, 1);
        a.handle_key(key(KeyCode::Down));
        assert_eq!(a.viewport_top, 2);
    }

    #[test]
    fn k_and_up_scroll_up_one() {
        let mut a = app(10, 5);
        a.viewport_top = 3;
        a.handle_key(key(KeyCode::Char('k')));
        assert_eq!(a.viewport_top, 2);
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.viewport_top, 1);
    }

    #[test]
    fn ctrl_d_scrolls_half_page_down() {
        let mut a = app(100, 10);
        a.handle_key(ctrl('d'));
        assert_eq!(a.viewport_top, 5);
    }

    #[test]
    fn space_scrolls_full_page_down() {
        let mut a = app(100, 10);
        a.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(a.viewport_top, 10);
    }

    #[test]
    fn ctrl_u_scrolls_half_page_up() {
        let mut a = app(100, 10);
        a.viewport_top = 20;
        a.handle_key(ctrl('u'));
        assert_eq!(a.viewport_top, 15);
    }

    #[test]
    fn g_jumps_top() {
        let mut a = app(100, 10);
        a.viewport_top = 50;
        a.handle_key(key(KeyCode::Char('g')));
        assert_eq!(a.viewport_top, 0);
    }

    #[test]
    fn home_jumps_top() {
        let mut a = app(100, 10);
        a.viewport_top = 50;
        a.handle_key(key(KeyCode::Home));
        assert_eq!(a.viewport_top, 0);
    }

    #[test]
    fn shift_g_jumps_bottom() {
        let mut a = app(100, 10);
        a.handle_key(shift('G'));
        assert_eq!(a.viewport_top, 90);
    }

    #[test]
    fn end_jumps_bottom() {
        let mut a = app(100, 10);
        a.handle_key(key(KeyCode::End));
        assert_eq!(a.viewport_top, 90);
    }

    #[test]
    fn cant_scroll_above_top() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('k')));
        assert_eq!(a.viewport_top, 0);
        a.handle_key(ctrl('u'));
        assert_eq!(a.viewport_top, 0);
    }

    #[test]
    fn cant_scroll_below_bottom() {
        let mut a = app(10, 5);
        a.viewport_top = 5; // == max_top for 10 rows / height 5
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.viewport_top, 5);
        a.handle_key(ctrl('d'));
        assert_eq!(a.viewport_top, 5);
    }

    #[test]
    fn small_content_clamps_to_zero() {
        let mut a = app(3, 10);
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.viewport_top, 0);
        a.handle_key(shift('G'));
        assert_eq!(a.viewport_top, 0);
        a.handle_key(key(KeyCode::End));
        assert_eq!(a.viewport_top, 0);
    }

    #[test]
    fn release_events_are_ignored() {
        let mut a = app(10, 5);
        let mut k = key(KeyCode::Char('q'));
        k.kind = KeyEventKind::Release;
        a.handle_key(k);
        assert!(!a.quit);
    }

    #[test]
    fn render_paints_rows_and_footer() {
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::new(vec![
            "alpha line".to_string(),
            "beta line".to_string(),
            "gamma line".to_string(),
        ]);
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("alpha line"), "dump:\n{dump}");
        assert!(dump.contains("gamma line"), "dump:\n{dump}");
        assert!(dump.contains("1-3 of 3"), "dump:\n{dump}");
    }

    fn buffer_text(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }
}
