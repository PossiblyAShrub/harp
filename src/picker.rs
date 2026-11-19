use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::Buffer,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, Paragraph, Widget},
};

use crate::{
    config::{ProgramOption, default_harp_config_dir, load_programs},
    scopes::{NavDirection, ScopeSpec},
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PickerField {
    Program,
    Cwd,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerAction {
    None,
    Cancel,
    Confirm {
        direction: NavDirection,
        spec: ScopeSpec,
    },
}

pub struct ScopePicker {
    direction: NavDirection,
    field: PickerField,
    programs: Vec<ProgramOption>,
    selected_program: usize,
    cwd: String,
    cursor: usize,
}

impl ScopePicker {
    pub fn new(direction: NavDirection, default_cwd: PathBuf) -> Self {
        let programs = load_programs(&default_cwd, default_harp_config_dir().as_deref());
        Self::with_programs(direction, default_cwd, programs)
    }

    pub fn with_programs(
        direction: NavDirection,
        default_cwd: PathBuf,
        programs: Vec<ProgramOption>,
    ) -> Self {
        let cwd = default_cwd.to_string_lossy().into_owned();
        let cursor = cwd.chars().count();
        Self {
            direction,
            field: PickerField::Program,
            programs,
            selected_program: 0,
            cwd,
            cursor,
        }
    }

    pub fn spec(&self) -> ScopeSpec {
        let program = self
            .programs
            .get(self.selected_program)
            .map(|p| p.program.clone())
            .unwrap_or_else(|| "zsh".to_string());
        let cwd = if self.cwd.trim().is_empty() {
            PathBuf::from("/")
        } else {
            PathBuf::from(self.cwd.trim())
        };

        ScopeSpec {
            machine: "localhost".to_string(),
            cwd,
            program,
        }
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> PickerAction {
        if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return PickerAction::None;
        }

        match key_event.code {
            KeyCode::Esc => return PickerAction::Cancel,
            KeyCode::Enter => {
                return PickerAction::Confirm {
                    direction: self.direction,
                    spec: self.spec(),
                };
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.field = match self.field {
                    PickerField::Program => PickerField::Cwd,
                    PickerField::Cwd => PickerField::Program,
                };
                return PickerAction::None;
            }
            _ => {}
        }

        match self.field {
            PickerField::Program => match key_event.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected_program = self.selected_program.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if self.selected_program + 1 < self.programs.len() {
                        self.selected_program += 1;
                    }
                }
                _ => {}
            },
            PickerField::Cwd => match key_event.code {
                KeyCode::Char(c) if !key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                    let byte_idx = char_to_byte_index(&self.cwd, self.cursor);
                    self.cwd.insert(byte_idx, c);
                    self.cursor += 1;
                }
                KeyCode::Backspace => {
                    if self.cursor > 0 {
                        self.cursor -= 1;
                        let byte_idx = char_to_byte_index(&self.cwd, self.cursor);
                        self.cwd.remove(byte_idx);
                    }
                }
                KeyCode::Delete => {
                    let len = self.cwd.chars().count();
                    if self.cursor < len {
                        let byte_idx = char_to_byte_index(&self.cwd, self.cursor);
                        self.cwd.remove(byte_idx);
                    }
                }
                KeyCode::Left => {
                    self.cursor = self.cursor.saturating_sub(1);
                }
                KeyCode::Right => {
                    let len = self.cwd.chars().count();
                    if self.cursor < len {
                        self.cursor += 1;
                    }
                }
                KeyCode::Home => {
                    self.cursor = 0;
                }
                KeyCode::End => {
                    self.cursor = self.cwd.chars().count();
                }
                _ => {}
            },
        }

        PickerAction::None
    }
}

impl Widget for &ScopePicker {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let list_rows = self.programs.len().max(1) as u16 + 2;
        let desired_height = 1 + list_rows + 3 + 1 + 2;
        let popup_width = area.width.saturating_sub(4).min(64).max(24);
        let popup_height = area.height.saturating_sub(2).min(desired_height).max(9);
        let popup = centered_rect(popup_width, popup_height, area);

