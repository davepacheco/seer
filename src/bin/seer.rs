// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `seer`: minimal interactive log viewer.
//!
//! Builds a [`seer::Engine`] from the file paths on the command line and
//! presents one or more tabs over it.  Each [`Tab`] is an independent
//! view with its own [`Filter`] and scroll position; the engine itself
//! (and therefore the underlying sources) is shared.  Future iterations
//! will lazy-load and wire in bookmarks and richer log streams; this is
//! the smallest end-to-end exercise of the parse → engine → render
//! path.

use camino::Utf8PathBuf;
use clap::Parser;
use ratatui::Frame;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph, Tabs};
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

/// One independent view: name, filter, the rows produced by querying
/// the engine with that filter, and the scroll offset within those
/// rows.
struct Tab {
    name: String,
    filter: Filter,
    rows: Vec<String>,
    /// Index of the row at the top of the viewport.
    viewport_top: usize,
}

impl Tab {
    fn new(name: String, engine: &Engine, filter: Filter) -> Self {
        let rows = render_rows(engine, &filter);
        Self { name, filter, rows, viewport_top: 0 }
    }

    fn apply_filter(&mut self, engine: &Engine, filter: Filter) {
        self.rows = render_rows(engine, &filter);
        self.filter = filter;
        self.viewport_top = 0;
    }

    /// Largest valid `viewport_top`: the row index that places the last
    /// row of `rows` flush with the bottom of the viewport.
    fn max_top(&self, viewport_height: u16) -> usize {
        self.rows.len().saturating_sub(viewport_height as usize)
    }

    fn scroll_down(&mut self, n: usize, viewport_height: u16) {
        let max = self.max_top(viewport_height);
        self.viewport_top = (self.viewport_top + n).min(max);
    }

    fn scroll_up(&mut self, n: usize) {
        self.viewport_top = self.viewport_top.saturating_sub(n);
    }
}

/// All TUI state.  Pure with respect to I/O so [`App::handle_key`] can
/// be unit-tested by feeding synthetic key events.
struct App {
    engine: Engine,
    /// Open tabs, in display order.  Invariant: never empty (closing
    /// the last tab pushes a fresh one to maintain this).
    tabs: Vec<Tab>,
    /// Index into `tabs` of the currently visible one.
    active: usize,
    /// Monotonically-increasing counter used to name new tabs.  Never
    /// reused — closing "Tab 2" leaves the next new tab named "Tab 4"
    /// rather than overlapping with an existing "Tab 3".
    next_tab_number: usize,
    /// Updated on each [`render`] call from the actual frame size.
    viewport_height: u16,
    quit: bool,
    /// When `Some`, this dialog is open and intercepts all keys.
    dialog: Option<Dialog>,
}

impl App {
    fn new(engine: Engine) -> Self {
        let mut a = Self {
            engine,
            tabs: Vec::new(),
            active: 0,
            next_tab_number: 1,
            viewport_height: 0,
            quit: false,
            dialog: None,
        };
        a.push_tab(Filter::default());
        a
    }

