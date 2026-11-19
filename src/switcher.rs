use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
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
    pub fn new(scopes: Vec<ScopeInfo>, allow_cancel: bool) -> Self {
        Self {
            scopes,
            selected: 0,
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
        let popup_width = area.width.saturating_sub(4).min(72).max(28);
        let popup_height = area.height.saturating_sub(2).min(desired_height).max(7);
        let x = area.x + area.width.saturating_sub(popup_width) / 2;
        let y = area.y + area.height.saturating_sub(popup_height) / 2;
        let popup = Rect::new(x, y, popup_width.min(area.width), popup_height.min(area.height));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
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
        let mut switcher = ScopeSwitcher::new(scopes, true);
        assert_eq!(
            switcher.handle_key(key(KeyCode::Enter)),
            SwitcherAction::Attach(7)
        );
        switcher.handle_key(key(KeyCode::Down));
        assert_eq!(
            switcher.handle_key(key(KeyCode::Enter)),
            SwitcherAction::CreateNew
        );
        assert_eq!(switcher.handle_key(key(KeyCode::Esc)), SwitcherAction::Cancel);
    }
}
