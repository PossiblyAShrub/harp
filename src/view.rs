use std::collections::HashMap;

use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    prelude::Buffer,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Widget},
};
use tui_term::{vt100, widget::PseudoTerminal};

use crate::protocol::{ScopeSnapshot, SnapshotNode, SplitDir, TabPageSnapshot};

/// Renders a [`ScopeSnapshot`] as a tree of tile nodes. The snapshot's root is
/// rendered by the exact same code path as every other node.
pub struct SnapshotView<'a> {
    snap: &'a ScopeSnapshot,
    parsers: &'a HashMap<usize, vt100::Parser>,
}

impl<'a> SnapshotView<'a> {
    pub fn new(snap: &'a ScopeSnapshot, parsers: &'a HashMap<usize, vt100::Parser>) -> Self {
        Self { snap, parsers }
    }

    fn render_node(&self, node: &SnapshotNode, area: Rect, buf: &mut Buffer) {
        let focused_id = self.snap.focused_id;
        match node {
            SnapshotNode::Leaf { id, .. } => {
                let border_style = if *id == focused_id {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let block = Block::bordered().border_style(border_style);
                let inner = block.inner(area);
                block.render(area, buf);
                if let Some(parser) = self.parsers.get(id) {
                    PseudoTerminal::new(parser.screen()).render(inner, buf);
                }
            }
            SnapshotNode::Split {
                direction,
                children,
            } => {
                let dir = match direction {
                    SplitDir::Horizontal => Direction::Horizontal,
                    SplitDir::Vertical => Direction::Vertical,
                };
                let constraints: Vec<Constraint> =
                    children.iter().map(|c| Constraint::Fill(c.fill)).collect();
                let rects = Layout::new(dir, constraints).split(area);
                for (child, rect) in children.iter().zip(rects.iter()) {
                    self.render_node(&child.node, *rect, buf);
                }
            }
            SnapshotNode::Tabs { pages, active } => {
                let is_focused = pages.get(*active).is_some_and(|p| p.id == focused_id);
                let border_style = if is_focused {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let mut block = Block::bordered().border_style(border_style);
                if pages.len() > 1 {
                    block = block.title(render_tab_bar_line(pages, *active));
                }
                let inner = block.inner(area);
                block.render(area, buf);

                if let Some(page) = pages.get(*active)
                    && let Some(parser) = self.parsers.get(&page.id)
                {
                    PseudoTerminal::new(parser.screen()).render(inner, buf);
                }
            }
        }
    }
}

impl Widget for &SnapshotView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.render_node(&self.snap.root, area, buf);
    }
}

/// Returns the id of the projection rendered at screen position `(x, y)`,
/// mirroring the layout math used by [`SnapshotView::render_node`].
pub fn pane_at(node: &SnapshotNode, area: Rect, x: u16, y: u16) -> Option<usize> {
    if !area.contains(Position::new(x, y)) {
        return None;
    }
    match node {
        SnapshotNode::Leaf { id, .. } => Some(*id),
        SnapshotNode::Split {
            direction,
            children,
        } => {
            let dir = match direction {
                SplitDir::Horizontal => Direction::Horizontal,
                SplitDir::Vertical => Direction::Vertical,
            };
            let constraints: Vec<Constraint> =
                children.iter().map(|c| Constraint::Fill(c.fill)).collect();
            let rects = Layout::new(dir, constraints).split(area);
            for (child, rect) in children.iter().zip(rects.iter()) {
                if let Some(id) = pane_at(&child.node, *rect, x, y) {
                    return Some(id);
                }
            }
            None
        }
        SnapshotNode::Tabs { pages, active } => pages.get(*active).map(|p| p.id),
    }
}

/// Returns the terminal cell area rendered for a pane, matching
/// [`SnapshotView::render_node`] so mouse coordinates map to parser cells.
pub fn pane_content_rect(node: &SnapshotNode, area: Rect, id: usize) -> Option<Rect> {
    match node {
        SnapshotNode::Leaf { id: leaf_id, .. } if *leaf_id == id => {
            Some(Block::bordered().inner(area))
        }
        SnapshotNode::Leaf { .. } => None,
        SnapshotNode::Split {
            direction,
            children,
        } => {
            let dir = match direction {
                SplitDir::Horizontal => Direction::Horizontal,
                SplitDir::Vertical => Direction::Vertical,
            };
            let constraints: Vec<Constraint> = children
                .iter()
                .map(|child| Constraint::Fill(child.fill))
                .collect();
            let rects = Layout::new(dir, constraints).split(area);
            children
                .iter()
                .zip(rects.iter())
                .find_map(|(child, rect)| pane_content_rect(&child.node, *rect, id))
        }
        SnapshotNode::Tabs { pages, active } => pages
            .get(*active)
            .filter(|page| page.id == id)
            .map(|_| Block::bordered().inner(area)),
    }
}

fn render_tab_bar_line(pages: &[TabPageSnapshot], active: usize) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        let label = format!(" {} ", page.name);
        let style = if i == active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(label, style));
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{SnapshotChild, SplitDir};

    fn leaf(id: usize) -> SnapshotNode {
        SnapshotNode::Leaf {
            id,
            cols: 10,
            rows: 10,
            state: Vec::new(),
            scrollback: Vec::new(),
            alternate_screen: false,
        }
    }

    #[test]
    fn pane_at_finds_projection_under_point() {
        let root = SnapshotNode::Split {
            direction: SplitDir::Horizontal,
            children: vec![
                SnapshotChild {
                    fill: 1,
                    node: SnapshotNode::Split {
                        direction: SplitDir::Vertical,
                        children: vec![
                            SnapshotChild {
                                fill: 1,
                                node: leaf(1),
                            },
                            SnapshotChild {
                                fill: 1,
                                node: leaf(2),
                            },
                        ],
                    },
                },
                SnapshotChild {
                    fill: 1,
                    node: leaf(3),
                },
            ],
        };
        let area = Rect::new(0, 0, 100, 50);
        assert_eq!(pane_at(&root, area, 5, 5), Some(1));
        assert_eq!(pane_at(&root, area, 5, 40), Some(2));
        assert_eq!(pane_at(&root, area, 80, 20), Some(3));
        assert_eq!(pane_at(&root, area, 200, 200), None);
    }

    #[test]
    fn pane_at_uses_active_tab_page() {
        let root = SnapshotNode::Tabs {
            pages: vec![
                TabPageSnapshot {
                    id: 1,
                    name: "one".into(),
                    cols: 10,
                    rows: 10,
                    state: Vec::new(),
                    scrollback: Vec::new(),
                    alternate_screen: false,
                },
                TabPageSnapshot {
                    id: 2,
                    name: "two".into(),
                    cols: 10,
                    rows: 10,
                    state: Vec::new(),
                    scrollback: Vec::new(),
                    alternate_screen: false,
                },
            ],
            active: 1,
        };
        let area = Rect::new(0, 0, 100, 50);
        assert_eq!(pane_at(&root, area, 50, 25), Some(2));
    }
}