    /// Pushes a new tab with the given filter and switches focus to
    /// it.  Does *not* open the filter dialog — callers that want that
    /// (e.g. Ctrl-T) do it explicitly after.
    fn push_tab(&mut self, filter: Filter) {
        let name = format!("Tab {}", self.next_tab_number);
        self.next_tab_number += 1;
        let tab = Tab::new(name, &self.engine, filter);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Replaces the active tab's filter, re-queries the engine, and
    /// resets that tab's viewport to the top.  Other tabs are
    /// untouched.
    fn apply_filter(&mut self, filter: Filter) {
        let engine = &self.engine;
        self.tabs[self.active].apply_filter(engine, filter);
    }

    fn rename_active_tab(&mut self, name: String) {
        self.tabs[self.active].name = name;
    }

    fn next_tab(&mut self) {
        self.active = (self.active + 1) % self.tabs.len();
    }

    fn prev_tab(&mut self) {
        self.active =
            (self.active + self.tabs.len() - 1) % self.tabs.len();
    }

    /// Removes the active tab.  When the last tab is closed, a fresh
    /// default tab is created so the `!tabs.is_empty()` invariant
    /// holds.
    fn close_active_tab(&mut self) {
        self.tabs.remove(self.active);
        if self.tabs.is_empty() {
            self.push_tab(Filter::default());
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
    }

    /// Test constructor that skips the engine entirely.  The scroll
    /// tests don't care where rows come from, and unit-testing those
    /// against pre-formatted strings is much simpler than wiring up
    /// real log files.
    #[cfg(test)]
    fn with_rows(rows: Vec<String>) -> Self {
        let mut a = Self {
            engine: Engine::new(),
            tabs: Vec::new(),
            active: 0,
            // The first push_tab below consumes "Tab 1".
            next_tab_number: 1,
            viewport_height: 0,
            quit: false,
            dialog: None,
        };
        // Manually push so we can override `rows` (the engine has no
        // sources, so a real push_tab would yield an empty Vec).
        a.tabs.push(Tab {
            name: format!("Tab {}", a.next_tab_number),
            filter: Filter::default(),
            rows,
            viewport_top: 0,
        });
        a.next_tab_number += 1;
        a
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Windows reports Press, Repeat, and Release; ignore the latter
        // two so a single keystroke isn't doubled.
        if key.kind != KeyEventKind::Press {
            return;
        }
        // While any dialog is open it gets every keystroke — otherwise
        // typing `q` or `j` into the editor would quit or scroll the
        // underlying view, and Ctrl-W would close the host tab from
        // under the dialog.
        if let Some(dialog) = self.dialog.as_mut() {
            match dialog.handle_key(key) {
                DialogResult::Stay => {}
                DialogResult::Cancel => self.dialog = None,
                DialogResult::ApplyFilter(filter) => {
                    self.dialog = None;
                    self.apply_filter(filter);
                }
                DialogResult::ApplyRename(name) => {
                    self.dialog = None;
                    self.rename_active_tab(name);
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
                let h = self.viewport_height;
                self.active_tab_mut().scroll_down(1, h);
            }
            KeyEvent {
                code: KeyCode::Char('k') | KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.active_tab_mut().scroll_up(1);
            }
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                let h = self.viewport_height;
                self.active_tab_mut().scroll_down(half_page, h);
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let h = self.viewport_height;
                self.active_tab_mut().scroll_down(page, h);
            }
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.active_tab_mut().scroll_up(half_page);
            }
            KeyEvent {
                code: KeyCode::Char('g') | KeyCode::Home,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.active_tab_mut().viewport_top = 0;
            }
            // Different terminals report `G` with NONE or SHIFT; accept
            // both.  Don't accept CONTROL/ALT — those are unrelated
            // bindings the user might add later.
            KeyEvent {
                code: KeyCode::Char('G'), modifiers, ..
            } if modifiers == KeyModifiers::NONE
                || modifiers == KeyModifiers::SHIFT =>
            {
                let max = self.active_tab().max_top(self.viewport_height);
                self.active_tab_mut().viewport_top = max;
            }
            KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let max = self.active_tab().max_top(self.viewport_height);
                self.active_tab_mut().viewport_top = max;
            }
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog =
                    Some(Dialog::filter(&self.active_tab().filter));
            }
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog =
                    Some(Dialog::rename(&self.active_tab().name));
            }
            // Ctrl-T: open a fresh tab cloning the current filter and
            // immediately drop into the filter dialog so the user can
            // tailor it before any rendering surprises them.
            KeyEvent {
                code: KeyCode::Char('t'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                let cloned = self.active_tab().filter.clone();
                self.push_tab(cloned);
                self.dialog =
                    Some(Dialog::filter(&self.active_tab().filter));
            }
            // Tab cycles forward; Shift-Tab cycles back.  Some
            // terminals send Shift-Tab as `BackTab`, others as `Tab`
            // with the SHIFT modifier — accept both.
            KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.next_tab();
            }
            KeyEvent {
                code: KeyCode::BackTab, ..
            }
            | KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::SHIFT,
                ..
            } => {
                self.prev_tab();
            }
            // Ctrl-W: close active tab.  When a dialog is open this
            // arm doesn't fire (we returned earlier), so the dialog's
            // editor still sees Ctrl-W as kill-word.
            KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.close_active_tab();
            }
            _ => {}
        }
    }
}

