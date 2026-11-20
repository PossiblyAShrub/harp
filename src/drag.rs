use crossterm::event::{KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    prelude::Buffer,
    style::{Color, Modifier, Style},
    widgets::{Block, Clear, Paragraph, Widget},
};

use crate::{
    protocol::{DropAction, ScopeSnapshot},
    view::{PaneHit, hit_pane, projection_position},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum DragKind {
    Pane,
    Tab,
    Stack,
}

pub struct Drag {
    scope_id: u64,
    source: usize,
    kind: DragKind,
    start: Position,
    pointer: Option<MouseEvent>,
    cancelled: bool,
}

impl Drag {
    pub fn start(snap: &ScopeSnapshot, area: Rect, mouse: MouseEvent) -> Option<Self> {
        let start = position(mouse);
        let pane = hit_pane(&snap.root, area, start)?;
        if start.y != pane.area.y {
            return None;
        }

        let (source, kind) = match pane.tab {
            Some((id, _)) => (id, DragKind::Tab),
            None if pane.is_stack => (pane.active_id, DragKind::Stack),
            None => (pane.active_id, DragKind::Pane),
        };
        Some(Self {
            scope_id: snap.scope_id,
            source,
            kind,
            start,
            pointer: None,
            cancelled: false,
        })
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn track(&mut self, mouse: MouseEvent) {
        if self.pointer.is_some() || position(mouse) != self.start {
            self.pointer = Some(mouse);
        }
    }

    pub fn set_shift(&mut self, held: bool) {
        if let Some(pointer) = &mut self.pointer {
            pointer.modifiers.set(KeyModifiers::SHIFT, held);
        }
    }

    pub fn scope_id(&self) -> u64 {
        self.scope_id
    }

    pub fn source(&self) -> usize {
        self.source
    }

    pub fn whole_stack(&self) -> bool {
        self.kind == DragKind::Stack
    }

    pub fn is_click(&self, mouse: MouseEvent) -> bool {
        self.pointer.is_none() && position(mouse) == self.start
    }

    pub fn destination(
        &self,
        snap: &ScopeSnapshot,
        area: Rect,
        mouse: MouseEvent,
    ) -> Option<Destination> {
        if self.cancelled || snap.scope_id != self.scope_id || self.is_click(mouse) {
            return None;
        }
        let (source_area, _) = projection_position(&snap.root, area, self.source)?;
        let pos = position(mouse);
        let pane = hit_pane(&snap.root, area, pos)?;
        let target = pane.tab.map_or(pane.active_id, |(id, _)| id);
        let action = self.action(pane, pos, mouse.modifiers);
        if source_area == pane.area
            && (self.kind != DragKind::Tab
                || (!action.is_split() && (target == self.source || action == DropAction::Swap))
                || (action.is_split() && !pane.is_stack))
        {
            return None;
        }
        Some(Destination {
            pane,
            target,
            action,
        })
    }

    fn action(&self, pane: PaneHit, pos: Position, modifiers: KeyModifiers) -> DropAction {
        if modifiers.contains(KeyModifiers::SHIFT) {
            return DropAction::Tab;
        }
        if let Some((_, tab)) = pane.tab {
            return if pos.x - tab.x < tab.width.div_ceil(2) {
                DropAction::TabBefore
            } else {
                DropAction::Tab
            };
        }

        let rect = pane.area;
        let x = pos.x - rect.x;
        let y = pos.y - rect.y;
        let center = if self.whole_stack() {
            DropAction::Tab
        } else {
            DropAction::Swap
        };
        // Compare distances as fractions of the pane dimensions so corners
        // select the nearest edge in the same coordinate system as the zones.
        [
            (y, rect.height, DropAction::SplitVerticalBefore),
            (rect.height - y - 1, rect.height, DropAction::SplitVertical),
            (x, rect.width, DropAction::SplitHorizontalBefore),
            (rect.width - x - 1, rect.width, DropAction::SplitHorizontal),
        ]
        .into_iter()
        .filter(|&(distance, length, _)| length >= 2 && u32::from(distance) * 4 < u32::from(length))
        .min_by(|&(a, a_len, _), &(b, b_len, _)| {
            (u32::from(a) * u32::from(b_len)).cmp(&(u32::from(b) * u32::from(a_len)))
        })
        .map_or(center, |(_, _, action)| action)
    }

    pub fn render(&self, snap: &ScopeSnapshot, area: Rect, status: Rect, buf: &mut Buffer) {
        let Some(mouse) = self.pointer.filter(|_| !self.cancelled) else {
            return;
        };
        let Some((source, tab)) = projection_position(&snap.root, area, self.source) else {
            return;
        };
        let style = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let source = match (self.kind, tab) {
            (DragKind::Tab, Some(tab)) => tab,
            _ => Rect::new(source.x, source.y, source.width, source.height.min(1)),
        };
        buf.set_style(source, style);

        let hint = self
            .destination(snap, area, mouse)
            .map_or("Drag to a destination · Esc cancels", |destination| {
                destination.render(buf)
            });
        Clear.render(status, buf);
        Paragraph::new(hint).style(style).render(status, buf);
    }
}

pub struct Destination {
    pane: PaneHit,
    pub target: usize,
    pub action: DropAction,
}

impl Destination {
    fn render(self, buf: &mut Buffer) -> &'static str {
        use DropAction::*;

        let area = self.pane.area;
        let highlight = Style::default().fg(Color::Black).bg(Color::Yellow);
        let hint = match self.action {
            Swap => "Release to swap projections",
            Tab => "Release to place after",
            TabBefore => "Release to place before",
            SplitHorizontal => "Release to split right",
            SplitHorizontalBefore => "Release to split left",
            SplitVertical => "Release to split below",
            SplitVerticalBefore => "Release to split above",
        };
        match self.action {
            Swap => buf.set_style(Rect::new(area.x, area.y, area.width, 1), highlight),
            Tab | TabBefore => {
                let marker =
                    self.pane
                        .tab
                        .map_or(Rect::new(area.x, area.y, area.width, 1), |(_, tab)| {
                            let x = if self.action == TabBefore {
                                tab.x
                            } else {
                                tab.right() - 1
                            };
                            Rect::new(x, tab.y, 1, 1)
                        });
                buf.set_style(marker, highlight);
            }
            action => {
                let (direction, index) = match action {
                    SplitHorizontalBefore => (Direction::Horizontal, 0),
                    SplitHorizontal => (Direction::Horizontal, 1),
                    SplitVerticalBefore => (Direction::Vertical, 0),
                    SplitVertical => (Direction::Vertical, 1),
                    _ => unreachable!(),
                };
                let slots = Layout::new(direction, [Constraint::Fill(1); 2]).split(area);
                Block::bordered()
                    .border_style(Style::default().fg(Color::Yellow))
                    .render(slots[index], buf);
            }
        }
        hint
    }
}

fn position(mouse: MouseEvent) -> Position {
    Position::new(mouse.column, mouse.row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{SnapshotNode, TabPageSnapshot};
    use crossterm::event::{MouseButton, MouseEventKind};

    #[test]
    fn click_on_wide_tab_and_tear_it_out_from_the_same_pane() {
        let snap = ScopeSnapshot {
            scope_id: 5,
            scope_name: String::new(),
            focused_id: 1,
            revision: 0,
            passthrough: String::new(),
            root: SnapshotNode::Tabs {
                pages: ["終端", "two"]
                    .into_iter()
                    .enumerate()
                    .map(|(i, name)| TabPageSnapshot {
                        id: i + 1,
                        name: name.into(),
                        cols: 1,
                        rows: 1,
                        state: Vec::new(),
                        scrollback: Vec::new(),
                        alternate_screen: false,
                        is_debug: true,
                    })
                    .collect(),
                active: 0,
            },
        };
        let area = Rect::new(2, 3, 13, 16);
        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let first = MouseEvent {
            column: 10,
            ..press
        };
        assert_eq!(Drag::start(&snap, area, first).unwrap().source(), 1);
        let mut drag = Drag::start(&snap, area, press).unwrap();
        assert_eq!(drag.source(), 2);
        assert!(drag.is_click(press));
        assert!(drag.destination(&snap, area, press).is_none());

        let side = MouseEvent {
            column: 2,
            row: 10,
            ..press
        };
        assert!(Drag::start(&snap, area, side).is_none());
        drag.track(side);
        let destination = drag.destination(&snap, area, side).unwrap();
        assert_eq!(destination.target, 1);
        assert_eq!(destination.action, DropAction::SplitHorizontalBefore);
        assert!(
            drag.destination(&snap, area, MouseEvent { column: 15, ..side })
                .is_none()
        );
        let shifted = MouseEvent {
            modifiers: KeyModifiers::SHIFT,
            ..side
        };
        assert_eq!(
            drag.destination(&snap, area, shifted).unwrap().action,
            DropAction::Tab
        );
        drag.cancel();
        assert!(drag.destination(&snap, area, side).is_none());
    }
}
