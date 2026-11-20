use std::collections::HashMap;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
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
