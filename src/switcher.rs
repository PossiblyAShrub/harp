use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::Buffer,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, Paragraph, Widget},
};

use crate::protocol::ScopeInfo;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwitcherAction {
    None,
    Cancel,
    Attach(u64),
    CreateNew,
}

pub struct ScopeSwitcher {
    scopes: Vec<ScopeInfo>,
    selected: usize,
    allow_cancel: bool,
}

impl ScopeSwitcher {
    pub fn new(scopes: Vec<ScopeInfo>, selected: usize, allow_cancel: bool) -> Self {
        Self {
            scopes,
            selected,
            allow_cancel,
        }
    }

    fn total_items(&self) -> usize {
        self.scopes.len() + 1
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> SwitcherAction {
        if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return SwitcherAction::None;
        }

        match key_event.code {
            KeyCode::Esc => return SwitcherAction::Cancel,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.total_items() {
                    self.selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(scope) = self.scopes.get(self.selected) {
                    return SwitcherAction::Attach(scope.id);
                }
                return SwitcherAction::CreateNew;
            }
            _ => {}
        }

        SwitcherAction::None
    }
}

impl Widget for &ScopeSwitcher {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let list_rows = self.total_items().max(1) as u16 + 2;
        let desired_height = list_rows + 3;
        let popup_width = area.width.saturating_sub(4).clamp(28, 72);
        let popup_height = area.height.saturating_sub(2).min(desired_height).max(7);
        let x = area.x + area.width.saturating_sub(popup_width) / 2;
        let y = area.y + area.height.saturating_sub(popup_height) / 2;
        let popup = Rect::new(
            x,
            y,
            popup_width.min(area.width),
            popup_height.min(area.height),
        );

        Clear.render(popup, buf);

        let title = if self.allow_cancel {
            " Switch Scope "
        } else {
            " Resume or Create Scope "
        };
        let outer = Block::bordered().title(title);
        let inner = outer.inner(popup);
        outer.render(popup, buf);

        let chunks = Layout::new(
            Direction::Vertical,
            [Constraint::Min(3), Constraint::Length(1)],
        )
        .split(inner);

        let mut items: Vec<ListItem> = self
            .scopes
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let prefix = if i == self.selected { "> " } else { "  " };
                let style = if i == self.selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let label = format!(
                    "{prefix}#{} {} ({} panes) [{}]",
                    s.id,
                    s.name,
                    s.pane_count,
                    s.cwd.display()
                );
                ListItem::new(Line::from(Span::styled(label, style)))
            })
            .collect();

        let new_idx = self.scopes.len();
        let new_prefix = if new_idx == self.selected { "> " } else { "  " };
        let new_style = if new_idx == self.selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Green)
        };
        items.push(ListItem::new(Line::from(Span::styled(
            format!("{new_prefix}+ New Scope"),
            new_style,
        ))));

        List::new(items).render(chunks[0], buf);

        let hint = if self.allow_cancel {
            "Up/Down: select | Enter: switch | Esc: cancel"
        } else {
            "Up/Down: select | Enter: open | Esc: quit"
        };
        Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray)))
            .render(chunks[1], buf);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenameAction {
    None,
    Cancel,
    Confirm(String),
}

pub struct RenameModal {
    name: String,
    cursor: usize,
}

impl RenameModal {
    pub fn new(current_name: impl Into<String>) -> Self {
        let name = current_name.into();
        let cursor = name.chars().count();
        Self { name, cursor }
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> RenameAction {
        if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return RenameAction::None;
        }

        match key_event.code {
            KeyCode::Esc => return RenameAction::Cancel,
            KeyCode::Enter => {
                let trimmed = self.name.trim();
                if trimmed.is_empty() {
                    return RenameAction::Cancel;
                }
                return RenameAction::Confirm(trimmed.to_string());
            }
            KeyCode::Char(c) if !key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                let idx = char_to_byte_index(&self.name, self.cursor);
                self.name.insert(idx, c);
                self.cursor += 1;
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let idx = char_to_byte_index(&self.name, self.cursor);
                    self.name.remove(idx);
                }
            }
            KeyCode::Delete => {
                let len = self.name.chars().count();
                if self.cursor < len {
                    let idx = char_to_byte_index(&self.name, self.cursor);
                    self.name.remove(idx);
                }
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let len = self.name.chars().count();
                if self.cursor < len {
                    self.cursor += 1;
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.name.chars().count(),
            _ => {}
        }

        RenameAction::None
    }
}

impl Widget for &RenameModal {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let popup_width = area.width.saturating_sub(4).clamp(24, 52);
        let popup_height = 6u16.min(area.height.saturating_sub(2)).max(5);
        let x = area.x + area.width.saturating_sub(popup_width) / 2;
        let y = area.y + area.height.saturating_sub(popup_height) / 2;
        let popup = Rect::new(
            x,
            y,
            popup_width.min(area.width),
            popup_height.min(area.height),
        );

        Clear.render(popup, buf);

        let outer = Block::bordered().title(" Rename Scope ");
        let inner = outer.inner(popup);
        outer.render(popup, buf);

        let chunks = Layout::new(
            Direction::Vertical,
            [Constraint::Length(3), Constraint::Length(1)],
        )
        .split(inner);

        let input_block = Block::bordered().border_style(Style::default().fg(Color::Yellow));
        let input_inner = input_block.inner(chunks[0]);
        input_block.render(chunks[0], buf);

        let byte_idx = char_to_byte_index(&self.name, self.cursor);
        let before = &self.name[..byte_idx];
        let (cursor_ch, after) = if byte_idx < self.name.len() {
            let ch = self.name[byte_idx..].chars().next().unwrap();
            (ch.to_string(), &self.name[byte_idx + ch.len_utf8()..])
        } else {
            (" ".to_string(), "")
        };
        Paragraph::new(Line::from(vec![
            Span::raw(before),
            Span::styled(
                cursor_ch,
                Style::default().fg(Color::Black).bg(Color::White),
            ),
            Span::raw(after),
        ]))
        .render(input_inner, buf);

        Paragraph::new(Span::styled(
            "Enter: save | Esc: cancel",
            Style::default().fg(Color::DarkGray),
        ))
        .render(chunks[1], buf);
    }
}

fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map_or(s.len(), |(idx, _)| idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn switcher_selects_existing_or_new_scope() {
        let scopes = vec![ScopeInfo {
            id: 7,
            name: "harp".to_string(),
            cwd: PathBuf::from("/tmp/harp"),
            pane_count: 2,
        }];
        let mut switcher = ScopeSwitcher::new(scopes, 0, true);
        assert_eq!(
            switcher.handle_key(key(KeyCode::Enter)),
            SwitcherAction::Attach(7)
        );
        switcher.handle_key(key(KeyCode::Down));
        assert_eq!(
            switcher.handle_key(key(KeyCode::Enter)),
            SwitcherAction::CreateNew
        );
        assert_eq!(
            switcher.handle_key(key(KeyCode::Esc)),
            SwitcherAction::Cancel
        );
    }

    #[test]
    fn rename_modal_edits_and_confirms() {
        let mut modal = RenameModal::new("old");
        modal.handle_key(key(KeyCode::Backspace));
        modal.handle_key(key(KeyCode::Backspace));
        modal.handle_key(key(KeyCode::Backspace));
        for ch in "backend".chars() {
            modal.handle_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            RenameAction::Confirm("backend".to_string())
        );
        assert_eq!(modal.handle_key(key(KeyCode::Esc)), RenameAction::Cancel);
    }
}
