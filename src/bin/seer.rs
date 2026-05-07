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
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
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

    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    let mut app = App::new(engine);
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

fn render_rows(engine: &Engine, filter: &Filter) -> Vec<String> {
    engine.query_events(filter).map(format_row).collect()
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
    engine: Engine,
    filter: Filter,
    rows: Vec<String>,
    /// Index of the row currently at the top of the viewport.
    viewport_top: usize,
    /// Updated on each [`render`] call from the actual frame size.
    viewport_height: u16,
    quit: bool,
    /// When `Some`, the filter editor is open and intercepts all keys.
    dialog: Option<FilterDialog>,
}

impl App {
    fn new(engine: Engine) -> Self {
        let filter = Filter::default();
        let rows = render_rows(&engine, &filter);
        Self {
            engine,
            filter,
            rows,
            viewport_top: 0,
            viewport_height: 0,
            quit: false,
            dialog: None,
        }
    }

    /// Replaces the active filter, re-queries the engine, and resets
    /// the viewport to the top — the prior cursor is meaningless against
    /// a freshly-filtered row set.
    fn apply_filter(&mut self, filter: Filter) {
        self.rows = render_rows(&self.engine, &filter);
        self.filter = filter;
        self.viewport_top = 0;
    }

    /// Test constructor that skips the engine entirely.  The scroll
    /// tests don't care where rows come from, and unit-testing those
    /// against pre-formatted strings is much simpler than wiring up
    /// real log files.
    #[cfg(test)]
    fn with_rows(rows: Vec<String>) -> Self {
        Self {
            engine: Engine::new(),
            filter: Filter::default(),
            rows,
            viewport_top: 0,
            viewport_height: 0,
            quit: false,
            dialog: None,
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
        // While the filter editor is open it gets every keystroke —
        // otherwise typing `q` or `j` into the editor would quit or
        // scroll the underlying view.
        if let Some(dialog) = self.dialog.as_mut() {
            match dialog.handle_key(key) {
                FilterDialogResult::Stay => {}
                FilterDialogResult::Cancel => self.dialog = None,
                FilterDialogResult::Apply(filter) => {
                    self.dialog = None;
                    self.apply_filter(filter);
                }
            }
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
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog = Some(FilterDialog::new(&self.filter));
            }
            _ => {}
        }
    }
}

/// Modal editor for the active [`Filter`].
///
/// Opens prepopulated with the filter's [`Display`] form and lets the
/// user edit it as a single line of text.  The dialog re-parses the
/// buffer on every change so a parse error is shown live; pressing
/// Enter only applies the filter when it parses cleanly, and Escape
/// always discards the edits.
struct FilterDialog {
    /// Editable buffer, byte-indexed by [`Self::cursor`].
    text: String,
    /// Insertion point as a byte offset into `text`.  Always sits on a
    /// `char` boundary.
    cursor: usize,
    /// Most recent parse error for `text`, or `None` when it parses.
    parse_error: Option<String>,
}

/// Outcome of a single keystroke routed to the dialog.
enum FilterDialogResult {
    /// Keep the dialog open with no further action.
    Stay,
    /// Close the dialog without changing the active filter.
    Cancel,
    /// Close the dialog and install this filter.
    Apply(Filter),
}

impl FilterDialog {
    fn new(current: &Filter) -> Self {
        let text = current.to_string();
        let cursor = text.len();
        let mut d = Self { text, cursor, parse_error: None };
        d.reparse();
        d
    }

    fn reparse(&mut self) {
        self.parse_error = match self.text.parse::<Filter>() {
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        };
    }