/// Single-line text editor used by both [`Dialog`] variants.  Owns the
/// buffer and cursor position and handles the readline-style keystrokes
/// the dialogs share; the dialog wrapping it is responsible for
/// Esc/Enter and any per-dialog side effects (e.g. parsing a filter).
struct LineEditor {
    /// Editable buffer, byte-indexed by [`Self::cursor`].
    text: String,
    /// Insertion point as a byte offset into `text`.  Always sits on a
    /// `char` boundary.
    cursor: usize,
}

/// Whether [`LineEditor::handle_edit`] consumed the keystroke.
enum EditAction {
    /// The key matched an editing binding (typing, motion, kill, etc.).
    /// The buffer/cursor may have changed; the dialog should refresh
    /// any derived state (such as the filter parse error).
    Handled,
    /// The key didn't match any editor binding.  The caller should
    /// interpret it (e.g. as Esc/Enter) or ignore it.
    Unhandled,
}

impl LineEditor {
    fn new(text: String) -> Self {
        let cursor = text.len();
        Self { text, cursor }
    }

    fn handle_edit(&mut self, key: KeyEvent) -> EditAction {
        match key {
            KeyEvent {
                code: KeyCode::Backspace, ..
            } => {
                self.backspace();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Delete, ..
            } => {
                self.delete();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Left,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.move_left();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Right,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.move_right();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Home,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.cursor = 0;
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.cursor = self.text.len();
                EditAction::Handled
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
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.kill_word_backward();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::ALT,
                ..
            } => {
                self.cursor = backward_word(&self.text, self.cursor);
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::ALT,
                ..
            } => {
                self.cursor = forward_word(&self.text, self.cursor);
                EditAction::Handled
            }
            // Plain typing: accept Char events with no modifiers other
            // than Shift (for capitals/symbols).  Anything with Ctrl/
            // Alt/Super is left to the caller.
            KeyEvent { code: KeyCode::Char(c), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                EditAction::Handled
            }
            _ => EditAction::Unhandled,
        }
    }

