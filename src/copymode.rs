//! Freeze-frame "copy mode" for selecting text inside a projection.
//!
//! Entering copy mode snapshots the focused pane into a private parser so the
//! text under the cursor stays put even while live output keeps streaming
//! behind the overlay. The selection is a tmux-style *stream* selection: Space
//! anchors it and moving the cursor extends it.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::Rect,
    prelude::Buffer,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Widget},
};
use tui_term::vt100;

/// Result of feeding a key press to a [`CopyMode`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CopyAction {
    /// Nothing to do; the copy mode stays active.
    None,
    /// The user asked to copy the selection (or whole screen) and exit.
    CopyAndExit,
    /// The user asked to leave copy mode without copying.
    Exit,
}

/// Freeze-frame copy mode over a single projection.
pub struct CopyMode {
    /// Identifier of the projection this mode was entered over.
    pub pane_id: usize,
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
    cursor_col: u16,
    cursor_row: u16,
    /// Start of the selection; `None` until the user anchors it with Space.
    anchor: Option<(u16, u16)>,
}

impl CopyMode {
    /// Freezes `parser`'s current screen into a fresh copy-mode state.
    pub fn new(pane_id: usize, parser: &vt100::Parser) -> Option<Self> {
        let (rows, cols) = parser.screen().size();
        if rows == 0 || cols == 0 {
            return None;
        }
        let mut frozen = vt100::Parser::new(rows, cols, 0);
        frozen.process(&parser.screen().state_formatted());
        Some(Self {
            pane_id,
            parser: frozen,
            cols,
            rows,
            cursor_col: 0,
            cursor_row: 0,
            anchor: None,
        })
    }

    /// Handles a single key event, updating movement / the selection.
    ///
    /// Arrow keys may repeat; confirm/cancel keys only act on the first press
    /// so a held key can't spam copies or fall through to the shell.
    pub fn handle_key(&mut self, key: KeyEvent) -> CopyAction {
        if key.kind == KeyEventKind::Release {
            return CopyAction::None;
        }

        let ctrl_shift = key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            KeyCode::Char(' ') if key.modifiers == KeyModifiers::NONE => {
                self.anchor = Some((self.cursor_col, self.cursor_row));
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => return CopyAction::CopyAndExit,
            KeyCode::Char('c') | KeyCode::Char('C')
                if ctrl_shift && key.kind == KeyEventKind::Press =>
            {
                return CopyAction::CopyAndExit;
            }
            KeyCode::Char('q') | KeyCode::Esc
                if key.kind == KeyEventKind::Press && key.modifiers == KeyModifiers::NONE =>
            {
                return CopyAction::Exit;
            }
            KeyCode::Up => self.move_cursor(0, -1),
            KeyCode::Down => self.move_cursor(0, 1),
            KeyCode::Left => self.move_cursor(-1, 0),
            KeyCode::Right => self.move_cursor(1, 0),
            KeyCode::Home => self.cursor_col = 0,
            KeyCode::End => self.cursor_col = self.cols.saturating_sub(1),
            KeyCode::PageUp => self.cursor_row = 0,
            KeyCode::PageDown => self.cursor_row = self.rows.saturating_sub(1),
            _ => {}
        }
        CopyAction::None
    }

    fn move_cursor(&mut self, dx: i32, dy: i32) {
        let max_col = i32::from(self.cols.saturating_sub(1));
        let max_row = i32::from(self.rows.saturating_sub(1));
        self.cursor_col = (i32::from(self.cursor_col) + dx).clamp(0, max_col) as u16;
        self.cursor_row = (i32::from(self.cursor_row) + dy).clamp(0, max_row) as u16;
    }

    /// The text to copy: the selection when one exists, otherwise the whole
    /// visible screen.
    pub fn copy_text(&self) -> String {
        match self.anchor {
            Some(anchor) => self.region_text(anchor, (self.cursor_col, self.cursor_row)),
            None => self.visible_text(),
        }
    }

    fn visible_text(&self) -> String {
        let mut lines = Vec::new();
        for row in 0..self.rows {
            lines.push(self.row_text(row, 0, self.cols.saturating_sub(1)));
        }
        join_trimmed(lines)
    }

    fn region_text(&self, a: (u16, u16), b: (u16, u16)) -> String {
        let (start_row, start_col, end_row, end_col) = if (b.1, b.0) >= (a.1, a.0) {
            (a.1, a.0, b.1, b.0)
        } else {
            (b.1, b.0, a.1, a.0)
        };
        let mut lines = Vec::new();
        for row in start_row..=end_row {
            let first_col = if row == start_row { start_col } else { 0 };
            let last_col = if row == end_row {
                end_col
            } else {
                self.cols.saturating_sub(1)
            };
            lines.push(self.row_text(row, first_col, last_col));
        }
        join_trimmed(lines)
    }

    fn row_text(&self, row: u16, start_col: u16, end_col: u16) -> String {
        let mut text = String::new();
        for col in start_col..=end_col {
            if let Some(cell) = self.parser.screen().cell(row, col)
                && !cell.is_wide_continuation()
            {
                text.push_str(cell.contents());
            }
        }
        text
    }