    fn handle_key(&mut self, key: KeyEvent) -> FilterDialogResult {
        match key {
            KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                ..
            } => FilterDialogResult::Cancel,
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => match self.text.parse::<Filter>() {
                Ok(f) => FilterDialogResult::Apply(f),
                Err(e) => {
                    self.parse_error = Some(e.to_string());
                    FilterDialogResult::Stay
                }
            },
            KeyEvent {
                code: KeyCode::Backspace, ..
            } => {
                self.backspace();
                FilterDialogResult::Stay
            }
            KeyEvent {
                code: KeyCode::Delete, ..
            } => {
                self.delete();
                FilterDialogResult::Stay
            }
            KeyEvent {
                code: KeyCode::Left,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.move_left();
                FilterDialogResult::Stay
            }
            KeyEvent {
                code: KeyCode::Right,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.move_right();
                FilterDialogResult::Stay
            }
            KeyEvent {
                code: KeyCode::Home,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.cursor = 0;
                FilterDialogResult::Stay
            }
            KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.cursor = self.text.len();
                FilterDialogResult::Stay
            }
            // Readline-style line editing.  ^U kills to BOL, ^W kills
            // the previous whitespace-delimited word (matching shell
            // behaviour, so a whole `name=Nexus` token disappears at
            // once), and Alt-B/Alt-F move by alphanumeric word so the
            // cursor can step inside a token like `level>=warn`.
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.kill_to_start();
                FilterDialogResult::Stay
            }
            KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.kill_word_backward();
                FilterDialogResult::Stay
            }
            KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::ALT,
                ..
            } => {
                self.cursor = backward_word(&self.text, self.cursor);
                FilterDialogResult::Stay
            }
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::ALT,
                ..
            } => {
                self.cursor = forward_word(&self.text, self.cursor);
                FilterDialogResult::Stay
            }
            // Plain typing: accept Char events with no modifiers other
            // than Shift (for capitals/symbols).  Anything with Ctrl/
            // Alt/Super is ignored so e.g. Ctrl-c doesn't get inserted
            // as a literal `c`.
            KeyEvent { code: KeyCode::Char(c), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.reparse();
                FilterDialogResult::Stay
            }
            _ => FilterDialogResult::Stay,
        }
    }

    fn kill_to_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.text.replace_range(0..self.cursor, "");
        self.cursor = 0;
        self.reparse();
    }

    fn kill_word_backward(&mut self) {
        let start = backward_whitespace_word(&self.text, self.cursor);
        if start == self.cursor {
            return;
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.reparse();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.reparse();
    }

    fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
        self.reparse();
    }

    fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = prev_char_boundary(&self.text, self.cursor);
    }

    fn move_right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.cursor = next_char_boundary(&self.text, self.cursor);
    }
}

fn prev_char_boundary(s: &str, byte_idx: usize) -> usize {
    s[..byte_idx]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_char_boundary(s: &str, byte_idx: usize) -> usize {
    s[byte_idx..]
        .chars()
        .next()
        .map(|c| byte_idx + c.len_utf8())
        .unwrap_or(byte_idx)
}

/// Readline `forward-word`: advance past any non-alphanumeric run, then
/// past the alphanumeric run, landing at the byte index just after the
/// next word.  Returns `s.len()` if there is no further word.
fn forward_word(s: &str, byte_idx: usize) -> usize {
    let bytes = s.as_bytes();
    let len = s.len();
    let mut i = byte_idx;
    while i < len && !bytes[i].is_ascii_alphanumeric() {
        i += 1;
    }
    while i < len && bytes[i].is_ascii_alphanumeric() {
        i += 1;
    }
    i
}

/// Readline `backward-word`: step back over any non-alphanumeric run,
/// then over the alphanumeric run, landing at the start of the previous
/// word.  Returns `0` if no earlier word exists.
fn backward_word(s: &str, byte_idx: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = byte_idx;
    while i > 0 && !bytes[i - 1].is_ascii_alphanumeric() {
        i -= 1;
    }
    while i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
        i -= 1;
    }
    i
}

/// Start of the previous whitespace-delimited word, used by ^W.  This
/// is more aggressive than [`backward_word`] — it treats anything
/// non-whitespace as part of the word, so a whole `name=Nexus` token is
/// killed in one shot.
fn backward_whitespace_word(s: &str, byte_idx: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = byte_idx;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    while i > 0 && !bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i
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
        "q quit · f filter · 0/0".to_string()
    } else {
        format!(
            "q quit · f filter · {}-{} of {}",
            top + 1,
            bottom,
            total,
        )
    };
    frame.render_widget(Paragraph::new(footer), footer_area);

    if let Some(dialog) = app.dialog.as_ref() {
        render_filter_dialog(frame, dialog, area);
    }
}

/// Carves a centered popup over `area` and draws the filter editor.
///
/// Inside a single bordered block the layout is:
/// ```text
/// ┌── Filter (Esc cancel · Enter apply) ─┐
/// │ level>=warn name=Nexus               │
/// │ <error message, when present>        │
/// └──────────────────────────────────────┘
/// ```
fn render_filter_dialog(
    frame: &mut Frame,
    dialog: &FilterDialog,
    area: Rect,
) {
    let popup = popup_area(area, 70, 5);
    // Clear the underlying rows so the editor isn't drawn on top of
    // them.
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title("Filter (Esc cancel · Enter apply)");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [edit_area, error_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(Line::raw(dialog.text.as_str())),
        edit_area,
    );

    // Cursor column: filter syntax is ASCII so byte offset == column.
    // If we ever accept multibyte chars in the buffer we'd need to
    // compute the display width here instead.
    let col = edit_area
        .x
        .saturating_add(u16::try_from(dialog.cursor).unwrap_or(u16::MAX));
    let col = col.min(edit_area.x.saturating_add(edit_area.width));
    frame.set_cursor_position(Position::new(col, edit_area.y));

    if let Some(err) = &dialog.parse_error
        && error_area.height > 0
    {
        frame.render_widget(
            Paragraph::new(Line::raw(err.as_str()))
                .style(Style::new().fg(Color::Red)),
            error_area,
        );
    }
}