    fn kill_to_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.text.replace_range(0..self.cursor, "");
        self.cursor = 0;
    }

    fn kill_word_backward(&mut self) {
        let start = backward_whitespace_word(&self.text, self.cursor);
        if start == self.cursor {
            return;
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
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

/// Modal text dialog overlaying the main view.
///
/// Both variants share the same UX (Esc cancels, Enter applies) and
/// the same line-editing keys via [`LineEditor`].  The variants differ
/// in what "apply" means and whether there's parse feedback while
/// editing.
enum Dialog {
    /// Editing the active tab's [`Filter`].  Re-parses on every change
    /// so a parse error is visible live; Enter only commits when the
    /// buffer parses cleanly.
    Filter { editor: LineEditor, parse_error: Option<String> },
    /// Editing the active tab's display name.  Any string (including
    /// empty) is acceptable, so there's no parse feedback.
    Rename { editor: LineEditor },
}

/// Outcome of one keystroke routed to the dialog.
enum DialogResult {
    /// Keep the dialog open with no further action.
    Stay,
    /// Close the dialog without changing app state.
    Cancel,
    /// Close the dialog and install this filter on the active tab.
    ApplyFilter(Filter),
    /// Close the dialog and rename the active tab to this string.
    ApplyRename(String),
}

impl Dialog {
    fn filter(current: &Filter) -> Self {
        let editor = LineEditor::new(current.to_string());
        let mut d = Self::Filter { editor, parse_error: None };
        d.reparse_filter();
        d
    }

    fn rename(current_name: &str) -> Self {
        Self::Rename { editor: LineEditor::new(current_name.to_string()) }
    }

    fn editor(&self) -> &LineEditor {
        match self {
            Self::Filter { editor, .. } | Self::Rename { editor } => editor,
        }
    }

    fn parse_error(&self) -> Option<&str> {
        match self {
            Self::Filter { parse_error, .. } => parse_error.as_deref(),
            Self::Rename { .. } => None,
        }
    }

    fn title(&self) -> &'static str {
        match self {
            Self::Filter { .. } => "Filter (Esc cancel · Enter apply)",
            Self::Rename { .. } => {
                "Rename tab (Esc cancel · Enter apply)"
            }
        }
    }

    fn reparse_filter(&mut self) {
        if let Self::Filter { editor, parse_error } = self {
            *parse_error = match editor.text.parse::<Filter>() {
                Ok(_) => None,
                Err(e) => Some(e.to_string()),
            };
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> DialogResult {
        // Esc/Enter come first so they aren't shadowed by editor
        // bindings.  Anything not handled by either path is dropped.
        match key {
            KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                ..
            } => return DialogResult::Cancel,
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => return self.try_apply(),
            _ => {}
        }
        let editor_result = match self {
            Self::Filter { editor, .. } | Self::Rename { editor } => {
                editor.handle_edit(key)
            }
        };
        if let EditAction::Handled = editor_result {
            self.reparse_filter();
        }
        DialogResult::Stay
    }

    fn try_apply(&mut self) -> DialogResult {
        match self {
            Self::Filter { editor, parse_error } => {
                match editor.text.parse::<Filter>() {
                    Ok(f) => DialogResult::ApplyFilter(f),
                    Err(e) => {
                        *parse_error = Some(e.to_string());
                        DialogResult::Stay
                    }
                }
            }
            Self::Rename { editor } => {
                DialogResult::ApplyRename(editor.text.clone())
            }
        }
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
/// non-whitespace as part of the word, so a whole `name=Nexus` token
/// is killed in one shot.
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
    let [tabs_area, content_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    render_tab_bar(frame, app, tabs_area);

    app.viewport_height = content_area.height;
    // Re-clamp in case the viewport just shrank past the previous top.
    let max_top = app.active_tab().max_top(app.viewport_height);
    if app.active_tab().viewport_top > max_top {
        app.active_tab_mut().viewport_top = max_top;
    }

    let tab = app.active_tab();
    let total = tab.rows.len();
    let top = tab.viewport_top;
    let bottom = (top + content_area.height as usize).min(total);

    let lines: Vec<Line<'_>> = tab.rows[top..bottom]
        .iter()
        .map(|s| Line::raw(s.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(lines), content_area);

    let footer = if total == 0 {
        "q quit · f filter · ^T new · ^W close · r rename · 0/0"
            .to_string()
    } else {
        format!(
            "q quit · f filter · ^T new · ^W close · r rename · \
             {}-{} of {}",
            top + 1,
            bottom,
            total,
        )
    };
    frame.render_widget(Paragraph::new(footer), footer_area);

    if let Some(dialog) = app.dialog.as_ref() {
        render_dialog(frame, dialog, area);
    }
}

fn render_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line<'_>> =
        app.tabs.iter().map(|t| Line::raw(t.name.as_str())).collect();
    let widget = Tabs::new(titles).select(app.active).highlight_style(
        Style::default().add_modifier(Modifier::REVERSED),
    );
    frame.render_widget(widget, area);
}

/// Carves a centered popup over `area` and draws either dialog variant.
///
/// The Filter variant additionally renders any parse error in red below
/// the edit row; the Rename variant simply leaves that row blank.
fn render_dialog(frame: &mut Frame, dialog: &Dialog, area: Rect) {
    let popup = popup_area(area, 70, 5);
    // Clear the underlying rows so the editor isn't drawn on top of
    // them.
    frame.render_widget(Clear, popup);

    let block = Block::bordered().title(dialog.title());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [edit_area, error_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    let editor = dialog.editor();
    frame.render_widget(
        Paragraph::new(Line::raw(editor.text.as_str())),
        edit_area,
    );

    // Cursor column: the dialog buffers are ASCII in practice (filter
    // syntax is ASCII, tab names typically too), so the byte offset
    // doubles as the column.  If we ever accept multibyte chars we'd
    // need to compute the display width here instead.
    let col = edit_area
        .x
        .saturating_add(u16::try_from(editor.cursor).unwrap_or(u16::MAX));
    let col = col.min(edit_area.x.saturating_add(edit_area.width));
    frame.set_cursor_position(Position::new(col, edit_area.y));

    if let Some(err) = dialog.parse_error()
        && error_area.height > 0
    {
        frame.render_widget(
            Paragraph::new(Line::raw(err))
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

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn back_tab() -> KeyEvent {
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)
    }

    fn app(rows: usize, height: u16) -> App {
        let rows = (0..rows).map(|i| format!("row {i}")).collect();
        let mut a = App::with_rows(rows);
        a.viewport_height = height;
        a
    }

    /// Drives the open dialog through a sequence of typed characters.
    /// Panics if any keystroke unexpectedly closes the dialog.
    fn type_into(d: &mut Dialog, s: &str) {
        for c in s.chars() {
            match d.handle_key(key(KeyCode::Char(c))) {
                DialogResult::Stay => {}
                _ => panic!("typing {c:?} unexpectedly closed dialog"),
            }
        }
    }

    // ---------- top-level (no dialog) ----------

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

    // ---------- scrolling ----------

    #[test]
    fn j_and_down_scroll_down_one() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().viewport_top, 1);
        a.handle_key(key(KeyCode::Down));
        assert_eq!(a.active_tab().viewport_top, 2);
    }

    #[test]
    fn k_and_up_scroll_up_one() {
        let mut a = app(10, 5);
        a.active_tab_mut().viewport_top = 3;
        a.handle_key(key(KeyCode::Char('k')));
        assert_eq!(a.active_tab().viewport_top, 2);
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.active_tab().viewport_top, 1);
    }

    #[test]
    fn ctrl_d_scrolls_half_page_down() {
        let mut a = app(100, 10);
        a.handle_key(ctrl('d'));
        assert_eq!(a.active_tab().viewport_top, 5);
    }

    #[test]
    fn space_scrolls_full_page_down() {
        let mut a = app(100, 10);
        a.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(a.active_tab().viewport_top, 10);
    }

    #[test]
    fn ctrl_u_scrolls_half_page_up() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = 20;
        a.handle_key(ctrl('u'));
        assert_eq!(a.active_tab().viewport_top, 15);
    }

    #[test]
    fn g_jumps_top() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = 50;
        a.handle_key(key(KeyCode::Char('g')));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn home_jumps_top() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = 50;
        a.handle_key(key(KeyCode::Home));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn shift_g_jumps_bottom() {
        let mut a = app(100, 10);
        a.handle_key(shift('G'));
        assert_eq!(a.active_tab().viewport_top, 90);
    }

    #[test]
    fn end_jumps_bottom() {
        let mut a = app(100, 10);
        a.handle_key(key(KeyCode::End));
        assert_eq!(a.active_tab().viewport_top, 90);
    }

    #[test]
    fn cant_scroll_above_top() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('k')));
        assert_eq!(a.active_tab().viewport_top, 0);
        a.handle_key(ctrl('u'));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn cant_scroll_below_bottom() {
        let mut a = app(10, 5);
        a.active_tab_mut().viewport_top = 5; // == max_top
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().viewport_top, 5);
        a.handle_key(ctrl('d'));
        assert_eq!(a.active_tab().viewport_top, 5);
    }

    #[test]
    fn small_content_clamps_to_zero() {
        let mut a = app(3, 10);
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().viewport_top, 0);
        a.handle_key(shift('G'));
        assert_eq!(a.active_tab().viewport_top, 0);
        a.handle_key(key(KeyCode::End));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn release_events_are_ignored() {
        let mut a = app(10, 5);
        let mut k = key(KeyCode::Char('q'));
        k.kind = KeyEventKind::Release;
        a.handle_key(k);
        assert!(!a.quit);
    }

    // ---------- top-level rendering ----------

    #[test]
    fn render_paints_rows_and_footer() {
        let backend = TestBackend::new(80, 6);
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

    #[test]
    fn f_opens_filter_dialog() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        assert!(matches!(a.dialog, Some(Dialog::Filter { .. })));
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
        assert_eq!(a.active_tab().viewport_top, 0);
        assert_eq!(a.dialog.as_ref().unwrap().editor().text, "qj");
    }

    #[test]
    fn dialog_prepopulates_with_current_filter() {
        let f: Filter = "level>=warn name=Nexus".parse().unwrap();
        let d = Dialog::filter(&f);
        assert_eq!(d.editor().text, "level>=warn name=Nexus");
        // Cursor is at the end so the user can extend the filter
        // without homing first.
        assert_eq!(d.editor().cursor, d.editor().text.len());
        assert!(d.parse_error().is_none());
    }

    #[test]
    fn dialog_typing_inserts_at_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "name=Nexus");
        assert_eq!(d.editor().text, "name=Nexus");
        assert_eq!(d.editor().cursor, "name=Nexus".len());
    }

    #[test]
    fn dialog_backspace_deletes_char_before_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Backspace));
        assert_eq!(d.editor().text, "ab");
        assert_eq!(d.editor().cursor, 2);
    }

    #[test]
    fn dialog_left_right_move_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Left));
        assert_eq!(d.editor().cursor, 2);
        d.handle_key(key(KeyCode::Home));
        assert_eq!(d.editor().cursor, 0);
        d.handle_key(key(KeyCode::Right));
        assert_eq!(d.editor().cursor, 1);
        d.handle_key(key(KeyCode::End));
        assert_eq!(d.editor().cursor, 3);
    }

    #[test]
    fn dialog_delete_removes_char_after_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(key(KeyCode::Delete));
        assert_eq!(d.editor().text, "bc");
        assert_eq!(d.editor().cursor, 0);
    }

    #[test]
    fn dialog_ctrl_u_kills_to_start_of_line() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        // Position cursor inside "Nexus".
        for _ in 0..3 {
            d.handle_key(key(KeyCode::Left));
        }
        let cursor_before = d.editor().cursor;
        d.handle_key(ctrl('u'));
        assert_eq!(d.editor().text, "xus");
        assert_eq!(d.editor().cursor, 0);
        assert!(cursor_before > 0);
    }

    #[test]
    fn dialog_ctrl_u_at_start_is_noop() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(ctrl('u'));
        assert_eq!(d.editor().text, "abc");
        assert_eq!(d.editor().cursor, 0);
    }

    #[test]
    fn dialog_ctrl_w_kills_previous_whitespace_word() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        d.handle_key(ctrl('w'));
        // The whole `name=Nexus` token disappears, plus the space.
        assert_eq!(d.editor().text, "level>=warn ");
        assert_eq!(d.editor().cursor, "level>=warn ".len());
    }

    #[test]
    fn dialog_ctrl_w_consumes_trailing_whitespace_first() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "name=Nexus   ");
        d.handle_key(ctrl('w'));
        assert_eq!(d.editor().text, "");
        assert_eq!(d.editor().cursor, 0);
    }

    #[test]
    fn dialog_alt_b_moves_back_one_alphanumeric_word() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        d.handle_key(alt('b'));
        assert_eq!(&d.editor().text[d.editor().cursor..], "Nexus");
        d.handle_key(alt('b'));
        assert_eq!(&d.editor().text[d.editor().cursor..], "name=Nexus");
        d.handle_key(alt('b'));
        assert_eq!(
            &d.editor().text[d.editor().cursor..],
            "warn name=Nexus",
        );
        d.handle_key(alt('b'));
        assert_eq!(d.editor().cursor, 0);
        // Once more: clamped at zero.
        d.handle_key(alt('b'));
        assert_eq!(d.editor().cursor, 0);
    }

    #[test]
    fn dialog_alt_f_moves_forward_one_alphanumeric_word() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(alt('f'));
        assert_eq!(&d.editor().text[..d.editor().cursor], "level");
        d.handle_key(alt('f'));
        assert_eq!(&d.editor().text[..d.editor().cursor], "level>=warn");
        d.handle_key(alt('f'));
        assert_eq!(
            &d.editor().text[..d.editor().cursor],
            "level>=warn name",
        );
        d.handle_key(alt('f'));
        assert_eq!(d.editor().cursor, d.editor().text.len());
        // Once more: clamped.
        d.handle_key(alt('f'));
        assert_eq!(d.editor().cursor, d.editor().text.len());
    }

    #[test]
    fn dialog_shows_parse_error_live() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "bogus");
        assert!(d.parse_error().is_some());
        let len = d.editor().text.len();
        for _ in 0..len {
            d.handle_key(key(KeyCode::Backspace));
        }
        type_into(&mut d, "level>=warn");
        assert!(d.parse_error().is_none());
    }

    #[test]
    fn dialog_enter_with_invalid_filter_keeps_dialog_open() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "bogus");
        a.handle_key(key(KeyCode::Enter));
        let d = a.dialog.as_ref().expect("dialog should still be open");
        assert!(d.parse_error().is_some());
    }

    #[test]
    fn dialog_enter_with_valid_filter_applies_and_closes() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().filter.to_string(), "level>=warn");
    }

    #[test]
    fn dialog_escape_discards_changes() {
        let mut a = app(10, 5);
        let original_filter = a.active_tab().filter.to_string();
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "name=Nexus");
        a.handle_key(key(KeyCode::Esc));
        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().filter.to_string(), original_filter);
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
        a.active_tab_mut().viewport_top = 3;
        assert_eq!(a.active_tab().rows.len(), 6);

        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));

        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().rows.len(), 1);
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn render_draws_dialog_with_error() {
        let backend = TestBackend::new(80, 9);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "bogus");
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("Filter"), "dump:\n{dump}");
        assert!(dump.contains("bogus"), "dump:\n{dump}");
        assert!(
            dump.contains("operator") || dump.contains("token"),
            "expected a parse error in dump:\n{dump}",
        );
    }

    // ---------- tabs ----------

    #[test]
    fn fresh_app_has_one_tab_named_tab_one() {
        let a = app(0, 5);
        assert_eq!(a.tabs.len(), 1);
        assert_eq!(a.active, 0);
        assert_eq!(a.active_tab().name, "Tab 1");
    }

    #[test]
    fn ctrl_t_creates_new_tab_with_cloned_filter_and_opens_dialog() {
        let mut a = app(10, 5);
        // Set a filter on the first tab.
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.tabs.len(), 1);

        a.handle_key(ctrl('t'));
        assert_eq!(a.tabs.len(), 2);
        assert_eq!(a.active, 1);
        assert_eq!(a.active_tab().filter.to_string(), "level>=warn");
        // Filter dialog is open with the cloned filter prefilled.
        let d = a.dialog.as_ref().expect("dialog should be open");
        assert!(matches!(d, Dialog::Filter { .. }));
        assert_eq!(d.editor().text, "level>=warn");
    }

    #[test]
    fn ctrl_t_uses_monotonic_tab_numbering() {
        let mut a = app(10, 5);
        assert_eq!(a.active_tab().name, "Tab 1");
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.active_tab().name, "Tab 2");
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.active_tab().name, "Tab 3");
    }

    #[test]
    fn esc_on_new_tab_dialog_keeps_tab_with_cloned_filter() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));

        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.tabs.len(), 2);
        assert_eq!(a.active, 1);
        assert_eq!(a.active_tab().filter.to_string(), "level>=warn");
    }

    #[test]
    fn tab_cycles_to_next_with_wrap() {
        let mut a = app(10, 5);
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.tabs.len(), 3);
        assert_eq!(a.active, 2);
        a.handle_key(key(KeyCode::Tab));
        assert_eq!(a.active, 0);
        a.handle_key(key(KeyCode::Tab));
        assert_eq!(a.active, 1);
    }

    #[test]
    fn shift_tab_cycles_to_previous_with_wrap() {
        let mut a = app(10, 5);
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.tabs.len(), 2);
        assert_eq!(a.active, 1);
        a.handle_key(back_tab());
        assert_eq!(a.active, 0);
        // Wrap.
        a.handle_key(back_tab());
        assert_eq!(a.active, 1);
    }

    #[test]
    fn ctrl_w_closes_active_tab() {
        let mut a = app(10, 5);
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.tabs.len(), 2);
        let surviving_name = a.tabs[0].name.clone();
        a.handle_key(ctrl('w'));
        assert_eq!(a.tabs.len(), 1);
        assert_eq!(a.tabs[0].name, surviving_name);
    }

    #[test]
    fn ctrl_w_on_last_tab_creates_fresh_one() {
        let mut a = app(10, 5);
        assert_eq!(a.tabs.len(), 1);
        let original_name = a.active_tab().name.clone();
        a.handle_key(ctrl('w'));
        assert_eq!(a.tabs.len(), 1);
        assert_eq!(a.active, 0);
        // It's a new tab — different name (next number).
        assert_ne!(a.active_tab().name, original_name);
        assert!(a.active_tab().filter.predicates().is_empty());
    }

    #[test]
    fn each_tab_keeps_its_own_viewport_top() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = 30;
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.active_tab().viewport_top, 0);
        a.active_tab_mut().viewport_top = 50;
        a.handle_key(back_tab());
        assert_eq!(a.active_tab().viewport_top, 30);
        a.handle_key(key(KeyCode::Tab));
        assert_eq!(a.active_tab().viewport_top, 50);
    }

    #[test]
    fn ctrl_w_inside_filter_dialog_kills_word_not_close_tab() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn name=Nexus");
        a.handle_key(ctrl('w'));
        // Dialog still open, buffer lost its last word.
        assert!(a.dialog.is_some());
        assert_eq!(a.dialog.as_ref().unwrap().editor().text, "level>=warn ");
        // Tab count unchanged.
        assert_eq!(a.tabs.len(), 1);
    }

    // ---------- rename dialog ----------

    #[test]
    fn r_opens_rename_dialog_with_current_name() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('r')));
        let d = a.dialog.as_ref().expect("dialog should be open");
        assert!(matches!(d, Dialog::Rename { .. }));
        assert_eq!(d.editor().text, "Tab 1");
    }

    #[test]
    fn rename_dialog_enter_applies_new_name() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('r')));
        // Clear and type a new name.
        a.handle_key(ctrl('u'));
        type_into(a.dialog.as_mut().unwrap(), "Nexus");
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().name, "Nexus");
    }

    #[test]
    fn rename_dialog_escape_discards() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('r')));
        type_into(a.dialog.as_mut().unwrap(), "garbage");
        a.handle_key(key(KeyCode::Esc));
        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().name, "Tab 1");
    }

    #[test]
    fn rename_dialog_has_no_parse_error() {
        // Anything (including invalid filter syntax) is acceptable for
        // a tab name — the rename dialog should never report a parse
        // error.
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('r')));
        type_into(a.dialog.as_mut().unwrap(), " not a filter ");
        assert!(a.dialog.as_ref().unwrap().parse_error().is_none());
    }

    // ---------- tab-bar rendering ----------

    #[test]
    fn render_paints_tab_bar() {
        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("Tab 1"), "dump:\n{dump}");
        assert!(dump.contains("Tab 2"), "dump:\n{dump}");
    }
}