        Clear.render(popup, buf);

        let dir_label = match self.direction {
            NavDirection::Up => "Up",
            NavDirection::Down => "Down",
            NavDirection::Left => "Left",
            NavDirection::Right => "Right",
        };
        let outer = Block::bordered().title(format!(" New Scope ({dir_label}) "));
        let inner = outer.inner(popup);
        outer.render(popup, buf);

        let chunks = Layout::new(
            Direction::Vertical,
            [
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ],
        )
        .split(inner);

        Paragraph::new(Line::from(vec![
            Span::styled("Machine: ", Style::default().fg(Color::DarkGray)),
            Span::styled("localhost", Style::default().fg(Color::Cyan)),
        ]))
        .render(chunks[0], buf);

        let prog_border = if self.field == PickerField::Program {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let items: Vec<ListItem> = self
            .programs
            .iter()
            .enumerate()
            .map(|(i, opt)| {
                let prefix = if i == self.selected_program { "> " } else { "  " };
                let style = if i == self.selected_program {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(
                    format!("{prefix}{} ({})", opt.label, opt.program),
                    style,
                )))
            })
            .collect();
        List::new(items)
            .block(Block::bordered().title(" Program ").border_style(prog_border))
            .render(chunks[1], buf);

        let cwd_border = if self.field == PickerField::Cwd {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let cwd_block = Block::bordered()
            .title(" Working Directory ")
            .border_style(cwd_border);
        let cwd_inner = cwd_block.inner(chunks[2]);
        cwd_block.render(chunks[2], buf);

        if self.field == PickerField::Cwd {
            let byte_idx = char_to_byte_index(&self.cwd, self.cursor);
            let before = &self.cwd[..byte_idx];
            let (cursor_ch, after) = if byte_idx < self.cwd.len() {
                let ch = self.cwd[byte_idx..].chars().next().unwrap();
                (ch.to_string(), &self.cwd[byte_idx + ch.len_utf8()..])
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
            .render(cwd_inner, buf);
        } else {
            Paragraph::new(self.cwd.as_str()).render(cwd_inner, buf);
        }

        Paragraph::new(Span::styled(
            "Tab: field | Up/Down: select | Enter: launch | Esc: cancel",
            Style::default().fg(Color::DarkGray),
        ))
        .render(chunks[3], buf);
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map_or(s.len(), |(idx, _)| idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn picker_selects_program_and_edits_cwd() {
        let mut picker = ScopePicker::with_programs(
            NavDirection::Right,
            PathBuf::from("/tmp/work"),
            crate::config::default_programs(),
        );
        assert_eq!(picker.spec().program, "zsh");
        assert_eq!(picker.spec().machine, "localhost");
        assert_eq!(picker.spec().cwd, PathBuf::from("/tmp/work"));

        picker.handle_key(key(KeyCode::Down));
        assert_eq!(picker.spec().program, "/home/aolse/.asdf/shims/nvim");

        picker.handle_key(key(KeyCode::Down));
        assert_eq!(picker.spec().program, "lazygit");

        picker.handle_key(key(KeyCode::Tab));
        picker.handle_key(key(KeyCode::Char('/')));
        picker.handle_key(key(KeyCode::Char('a')));
        assert_eq!(picker.spec().cwd, PathBuf::from("/tmp/work/a"));

        picker.handle_key(key(KeyCode::Backspace));
        picker.handle_key(key(KeyCode::Backspace));
        assert_eq!(picker.spec().cwd, PathBuf::from("/tmp/work"));

        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            PickerAction::Confirm {
                direction: NavDirection::Right,
                spec: ScopeSpec {
                    machine: "localhost".to_string(),
                    cwd: PathBuf::from("/tmp/work"),
                    program: "lazygit".to_string(),
                },
            }
        );
    }

    #[test]
    fn picker_cancels_on_esc() {
        let mut picker = ScopePicker::new(NavDirection::Left, PathBuf::from("/tmp"));
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), PickerAction::Cancel);
    }
}