fn popup_area(area: Rect, percent_width: u16, height: u16) -> Rect {
    let width = area.width.saturating_mul(percent_width) / 100;
    let height = height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
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
        let mut a = App::with_rows(rows);
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
        let mut a = App::with_rows(vec![
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

    // ---------- filter dialog ----------

    /// Drives the dialog through a sequence of [`KeyEvent`]s and asserts
    /// it is still open afterwards.  Returns a reference for follow-up
    /// inspection.
    fn type_into(d: &mut FilterDialog, s: &str) {
        for c in s.chars() {
            match d.handle_key(key(KeyCode::Char(c))) {
                FilterDialogResult::Stay => {}
                _ => panic!("typing {c:?} unexpectedly closed dialog"),
            }
        }
    }

    #[test]
    fn f_opens_filter_dialog() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        assert!(a.dialog.is_some());
    }

    #[test]
    fn dialog_keys_do_not_quit_or_scroll() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        // 'q' goes into the buffer, not to the quit binding.
        a.handle_key(key(KeyCode::Char('q')));
        assert!(!a.quit);
        assert!(a.dialog.is_some());
        // 'j' goes into the buffer, not to scroll.
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.viewport_top, 0);
        assert_eq!(a.dialog.as_ref().unwrap().text, "qj");
    }

    #[test]
    fn dialog_prepopulates_with_current_filter() {
        let f: Filter = "level>=warn name=Nexus".parse().unwrap();
        let d = FilterDialog::new(&f);
        assert_eq!(d.text, "level>=warn name=Nexus");
        // Cursor is at the end so the user can extend the filter
        // without homing first.
        assert_eq!(d.cursor, d.text.len());
        assert!(d.parse_error.is_none());
    }

    #[test]
    fn dialog_typing_inserts_at_cursor() {
        let mut d = FilterDialog::new(&Filter::default());
        type_into(&mut d, "name=Nexus");
        assert_eq!(d.text, "name=Nexus");
        assert_eq!(d.cursor, "name=Nexus".len());
    }

    #[test]
    fn dialog_backspace_deletes_char_before_cursor() {
        let mut d = FilterDialog::new(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Backspace));
        assert_eq!(d.text, "ab");
        assert_eq!(d.cursor, 2);
    }

    #[test]
    fn dialog_left_right_move_cursor() {
        let mut d = FilterDialog::new(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Left));
        assert_eq!(d.cursor, 2);
        d.handle_key(key(KeyCode::Home));
        assert_eq!(d.cursor, 0);
        d.handle_key(key(KeyCode::Right));
        assert_eq!(d.cursor, 1);
        d.handle_key(key(KeyCode::End));
        assert_eq!(d.cursor, 3);
    }

    #[test]
    fn dialog_delete_removes_char_after_cursor() {
        let mut d = FilterDialog::new(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(key(KeyCode::Delete));
        assert_eq!(d.text, "bc");
        assert_eq!(d.cursor, 0);
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn dialog_ctrl_u_kills_to_start_of_line() {
        let mut d = FilterDialog::new(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        // Position cursor inside "Nexus".
        for _ in 0..3 {
            d.handle_key(key(KeyCode::Left));
        }
        let cursor_before = d.cursor;
        d.handle_key(ctrl('u'));
        assert_eq!(d.text, "xus");
        assert_eq!(d.cursor, 0);
        assert!(cursor_before > 0);
    }

    #[test]
    fn dialog_ctrl_u_at_start_is_noop() {
        let mut d = FilterDialog::new(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(ctrl('u'));
        assert_eq!(d.text, "abc");
        assert_eq!(d.cursor, 0);
    }

    #[test]
    fn dialog_ctrl_w_kills_previous_whitespace_word() {
        let mut d = FilterDialog::new(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        d.handle_key(ctrl('w'));
        // The whole `name=Nexus` token disappears, plus the space.
        assert_eq!(d.text, "level>=warn ");
        assert_eq!(d.cursor, "level>=warn ".len());
    }

    #[test]
    fn dialog_ctrl_w_consumes_trailing_whitespace_first() {
        let mut d = FilterDialog::new(&Filter::default());
        type_into(&mut d, "name=Nexus   ");
        d.handle_key(ctrl('w'));
        assert_eq!(d.text, "");
        assert_eq!(d.cursor, 0);
    }

    #[test]
    fn dialog_alt_b_moves_back_one_alphanumeric_word() {
        let mut d = FilterDialog::new(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        // From end-of-line: alt-B moves to start of "Nexus".
        d.handle_key(alt('b'));
        assert_eq!(&d.text[d.cursor..], "Nexus");
        // Again: start of "name".
        d.handle_key(alt('b'));
        assert_eq!(&d.text[d.cursor..], "name=Nexus");
        // Again: start of "warn".
        d.handle_key(alt('b'));
        assert_eq!(&d.text[d.cursor..], "warn name=Nexus");
        // Again: start of "level".
        d.handle_key(alt('b'));
        assert_eq!(d.cursor, 0);
        // Once more: clamped at zero.
        d.handle_key(alt('b'));
        assert_eq!(d.cursor, 0);
    }

    #[test]
    fn dialog_alt_f_moves_forward_one_alphanumeric_word() {
        let mut d = FilterDialog::new(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        d.handle_key(key(KeyCode::Home));
        // alt-F lands just past "level".
        d.handle_key(alt('f'));
        assert_eq!(&d.text[..d.cursor], "level");
        // Past "warn".
        d.handle_key(alt('f'));
        assert_eq!(&d.text[..d.cursor], "level>=warn");
        // Past "name".
        d.handle_key(alt('f'));
        assert_eq!(&d.text[..d.cursor], "level>=warn name");
        // Past "Nexus" — at end of buffer.
        d.handle_key(alt('f'));
        assert_eq!(d.cursor, d.text.len());
        // Once more: clamped.
        d.handle_key(alt('f'));
        assert_eq!(d.cursor, d.text.len());
    }

    #[test]
    fn dialog_shows_parse_error_live() {
        let mut d = FilterDialog::new(&Filter::default());
        type_into(&mut d, "bogus");
        assert!(d.parse_error.is_some());
        // Replace the buffer with something valid by clearing it.
        for _ in 0..d.text.len() {
            d.handle_key(key(KeyCode::Backspace));
        }
        type_into(&mut d, "level>=warn");
        assert!(d.parse_error.is_none());
    }

    #[test]
    fn dialog_enter_with_invalid_filter_keeps_dialog_open() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "bogus");
        a.handle_key(key(KeyCode::Enter));
        // Dialog still open, error reported.
        let d = a.dialog.as_ref().expect("dialog should still be open");
        assert!(d.parse_error.is_some());
    }

    #[test]
    fn dialog_enter_with_valid_filter_applies_and_closes() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none());
        assert_eq!(a.filter.to_string(), "level>=warn");
    }

    #[test]
    fn dialog_escape_discards_changes() {
        let mut a = app(10, 5);
        let original_filter = a.filter.to_string();
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "name=Nexus");
        a.handle_key(key(KeyCode::Esc));
        assert!(a.dialog.is_none());
        assert_eq!(a.filter.to_string(), original_filter);
    }

    #[test]
    fn dialog_apply_resets_viewport_and_requeries_engine() {
        // Build a real engine with a tiny bunyan file so apply_filter
        // can re-run query_events.  This is the only filter-dialog
        // test that isn't pure state-machine.
        use camino_tempfile::tempdir;
        use slog::{Drain, Logger, info, o, warn};
        use std::fs::OpenOptions;
        use std::sync::Mutex;

        let dir = tempdir().unwrap();
        let path = dir.path().join("a.log");
        {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            let drain = slog_bunyan::with_name("Nexus", file)
                .build()
                .fuse();
            let log = Logger::root(Mutex::new(drain).fuse(), o!());
            for _ in 0..5 {
                info!(log, "info entry");
            }
            warn!(log, "warn entry");
        }

        let mut engine = Engine::new();
        engine.add_file_source(&path).unwrap();
        let mut a = App::new(engine);
        a.viewport_height = 2;
        a.viewport_top = 3;
        assert_eq!(a.rows.len(), 6);

        // Open dialog, restrict to warnings, apply.
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));

        assert!(a.dialog.is_none());
        assert_eq!(a.rows.len(), 1);
        assert_eq!(a.viewport_top, 0);
    }

    #[test]
    fn render_draws_dialog_with_error() {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "bogus");
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("Filter"), "dump:\n{dump}");
        assert!(dump.contains("bogus"), "dump:\n{dump}");
        // Some fragment of the parser error should be visible.
        assert!(
            dump.contains("operator") || dump.contains("token"),
            "expected a parse error in dump:\n{dump}",
        );
    }
}