    fn in_selection(&self, col: u16, row: u16) -> bool {
        let Some(anchor) = self.anchor else {
            return false;
        };
        let (start_row, start_col, end_row, end_col) =
            if (self.cursor_row, self.cursor_col) >= (anchor.1, anchor.0) {
                (anchor.1, anchor.0, self.cursor_row, self.cursor_col)
            } else {
                (self.cursor_row, self.cursor_col, anchor.1, anchor.0)
            };
        if row < start_row || row > end_row {
            return false;
        }
        if start_row == end_row {
            return col >= start_col && col <= end_col;
        }
        if row == start_row {
            col >= start_col
        } else if row == end_row {
            col <= end_col
        } else {
            true
        }
    }
}

fn join_trimmed(mut lines: Vec<String>) -> String {
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn vt_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn cell_style(cell: &vt100::Cell, selected: bool, is_cursor: bool) -> Style {
    let mut fg = cell.fgcolor();
    let mut bg = cell.bgcolor();
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    let mut style = Style::default().fg(vt_color(fg)).bg(if selected {
        Color::Indexed(27)
    } else {
        vt_color(bg)
    });
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.dim() {
        style = style.add_modifier(Modifier::DIM);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if is_cursor {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

impl Widget for &CopyMode {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered()
            .title(Span::styled(
                " COPY ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Line::from(Span::styled(
                " Space: select | Arrows: move | Home/End: line | PgUp/PgDn: edge | Enter or Ctrl+Shift+C: copy | q or Esc: quit ",
                Style::default().fg(Color::Gray),
            )))
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let rows = self.rows.min(inner.height);
        let cols = self.cols.min(inner.width);

        for row in 0..rows {
            let mut spans = Vec::with_capacity(usize::from(cols));
            let mut col: u16 = 0;
            while col < cols {
                let Some(cell) = self.parser.screen().cell(row, col) else {
                    col = col.saturating_add(1);
                    continue;
                };
                let width = if cell.is_wide() { 2 } else { 1 };
                let is_cursor = col == self.cursor_col && row == self.cursor_row;
                let selected = self.in_selection(col, row);
                let text = if cell.has_contents() {
                    cell.contents().to_string()
                } else {
                    " ".to_string()
                };
                spans.push(Span::styled(text, cell_style(cell, selected, is_cursor)));
                col = col.saturating_add(width);
            }
            let y = inner.y + row;
            buf.set_line(inner.x, y, &Line::from(spans), inner.width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(text: &str, rows: u16, cols: u16) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(text.as_bytes());
        parser
    }

    #[test]
    fn copies_visible_screen_without_selection() {
        let parser = screen("abc\r\ndef\r\nghi\r\n\r\n", 5, 8);
        let cm = CopyMode::new(0, &parser).unwrap();
        assert_eq!(cm.copy_text(), "abc\ndef\nghi");
    }

    #[test]
    fn copies_selected_stream_from_anchor_to_cursor() {
        let parser = screen("abc\r\ndef\r\nghi", 3, 8);
        let mut cm = CopyMode::new(0, &parser).unwrap();
        cm.anchor = Some((0, 0));
        cm.cursor_col = 2;
        cm.cursor_row = 1;
        assert_eq!(cm.copy_text(), "abc\ndef");
    }

    #[test]
    fn selection_works_upside_down() {
        let parser = screen("abc\r\ndef\r\nghi", 3, 8);
        let mut cm = CopyMode::new(0, &parser).unwrap();
        cm.anchor = Some((1, 2));
        cm.cursor_col = 0;
        cm.cursor_row = 0;
        assert_eq!(cm.copy_text(), "abc\ndef\ngh");
    }

    #[test]
    fn selects_within_a_single_row() {
        let parser = screen("abcdef", 1, 8);
        let mut cm = CopyMode::new(0, &parser).unwrap();
        cm.anchor = Some((1, 0));
        cm.cursor_col = 4;
        cm.cursor_row = 0;
        assert_eq!(cm.copy_text(), "bcde");
    }

    #[test]
    fn space_anchors_and_ctrl_shift_c_copies() {
        let parser = screen("hello world", 1, 16);
        let mut cm = CopyMode::new(0, &parser).unwrap();
        assert_eq!(
            cm.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            CopyAction::None
        );
        cm.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        cm.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(
            cm.handle_key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            CopyAction::CopyAndExit
        );
        assert_eq!(cm.copy_text(), "hel");
    }

    #[test]
    fn q_exits_without_copying() {
        let parser = screen("x", 1, 2);
        let mut cm = CopyMode::new(0, &parser).unwrap();
        assert_eq!(
            cm.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            CopyAction::Exit
        );
    }

    #[test]
    fn ignores_release_events() {
        let parser = screen("x", 1, 2);
        let mut cm = CopyMode::new(0, &parser).unwrap();
        assert_eq!(
            cm.handle_key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Release
            )),
            CopyAction::None
        );
    }
}
