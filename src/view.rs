use std::collections::HashMap;

use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    prelude::Buffer,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Widget},
};
use tui_term::{vt100, widget::PseudoTerminal};

use crate::{
    debug::{DebugLogProjection, ServerDebugProjection},
    protocol::{ScopeSnapshot, SnapshotChild, SnapshotNode, SplitDir, TabPageSnapshot},
    server::ServerTraceEntry,
};

/// Renders a [`ScopeSnapshot`] as a tree of tile nodes. The snapshot's root is
/// rendered by the exact same code path as every other node.
pub struct SnapshotView<'a> {
    snap: &'a ScopeSnapshot,
    parsers: &'a HashMap<usize, vt100::Parser>,
    trace: &'a [ServerTraceEntry],
}

impl<'a> SnapshotView<'a> {
    pub fn new(
        snap: &'a ScopeSnapshot,
        parsers: &'a HashMap<usize, vt100::Parser>,
        trace: &'a [ServerTraceEntry],
    ) -> Self {
        Self {
            snap,
            parsers,
            trace,
        }
    }

    fn render_node(&self, node: &SnapshotNode, area: Rect, buf: &mut Buffer) {
        let focused_id = self.snap.focused_id;
        match node {
            SnapshotNode::Leaf { id, name, .. } => {
                let is_focused = *id == focused_id;
                let mut block = Block::bordered().border_style(pane_border_style(is_focused));
                if !name.is_empty() {
                    block = block.title(pane_title_span(name, is_focused));
                }
                let inner = block.inner(area);
                block.render(area, buf);
                if let Some(parser) = self.parsers.get(id) {
                    PseudoTerminal::new(parser.screen()).render(inner, buf);
                }
            }
            SnapshotNode::Debug { .. } => {
                ServerDebugProjection::new(self.trace).render(area, buf);
            }
            SnapshotNode::DebugLog { .. } => {
                DebugLogProjection::new(&self.snap.debug_log).render(area, buf);
            }
            SnapshotNode::Split {
                direction,
                children,
            } => {
                let rects = split_layout(*direction, children).split(area);
                for (child, rect) in children.iter().zip(rects.iter()) {
                    self.render_node(&child.node, *rect, buf);
                }
            }
            SnapshotNode::Tabs { pages, active } => {
                let is_focused = pages.get(*active).is_some_and(|p| p.id == focused_id);
                let mut block = Block::bordered().border_style(pane_border_style(is_focused));
                if pages.len() > 1 {
                    block = block.title(render_tab_bar_line(pages, *active));
                } else if let Some(page) = pages.first()
                    && !page.name.is_empty()
                {
                    block = block.title(pane_title_span(&page.name, is_focused));
                }
                let inner = block.inner(area);
                block.render(area, buf);

                if let Some(page) = pages.get(*active) {
                    if page.is_debug {
                        ServerDebugProjection::new(self.trace).render(inner, buf);
                    } else if page.is_debug_log {
                        DebugLogProjection::new(&self.snap.debug_log).render(inner, buf);
                    } else if let Some(parser) = self.parsers.get(&page.id) {
                        PseudoTerminal::new(parser.screen()).render(inner, buf);
                    }
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

fn pane_border_style(is_focused: bool) -> Style {
    if is_focused {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn pane_title_span(title: &str, is_focused: bool) -> Span<'_> {
    let style = if is_focused {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Span::styled(format!(" {title} "), style)
}

fn split_layout(direction: SplitDir, children: &[SnapshotChild]) -> Layout {
    let direction = match direction {
        SplitDir::Horizontal => Direction::Horizontal,
        SplitDir::Vertical => Direction::Vertical,
    };
    Layout::new(
        direction,
        children.iter().map(|child| Constraint::Fill(child.fill)),
    )
}

fn find_pane<T>(
    node: &SnapshotNode,
    area: Rect,
    inside: impl Fn(Rect) -> bool + Copy,
    find: impl Fn(&SnapshotNode, Rect) -> Option<T> + Copy,
) -> Option<T> {
    if !inside(area) {
        return None;
    }
    match node {
        SnapshotNode::Split {
            direction,
            children,
        } => {
            let rects = split_layout(*direction, children).split(area);
            children
                .iter()
                .zip(rects.iter())
                .find_map(|(child, rect)| find_pane(&child.node, *rect, inside, find))
        }
        _ => find(node, area),
    }
}

fn active_id(node: &SnapshotNode) -> Option<usize> {
    match node {
        SnapshotNode::Leaf { id, .. }
        | SnapshotNode::Debug { id }
        | SnapshotNode::DebugLog { id } => Some(*id),
        SnapshotNode::Tabs { pages, active } => pages.get(*active).map(|page| page.id),
        SnapshotNode::Split { .. } => None,
    }
}

/// Returns the visible projection under the pointer.
pub fn pane_at(node: &SnapshotNode, area: Rect, x: u16, y: u16) -> Option<usize> {
    find_pane(
        node,
        area,
        |rect| rect.contains(Position::new(x, y)),
        |node, _| active_id(node),
    )
}

#[derive(Clone, Copy)]
pub struct PaneHit {
    pub active_id: usize,
    pub area: Rect,
    pub is_stack: bool,
    pub tab: Option<(usize, Rect)>,
}

pub fn hit_pane(node: &SnapshotNode, area: Rect, pos: Position) -> Option<PaneHit> {
    find_pane(
        node,
        area,
        |rect| rect.contains(pos),
        |node, area| {
            let pages = stack_pages(node);
            let tab = if pos.y == area.y {
                tab_rects(pages, area).find(|(_, rect)| rect.contains(pos))
            } else {
                None
            };
            Some(PaneHit {
                active_id: active_id(node)?,
                area,
                is_stack: !pages.is_empty(),
                tab,
            })
        },
    )
}

pub fn projection_position(
    node: &SnapshotNode,
    area: Rect,
    id: usize,
) -> Option<(Rect, Option<Rect>)> {
    find_pane(
        node,
        area,
        |_| true,
        |node, area| {
            let pages = stack_pages(node);
            if active_id(node) != Some(id) && !pages.iter().any(|page| page.id == id) {
                return None;
            }
            let tab =
                tab_rects(pages, area).find_map(|(page_id, rect)| (page_id == id).then_some(rect));
            Some((area, tab))
        },
    )
}

/// Returns the terminal cell area used by the renderer for the visible projection.
pub fn pane_content_rect(node: &SnapshotNode, area: Rect, id: usize) -> Option<Rect> {
    find_pane(
        node,
        area,
        |_| true,
        |node, area| (active_id(node) == Some(id)).then(|| Block::bordered().inner(area)),
    )
}

fn stack_pages(node: &SnapshotNode) -> &[TabPageSnapshot] {
    match node {
        SnapshotNode::Tabs { pages, .. } if pages.len() > 1 => pages,
        _ => &[],
    }
}

fn tab_text(page: &TabPageSnapshot) -> Span<'static> {
    Span::raw(format!("  {}  ", page.name))
}

fn tab_rects(pages: &[TabPageSnapshot], area: Rect) -> impl Iterator<Item = (usize, Rect)> + '_ {
    // Block titles start after the left border and end before the right border.
    let mut x = area.x.saturating_add(1);
    let right = area.right().saturating_sub(1);
    pages.iter().map_while(move |page| {
        let available = right.saturating_sub(x);
        if area.height == 0 || available == 0 {
            return None;
        }
        let width = tab_text(page).width().min(usize::from(available)) as u16;
        let rect = Rect::new(x, area.y, width, 1);
        x += width;
        Some((page.id, rect))
    })
}

fn render_tab_bar_line(pages: &[TabPageSnapshot], active: usize) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        let style = if i == active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(tab_text(page).style(style));
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
            name: String::new(),
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
                    is_debug: false,
                    is_debug_log: false,
                },
                TabPageSnapshot {
                    id: 2,
                    name: "two".into(),
                    cols: 10,
                    rows: 10,
                    state: Vec::new(),
                    scrollback: Vec::new(),
                    alternate_screen: false,
                    is_debug: false,
                    is_debug_log: false,
                },
            ],
            active: 1,
        };
        let area = Rect::new(0, 0, 100, 50);
        assert_eq!(pane_at(&root, area, 50, 25), Some(2));
    }

    #[test]
    fn leaf_renders_title_on_top_border() {
        let snap = ScopeSnapshot {
            scope_id: 1,
            scope_name: "test".into(),
            revision: 1,
            focused_id: 1,
            root: SnapshotNode::Leaf {
                id: 1,
                name: "nvim src/main.rs".into(),
                cols: 28,
                rows: 8,
                state: Vec::new(),
                scrollback: Vec::new(),
                alternate_screen: false,
            },
            passthrough: String::new(),
            debug_log: Vec::new(),
        };
        let parsers = HashMap::new();
        let area = Rect::new(0, 0, 30, 10);
        let mut buf = Buffer::empty(area);
        SnapshotView::new(&snap, &parsers, &[]).render(area, &mut buf);
        let top_row: String = (0..30).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            top_row.contains(" nvim src/main.rs "),
            "expected leaf title in top border, got: {top_row:?}"
        );
    }
}
