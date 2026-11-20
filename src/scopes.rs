use std::{
    io,
    path::{Path, PathBuf},
};

use anyhow::Result;
use crossterm::event::{Event, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    widgets::Block,
};

use crate::{
    projections::ProjectionTerm,
    protocol::{
        DEBUG_PROJECTION_PROGRAM, DropAction, ScopeSnapshot, SnapshotChild, SnapshotNode, SplitDir,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeSpec {
    pub machine: String,
    pub cwd: PathBuf,
    pub program: String,
}

impl ScopeSpec {
    pub fn default_zsh(cwd: PathBuf) -> Self {
        Self {
            machine: "localhost".to_string(),
            cwd,
            program: "zsh".to_string(),
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NavDirection {
    Up,
    Down,
    Left,
    Right,
}

impl NavDirection {
    fn axis(self) -> Direction {
        match self {
            NavDirection::Left | NavDirection::Right => Direction::Horizontal,
            NavDirection::Up | NavDirection::Down => Direction::Vertical,
        }
    }

    fn insert_before(self) -> bool {
        matches!(self, NavDirection::Left | NavDirection::Up)
    }
}

pub enum Projection {
    Term(ProjectionTerm),
    ServerDebug,
}

impl Projection {
    fn from_spec(spec: &ScopeSpec, cols: u16, rows: u16, cell_px: (u16, u16)) -> Result<Self> {
        if spec.program == DEBUG_PROJECTION_PROGRAM {
            return Ok(Projection::ServerDebug);
        }
        let mut term = ProjectionTerm::with_command(cols, rows, &spec.program, &spec.cwd)?;
        term.set_cell_size(cell_px.0, cell_px.1)?;
        Ok(Projection::Term(term))
    }

    fn is_exited(&self) -> bool {
        match self {
            Projection::Term(term) => term.is_exited(),
            Projection::ServerDebug => false,
        }
    }

    fn needs_border(&self) -> bool {
        true
    }

    fn set_cell_pixels(&mut self, w: u16, h: u16) -> io::Result<()> {
        match self {
            Projection::Term(term) => term.set_cell_size(w, h),
            Projection::ServerDebug => Ok(()),
        }
    }

    fn take_passthrough(&mut self) -> Vec<u8> {
        match self {
            Projection::Term(term) => term.take_passthrough(),
            Projection::ServerDebug => Vec::new(),
        }
    }

    fn resize(&mut self, rect: Rect) -> io::Result<()> {
        match self {
            Projection::Term(term) => {
                let inner = pane_inner(rect);
                term.resize(inner.width, inner.height)
            }
            Projection::ServerDebug => Ok(()),
        }
    }

    fn handle_event(&mut self, event: &Event) -> io::Result<()> {
        match self {
            Projection::Term(term) => term.handle_event(event),
            Projection::ServerDebug => Ok(()),
        }
    }

    fn handle_mouse_event(&mut self, mouse_event: MouseEvent) -> io::Result<()> {
        match self {
            Projection::Term(term) => term.handle_mouse_event(mouse_event),
            Projection::ServerDebug => Ok(()),
        }
    }

    fn prune_exited(&mut self) -> io::Result<bool> {
        match self {
            Projection::Term(_) => Ok(false),
            Projection::ServerDebug => Ok(false),
        }
    }

    fn revision(&self) -> u64 {
        match self {
            Projection::Term(term) => term.revision(),
            Projection::ServerDebug => 0,
        }
    }

    fn force_close(&mut self) {
        if let Projection::Term(term) = self {
            term.force_close();
        }
    }
}

struct TileChild {
    constraint: Constraint,
    node: TileNode,
}

struct TabPage {
    id: usize,
    projection: Projection,
}

impl TabPage {
    fn into_leaf(self) -> TileNode {
        TileNode::Leaf {
            id: self.id,
            projection: self.projection,
        }
    }
}

enum TileNode {
    Leaf {
        id: usize,
        projection: Projection,
    },
    Split {
        direction: Direction,
        children: Vec<TileChild>,
    },
    Tabs {
        pages: Vec<TabPage>,
        active: usize,
    },
}

impl TileNode {
    fn pane(&self, id: usize) -> Option<&Self> {
        match self {
            Self::Leaf { id: leaf_id, .. } => (*leaf_id == id).then_some(self),
            Self::Tabs { pages, .. } => pages.iter().any(|page| page.id == id).then_some(self),
            Self::Split { children, .. } => children.iter().find_map(|child| child.node.pane(id)),
        }
    }

    fn extract(self, id: usize, whole_stack: bool) -> (Option<Self>, Option<Self>) {
        match self {
            node @ Self::Leaf { id: leaf_id, .. } if leaf_id == id && !whole_stack => {
                (None, Some(node))
            }
            Self::Tabs { mut pages, active } => {
                let Some(index) = pages.iter().position(|page| page.id == id) else {
                    return (Some(Self::Tabs { pages, active }), None);
                };
                if whole_stack {
                    return (None, Some(Self::Tabs { pages, active }));
                }

                let moving = pages.remove(index).into_leaf();
                let remainder = match pages.len() {
                    0 => None,
                    1 => pages.pop().map(TabPage::into_leaf),
                    _ => {
                        let active = (active - usize::from(index < active)).min(pages.len() - 1);
                        Some(Self::Tabs { pages, active })
                    }
                };
                (remainder, Some(moving))
            }
            Self::Split {
                direction,
                mut children,
            } => {
                let Some(index) = children
                    .iter()
                    .position(|child| child.node.pane(id).is_some())
                else {
                    return (
                        Some(Self::Split {
                            direction,
                            children,
                        }),
                        None,
                    );
                };
                let TileChild { constraint, node } = children.remove(index);
                let (remainder, moving) = node.extract(id, whole_stack);
                if let Some(node) = remainder {
                    children.insert(index, TileChild { constraint, node });
                }
                let remainder = if moving.is_some() && children.len() == 1 {
                    children.pop().map(|child| child.node)
                } else {
                    Some(Self::Split {
                        direction,
                        children,
                    })
                };
                (remainder, moving)
            }
            node => (Some(node), None),
        }
    }

    fn insert_drop(&mut self, target: usize, moving: Self, action: DropAction) {
        if let Self::Split { children, .. } = self {
            let child = children
                .iter_mut()
                .find(|child| child.node.pane(target).is_some())
                .expect("validated drop target");
            child.node.insert_drop(target, moving, action);
            return;
        }

        let existing = std::mem::replace(
            self,
            Self::Tabs {
                pages: Vec::new(),
                active: 0,
            },
        );
        *self = match action {
            DropAction::Tab | DropAction::TabBefore => {
                let (mut pages, _) = existing.into_pages();
                let (moving_pages, active) = moving.into_pages();
                let index = pages
                    .iter()
                    .position(|page| page.id == target)
                    .expect("validated target tab");
                let insert_at = index + usize::from(action == DropAction::Tab);
                pages.splice(insert_at..insert_at, moving_pages);
                Self::Tabs {
                    pages,
                    active: insert_at + active,
                }
            }
            _ => {
                let direction = match action {
                    DropAction::SplitHorizontalBefore => NavDirection::Left,
                    DropAction::SplitHorizontal => NavDirection::Right,
                    DropAction::SplitVerticalBefore => NavDirection::Up,
                    DropAction::SplitVertical => NavDirection::Down,
                    _ => unreachable!("expected a split action"),
                };
                Self::split_pair(existing, moving, direction)
            }
        };
    }

    fn into_pages(self) -> (Vec<TabPage>, usize) {
        match self {
            Self::Leaf { id, projection } => (vec![TabPage { id, projection }], 0),
            Self::Tabs { pages, active } => (pages, active),
            Self::Split { .. } => unreachable!("only panes can become tabs"),
        }
    }

    fn split_pair(existing: Self, new: Self, direction: NavDirection) -> Self {
        let nodes = if direction.insert_before() {
            [new, existing]
        } else {
            [existing, new]
        };
        Self::Split {
            direction: direction.axis(),
            children: nodes
                .into_iter()
                .map(|node| TileChild {
                    node,
                    constraint: Constraint::Fill(1),
                })
                .collect(),
        }
    }

    fn activate(&mut self, target: usize) -> bool {
        match self {
            Self::Leaf { id, .. } => *id == target,
            Self::Split { children, .. } => {
                children.iter_mut().any(|child| child.node.activate(target))
            }
            Self::Tabs { pages, active } => {
                let Some(index) = pages.iter().position(|page| page.id == target) else {
                    return false;
                };
                *active = index;
                true
            }
        }
    }

    fn matching_slots<'a>(
        &'a mut self,
        ids: [usize; 2],
        slots: &mut Vec<(&'a mut usize, &'a mut Projection)>,
    ) {
        match self {
            Self::Leaf { id, projection } if ids.contains(id) => slots.push((id, projection)),
            Self::Split { children, .. } => {
                for child in children {
                    child.node.matching_slots(ids, slots);
                }
            }
            Self::Tabs { pages, .. } => {
                for page in pages.iter_mut().filter(|page| ids.contains(&page.id)) {
                    slots.push((&mut page.id, &mut page.projection));
                }
            }
            _ => {}
        }
    }

    fn swap_slots(&mut self, source: usize, target: usize) -> bool {
        let mut slots = Vec::with_capacity(2);
        self.matching_slots([source, target], &mut slots);
        let [(left_id, left), (right_id, right)] = slots.as_mut_slice() else {
            return false;
        };
        std::mem::swap(*left_id, *right_id);
        std::mem::swap(*left, *right);
        true
    }

    fn collect_rects(&self, area: Rect, out: &mut Vec<(usize, Rect, bool)>) {
        match self {
            TileNode::Leaf { id, projection } => {
                out.push((*id, area, projection.needs_border()));
            }
            TileNode::Split {
                direction,
                children,
            } => {
                let constraints: Vec<Constraint> = children.iter().map(|c| c.constraint).collect();
                let rects = Layout::new(*direction, constraints).split(area);
                for (child, rect) in children.iter().zip(rects.iter()) {
                    child.node.collect_rects(*rect, out);
                }
            }
            TileNode::Tabs { pages, active } => {
                if let Some(page) = pages.get(*active) {
                    out.push((page.id, area, true));
                }
            }
        }
    }

    fn find_mut(&mut self, target_id: usize) -> Option<&mut Projection> {
        match self {
            TileNode::Leaf { id, projection } => (*id == target_id).then_some(projection),
            TileNode::Split { children, .. } => {
                for child in children {
                    if let Some(found) = child.node.find_mut(target_id) {
                        return Some(found);
                    }
                }
                None
            }
            TileNode::Tabs { pages, .. } => {
                for page in pages {
                    if page.id == target_id {
                        return Some(&mut page.projection);
                    }
                }
                None
            }
        }
    }

    fn first_visible_id(&self) -> Option<usize> {
        match self {
            TileNode::Leaf { id, .. } => Some(*id),
            TileNode::Split { children, .. } => {
                children.iter().find_map(|c| c.node.first_visible_id())
            }
            TileNode::Tabs { pages, active } => pages.get(*active).map(|p| p.id),
        }
    }

    fn set_cell_pixels(&mut self, w: u16, h: u16) -> io::Result<()> {
        match self {
            TileNode::Leaf { projection, .. } => projection.set_cell_pixels(w, h),
            TileNode::Split { children, .. } => {
                for child in children.iter_mut() {
                    child.node.set_cell_pixels(w, h)?;
                }
                Ok(())
            }
            TileNode::Tabs { pages, .. } => {
                for page in pages.iter_mut() {
                    page.projection.set_cell_pixels(w, h)?;
                }
                Ok(())
            }
        }
    }

    fn drain_passthrough(&mut self) -> Vec<u8> {
        match self {
            TileNode::Leaf { projection, .. } => projection.take_passthrough(),
            TileNode::Split { children, .. } => {
                children.iter_mut().fold(Vec::new(), |mut acc, c| {
                    acc.extend(c.node.drain_passthrough());
                    acc
                })
            }
            TileNode::Tabs { pages, .. } => pages.iter_mut().fold(Vec::new(), |mut acc, p| {
                acc.extend(p.projection.take_passthrough());
                acc
            }),
        }
    }

    fn resize(&mut self, area: Rect) -> io::Result<()> {
        match self {
            TileNode::Leaf { projection, .. } => projection.resize(area),
            TileNode::Split {
                direction,
                children,
            } => {
                let constraints: Vec<Constraint> = children.iter().map(|c| c.constraint).collect();
                let rects = Layout::new(*direction, constraints).split(area);
                for (child, rect) in children.iter_mut().zip(rects.iter()) {
                    child.node.resize(*rect)?;
                }
                Ok(())
            }
            TileNode::Tabs { pages, .. } => {
                for page in pages {
                    page.projection.resize(area)?;
                }
                Ok(())
            }
        }
    }

    fn term_revision_sum(&self) -> u64 {
        match self {
            TileNode::Leaf { projection, .. } => projection.revision(),
            TileNode::Split { children, .. } => children
                .iter()
                .map(|c| c.node.term_revision_sum())
                .fold(0u64, |acc, r| acc.wrapping_add(r)),
            TileNode::Tabs { pages, .. } => pages
                .iter()
                .map(|p| p.projection.revision())
                .fold(0u64, |acc, r| acc.wrapping_add(r)),
        }
    }

    fn leaf_count(&self) -> usize {
        match self {
            TileNode::Leaf { .. } => 1,
            TileNode::Split { children, .. } => children.iter().map(|c| c.node.leaf_count()).sum(),
            TileNode::Tabs { pages, .. } => pages.len(),
        }
    }

    fn to_snapshot(&self) -> Option<SnapshotNode> {
        match self {
            TileNode::Leaf { id, projection } => match projection {
                Projection::Term(term) => {
                    let (cols, rows, state, scrollback, alternate_screen) = term.state_snapshot();
                    Some(SnapshotNode::Leaf {
                        id: *id,
                        cols,
                        rows,
                        state,
                        scrollback,
                        alternate_screen,
                    })
                }
                Projection::ServerDebug => Some(SnapshotNode::Debug { id: *id }),
            },
            TileNode::Split {
                direction,
                children,
            } => {
                let dir = match direction {
                    Direction::Horizontal => SplitDir::Horizontal,
                    Direction::Vertical => SplitDir::Vertical,
                };
                let snap_children: Vec<SnapshotChild> = children
                    .iter()
                    .filter_map(|c| {
                        let fill = match c.constraint {
                            Constraint::Fill(n) => n,
                            _ => 1,
                        };
                        c.node
                            .to_snapshot()
                            .map(|node| SnapshotChild { fill, node })
                    })
                    .collect();
                if snap_children.is_empty() {
                    None
                } else {
                    Some(SnapshotNode::Split {
                        direction: dir,
                        children: snap_children,
                    })
                }
            }
            TileNode::Tabs { pages, active } => {
                let snap_pages: Vec<crate::protocol::TabPageSnapshot> = pages
                    .iter()
                    .map(|page| match &page.projection {
                        Projection::Term(term) => {
                            let (cols, rows, state, scrollback, alternate_screen) =
                                term.state_snapshot();
                            crate::protocol::TabPageSnapshot {
                                id: page.id,
                                name: term.title(),
                                cols,
                                rows,
                                state,
                                scrollback,
                                alternate_screen,
                                is_debug: false,
                            }
                        }
                        Projection::ServerDebug => crate::protocol::TabPageSnapshot {
                            id: page.id,
                            name: "Server Debug".to_string(),
                            cols: 1,
                            rows: 1,
                            state: Vec::new(),
                            scrollback: Vec::new(),
                            alternate_screen: false,
                            is_debug: true,
                        },
                    })
                    .collect();
                if snap_pages.is_empty() {
                    None
                } else {
                    let active_idx = (*active).min(snap_pages.len().saturating_sub(1));
                    Some(SnapshotNode::Tabs {
                        pages: snap_pages,
                        active: active_idx,
                    })
                }
            }
        }
    }

    fn split_leaf(
        &mut self,
        target_id: usize,
        dir: NavDirection,
        new_id: usize,
        new_projection: Projection,
    ) -> Result<(), Projection> {
        match self {
            TileNode::Leaf { id, .. } => {
                if *id == target_id {
                    let existing = std::mem::replace(
                        self,
                        TileNode::Split {
                            direction: dir.axis(),
                            children: Vec::new(),
                        },
                    );
                    let new_leaf = TileNode::Leaf {
                        id: new_id,
                        projection: new_projection,
                    };
                    *self = Self::split_pair(existing, new_leaf, dir);
                    Ok(())
                } else {
                    Err(new_projection)
                }
            }
            TileNode::Split {
                direction,
                children,
            } => {
                if *direction == dir.axis()
                    && let Some(pos) = children.iter().position(
                        |c| matches!(c.node, TileNode::Leaf { id, .. } if id == target_id),
                    )
                {
                    let insert_idx = if dir.insert_before() { pos } else { pos + 1 };
                    children.insert(
                        insert_idx,
                        TileChild {
                            constraint: Constraint::Fill(1),
                            node: TileNode::Leaf {
                                id: new_id,
                                projection: new_projection,
                            },
                        },
                    );
                    return Ok(());
                }

                let mut pending = Some(new_projection);
                for child in children.iter_mut() {
                    if let Some(proj) = pending.take() {
                        match child.node.split_leaf(target_id, dir, new_id, proj) {
                            Ok(()) => return Ok(()),
                            Err(returned) => pending = Some(returned),
                        }
                    }
                }
                Err(pending.unwrap())
            }
            TileNode::Tabs { pages, .. } => {
                if pages.iter().any(|p| p.id == target_id) {
                    let existing = std::mem::replace(
                        self,
                        TileNode::Split {
                            direction: dir.axis(),
                            children: Vec::new(),
                        },
                    );
                    let new_leaf = TileNode::Leaf {
                        id: new_id,
                        projection: new_projection,
                    };
                    *self = Self::split_pair(existing, new_leaf, dir);
                    Ok(())
                } else {
                    Err(new_projection)
                }
            }
        }
    }

    fn new_tab(
        &mut self,
        target_id: usize,
        new_id: usize,
        new_projection: Projection,
    ) -> Result<(), Projection> {
        match self {
            TileNode::Leaf { id, .. } => {
                if *id == target_id {
                    let existing = std::mem::replace(
                        self,
                        TileNode::Tabs {
                            pages: Vec::new(),
                            active: 0,
                        },
                    );
                    let existing_leaf = match existing {
                        TileNode::Leaf { id, projection } => TabPage { id, projection },
                        _ => unreachable!("leaf match guaranteed"),
                    };
                    *self = TileNode::Tabs {
                        pages: vec![
                            existing_leaf,
                            TabPage {
                                id: new_id,
                                projection: new_projection,
                            },
                        ],
                        active: 1,
                    };
                    Ok(())
                } else {
                    Err(new_projection)
                }
            }
            TileNode::Split { children, .. } => {
                let mut pending = Some(new_projection);
                for child in children.iter_mut() {
                    if let Some(proj) = pending.take() {
                        match child.node.new_tab(target_id, new_id, proj) {
                            Ok(()) => return Ok(()),
                            Err(returned) => pending = Some(returned),
                        }
                    }
                }
                Err(pending.unwrap())
            }
            TileNode::Tabs { pages, active } => {
                if pages.iter().any(|p| p.id == target_id) {
                    pages.push(TabPage {
                        id: new_id,
                        projection: new_projection,
                    });
                    *active = pages.len() - 1;
                    Ok(())
                } else {
                    Err(new_projection)
                }
            }
        }
    }

    fn switch_tab(&mut self, target_id: usize, dir: crate::protocol::TabDir) -> Option<usize> {
        match self {
            TileNode::Leaf { .. } => None,
            TileNode::Split { children, .. } => {
                for child in children {
                    if let Some(id) = child.node.switch_tab(target_id, dir) {
                        return Some(id);
                    }
                }
                None
            }
            TileNode::Tabs { pages, active } => {
                if !pages.iter().any(|p| p.id == target_id) {
                    return None;
                }
                let count = pages.len();
                let next = match dir {
                    crate::protocol::TabDir::Next => (*active + 1) % count,
                    crate::protocol::TabDir::Prev => (*active + count - 1) % count,
                };
                *active = next;
                Some(pages[next].id)
            }
        }
    }

    fn prune_exited(node_opt: &mut Option<TileNode>) -> io::Result<bool> {
        let Some(node) = node_opt else {
            return Ok(false);
        };

        let mut changed = false;
        match node {
            TileNode::Leaf { projection, .. } => {
                if projection.prune_exited()? {
                    changed = true;
                }
                if projection.is_exited() {
                    *node_opt = None;
                    return Ok(true);
                }
            }
            TileNode::Split { children, .. } => {
                let mut i = 0;
                while i < children.len() {
                    let mut child_opt = Some(std::mem::replace(
                        &mut children[i].node,
                        TileNode::Split {
                            direction: Direction::Horizontal,
                            children: Vec::new(),
                        },
                    ));
                    if Self::prune_exited(&mut child_opt)? {
                        changed = true;
                    }
                    if let Some(kept) = child_opt {
                        children[i].node = kept;
                        i += 1;
                    } else {
                        children.remove(i);
                        changed = true;
                    }
                }

                if children.is_empty() {
                    *node_opt = None;
                    return Ok(true);
                } else if children.len() == 1 {
                    let only = children.pop().unwrap().node;
                    *node = only;
                    changed = true;
                }
            }
            TileNode::Tabs { pages, active } => {
                let mut i = 0;
                while i < pages.len() {
                    if pages[i].projection.is_exited() {
                        pages.remove(i);
                        changed = true;
                    } else {
                        i += 1;
                    }
                }

                if pages.is_empty() {
                    *node_opt = None;
                    return Ok(true);
                }

                if *active >= pages.len() {
                    *active = pages.len() - 1;
                    changed = true;
                }
            }
        }

        Ok(changed)
    }
}

pub struct Scope {
    root: Option<TileNode>,
    focused_id: usize,
    next_id: usize,
    area: Rect,
    layout_revision: u64,
    cell_px: (u16, u16),
}

impl Scope {
    pub fn from_children(
        direction: Direction,
        children: Vec<(Constraint, Projection)>,
        area: Rect,
    ) -> io::Result<Self> {
        let mut next_id = 0;
        let tile_children: Vec<TileChild> = children
            .into_iter()
            .map(|(constraint, projection)| {
                let id = next_id;
                next_id += 1;
                TileChild {
                    constraint,
                    node: TileNode::Leaf { id, projection },
                }
            })
            .collect();

        let root = if tile_children.is_empty() {
            None
        } else if tile_children.len() == 1 {
            Some(tile_children.into_iter().next().unwrap().node)
        } else {
            Some(TileNode::Split {
                direction,
                children: tile_children,
            })
        };

        let focused_id = root
            .as_ref()
            .and_then(|n| n.first_visible_id())
            .unwrap_or(0);
        let mut scope = Self {
            root,
            focused_id,
            next_id,
            area,
            layout_revision: 1,
            cell_px: (0, 0),
        };
        scope.resize(area)?;
        Ok(scope)
    }

    pub fn with_terminals_in(
        count: usize,
        direction: Direction,
        area: Rect,
        cwd: &Path,
        cell_px: (u16, u16),
    ) -> Result<Self> {
        let constraints = vec![Constraint::Fill(1); count];
        let rects = Layout::new(direction, constraints).split(area);
        let mut children = Vec::with_capacity(count);
        for rect in rects.iter() {
            let inner = pane_inner(*rect);
            let mut term = ProjectionTerm::with_command(inner.width, inner.height, "zsh", cwd)?;
            term.set_cell_size(cell_px.0, cell_px.1)?;
            children.push((Constraint::Fill(1), Projection::Term(term)));
        }

        let mut scope = Self::from_children(direction, children, area)?;
        scope.cell_px = cell_px;
        Ok(scope)
    }

    pub fn revision(&self) -> u64 {
        let terms = self.root.as_ref().map_or(0, TileNode::term_revision_sum);
        self.layout_revision.wrapping_add(terms)
    }

    pub fn bump_revision(&mut self) {
        self.layout_revision = self.layout_revision.wrapping_add(1);
    }

    pub fn leaf_count(&self) -> usize {
        self.root.as_ref().map_or(0, TileNode::leaf_count)
    }

    pub fn snapshot(&mut self, scope_id: u64, scope_name: String) -> Option<ScopeSnapshot> {
        let passthrough = self
            .root
            .as_mut()
            .map_or_else(Vec::new, TileNode::drain_passthrough);
        // The bytes are always ASCII (base64 payloads in APC escape codes).
        let passthrough = String::from_utf8_lossy(&passthrough).into_owned();
        let root = self.root.as_ref()?.to_snapshot()?;
        Some(ScopeSnapshot {
            scope_id,
            scope_name,
            revision: self.revision(),
            focused_id: self.focused_id,
            root,
            passthrough,
        })
    }

    /// Updates the character cell pixel size used for TIOCGWINSZ reporting.
    pub fn set_cell_pixels(&mut self, w: u16, h: u16) -> io::Result<()> {
        if self.cell_px == (w, h) {
            return Ok(());
        }
        self.cell_px = (w, h);
        if let Some(root) = &mut self.root {
            root.set_cell_pixels(w, h)?;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn resize(&mut self, area: Rect) -> io::Result<()> {
        self.area = area;
        self.layout_revision = self.layout_revision.wrapping_add(1);
        if let Some(root) = &mut self.root {
            root.resize(area)?;
        }
        Ok(())
    }

    pub fn prune_exited(&mut self) -> io::Result<bool> {
        let changed = TileNode::prune_exited(&mut self.root)?;
        if changed {
            if let Some(root) = &self.root {
                let mut rects = Vec::new();
                root.collect_rects(self.area, &mut rects);
                if !rects.iter().any(|(id, _, _)| *id == self.focused_id)
                    && let Some((id, _, _)) = rects.first()
                {
                    self.focused_id = *id;
                }
            }
            self.layout_revision = self.layout_revision.wrapping_add(1);
            self.resize(self.area)?;
        }
        Ok(changed)
    }

    pub fn close_projection(&mut self, projection_id: usize) -> io::Result<bool> {
        let Some(projection) = self
            .root
            .as_mut()
            .and_then(|root| root.find_mut(projection_id))
        else {
            return Ok(false);
        };
        projection.force_close();
        let Some(root) = self.root.take() else {
            return Ok(false);
        };
        let (root, removed) = root.extract(projection_id, false);
        let removed = removed.is_some();
        self.root = root;
        if removed {
            if self.focused_id == projection_id {
                self.focused_id = self
                    .root
                    .as_ref()
                    .and_then(TileNode::first_visible_id)
                    .unwrap_or(0);
            }
            self.resize(self.area)?;
        }
        Ok(removed)
    }

    pub fn move_focus(&mut self, dir: NavDirection) {
        if let Some(next_id) = self.focus_neighbor(dir) {
            self.focused_id = next_id;
            self.layout_revision = self.layout_revision.wrapping_add(1);
        }
    }

    fn focus_neighbor(&self, dir: NavDirection) -> Option<usize> {
        let root = self.root.as_ref()?;
        let mut rects = Vec::new();
        root.collect_rects(self.area, &mut rects);
        let (_, current, _) = rects.iter().find(|(id, _, _)| *id == self.focused_id)?;
        find_neighbor(&rects, self.focused_id, *current, dir)
    }

    pub fn drop_projection(
        &mut self,
        source: usize,
        target: usize,
        action: DropAction,
        whole_stack: bool,
    ) -> io::Result<bool> {
        let Some(root) = &mut self.root else {
            return Ok(false);
        };
        let (Some(source_pane), Some(target_pane)) = (root.pane(source), root.pane(target)) else {
            return Ok(false);
        };
        let same_pane = std::ptr::eq(source_pane, target_pane);
        let target = if same_pane && action.is_split() && !whole_stack {
            let TileNode::Tabs { pages, .. } = source_pane else {
                return Ok(false);
            };
            let Some(sibling) = pages.iter().find(|page| page.id != source) else {
                return Ok(false);
            };
            sibling.id
        } else if source == target || (same_pane && whole_stack) {
            return Ok(false);
        } else {
            target
        };

        if whole_stack && !matches!(source_pane, TileNode::Tabs { pages, .. } if pages.len() > 1) {
            return Ok(false);
        }
        let action = if whole_stack && action == DropAction::Swap {
            DropAction::Tab
        } else {
            action
        };
        if action == DropAction::Swap {
            if !root.swap_slots(source, target) {
                return Ok(false);
            }
        } else {
            // Both projections exist, and the destination survives extraction.
            let root = self.root.take().expect("validated root");
            let (remainder, moving) = root.extract(source, whole_stack);
            let mut root = remainder.expect("validated destination");
            root.insert_drop(target, moving.expect("validated source"), action);
            self.root = Some(root);
        }

        self.focused_id = source;
        self.root
            .as_mut()
            .expect("root after drop")
            .activate(source);
        self.resize(self.area)?;
        Ok(true)
    }

    pub fn move_projection(&mut self, dir: NavDirection) -> io::Result<bool> {
        let Some(target) = self.focus_neighbor(dir) else {
            return Ok(false);
        };
        self.drop_projection(self.focused_id, target, DropAction::Swap, false)
    }

    pub fn activate_projection(&mut self, id: usize) -> bool {
        if self.focused_id == id {
            return false;
        }
        if self.root.as_mut().is_some_and(|root| root.activate(id)) {
            self.focused_id = id;
            self.layout_revision = self.layout_revision.wrapping_add(1);
            true
        } else {
            false
        }
    }

    pub fn split_focused_with_spec(&mut self, dir: NavDirection, spec: &ScopeSpec) -> Result<()> {
        let Some(root) = &mut self.root else {
            let inner = pane_inner(self.area);
            let projection = Projection::from_spec(spec, inner.width, inner.height, self.cell_px)?;
            let id = self.next_id;
            self.next_id += 1;
            self.root = Some(TileNode::Leaf { id, projection });
            self.focused_id = id;
            return Ok(());
        };

        let new_id = self.next_id;
        self.next_id += 1;

        let placeholder = Projection::from_spec(spec, 1, 1, self.cell_px)?;
        if root
            .split_leaf(self.focused_id, dir, new_id, placeholder)
            .is_ok()
        {
            self.focused_id = new_id;
            self.layout_revision = self.layout_revision.wrapping_add(1);
            self.resize(self.area)?;
        }

        Ok(())
    }

    pub fn new_tab_with_spec(&mut self, spec: &ScopeSpec) -> Result<()> {
        let Some(root) = &mut self.root else {
            let inner = pane_inner(self.area);
            let projection = Projection::from_spec(spec, inner.width, inner.height, self.cell_px)?;
            let id = self.next_id;
            self.next_id += 1;
            self.root = Some(TileNode::Leaf { id, projection });
            self.focused_id = id;
            return Ok(());
        };
        let new_id = self.next_id;
        self.next_id += 1;

        let placeholder = Projection::from_spec(spec, 1, 1, self.cell_px)?;
        if root.new_tab(self.focused_id, new_id, placeholder).is_ok() {
            self.focused_id = new_id;
            self.layout_revision = self.layout_revision.wrapping_add(1);
            self.resize(self.area)?;
        }

        Ok(())
    }

    pub fn switch_tab(&mut self, dir: crate::protocol::TabDir) -> bool {
        let Some(root) = &mut self.root else {
            return false;
        };
        if let Some(new_focus) = root.switch_tab(self.focused_id, dir) {
            self.focused_id = new_focus;
            self.layout_revision = self.layout_revision.wrapping_add(1);
            true
        } else {
            false
        }
    }

    pub fn handle_event(&mut self, event: &Event) -> io::Result<()> {
        match event {
            Event::Resize(cols, rows) => self.resize(Rect::new(0, 0, *cols, *rows)),
            Event::Mouse(mouse_event) => {
                let pos = Position::new(mouse_event.column, mouse_event.row);
                let Some(root) = &mut self.root else {
                    return Ok(());
                };
                let mut rects = Vec::new();
                root.collect_rects(self.area, &mut rects);

                for (id, rect, bordered) in rects {
                    let target = if bordered { pane_inner(rect) } else { rect };
                    if !target.contains(pos) {
                        continue;
                    }

                    let translated = MouseEvent {
                        column: mouse_event.column - target.x,
                        row: mouse_event.row - target.y,
                        ..*mouse_event
                    };

                    // Forward the event to the pane under the pointer. The pane
                    // decides whether to deliver it based on the mouse protocol
                    // mode it requested (clicks for most apps, motion only when
                    // the app asked for it).
                    if let Some(proj) = root.find_mut(id) {
                        proj.handle_mouse_event(translated)?;
                    }

                    // Focus a pane on press and wheel events, but not on plain
                    // pointer motion or drags, so hovering never steals focus
                    // and a drag that strays across panes doesn't flip focus
                    // mid-gesture.
                    let activates = matches!(
                        mouse_event.kind,
                        MouseEventKind::Down(_)
                            | MouseEventKind::ScrollUp
                            | MouseEventKind::ScrollDown
                            | MouseEventKind::ScrollLeft
                            | MouseEventKind::ScrollRight
                    );
                    if activates && self.focused_id != id {
                        self.focused_id = id;
                        self.layout_revision = self.layout_revision.wrapping_add(1);
                    }
                    return Ok(());
                }
                Ok(())
            }
            _ => {
                if let Some(root) = &mut self.root
                    && let Some(proj) = root.find_mut(self.focused_id)
                {
                    proj.handle_event(event)?;
                }
                Ok(())
            }
        }
    }
}

pub fn pane_inner(rect: Rect) -> Rect {
    Block::bordered().inner(rect)
}

fn interval_distance(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    if a_end <= b_start {
        b_start - a_end
    } else {
        a_start.saturating_sub(b_end)
    }
}

fn find_neighbor(
    rects: &[(usize, Rect, bool)],
    current_id: usize,
    current: Rect,
    dir: NavDirection,
) -> Option<usize> {
    let mut best: Option<(usize, (bool, u16, u16))> = None;

    for &(id, cand, _) in rects {
        if id == current_id {
            continue;
        }

        let (primary, secondary) = match dir {
            NavDirection::Right => {
                if cand.x < current.x.saturating_add(current.width) {
                    continue;
                }
                (
                    cand.x - current.x.saturating_add(current.width),
                    interval_distance(current.y, current.height, cand.y, cand.height),
                )
            }
            NavDirection::Left => {
                if cand.x.saturating_add(cand.width) > current.x {
                    continue;
                }
                (
                    current.x - cand.x.saturating_add(cand.width),
                    interval_distance(current.y, current.height, cand.y, cand.height),
                )
            }
            NavDirection::Down => {
                if cand.y < current.y.saturating_add(current.height) {
                    continue;
                }
                (
                    cand.y - current.y.saturating_add(current.height),
                    interval_distance(current.x, current.width, cand.x, cand.width),
                )
            }
            NavDirection::Up => {
                if cand.y.saturating_add(cand.height) > current.y {
                    continue;
                }
                (
                    current.y - cand.y.saturating_add(cand.height),
                    interval_distance(current.x, current.width, cand.x, cand.width),
                )
            }
        };

        let score = (secondary > 0, primary, secondary);
        if best.is_none_or(|(_, best_score)| score < best_score) {
            best = Some((id, score));
        }
    }

    best.map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_drop_preserves_projection_and_revision() {
        let mut scope = Scope::from_children(
            Direction::Horizontal,
            (0..2)
                .map(|_| (Constraint::Fill(1), Projection::ServerDebug))
                .collect(),
            Rect::new(0, 0, 80, 24),
        )
        .unwrap();
        let source = scope.focused_id;
        let revision = scope.revision();
        assert!(
            !scope
                .drop_projection(source, usize::MAX, DropAction::Tab, false)
                .unwrap()
        );
        assert_eq!(scope.leaf_count(), 2);
        assert_eq!(scope.revision(), revision);
        assert_eq!(scope.focused_id, source);
    }

    #[test]
    fn moves_keep_projection_ids_and_focus_with_the_projection() {
        let area = Rect::new(0, 0, 80, 24);
        let mut scope = Scope::from_children(
            Direction::Horizontal,
            (0..3)
                .map(|_| (Constraint::Fill(1), Projection::ServerDebug))
                .collect(),
            area,
        )
        .unwrap();

        assert!(scope.move_projection(NavDirection::Right).unwrap());
        let mut rects = Vec::new();
        scope.root.as_ref().unwrap().collect_rects(area, &mut rects);
        assert_eq!(
            rects.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
            [1, 0, 2]
        );
        assert_eq!(scope.focused_id, 0);

        assert!(scope.drop_projection(0, 1, DropAction::Tab, false).unwrap());
        assert!(scope.drop_projection(1, 0, DropAction::Tab, false).unwrap());
        assert!(
            scope
                .drop_projection(1, 2, DropAction::TabBefore, true)
                .unwrap()
        );
        let Some(TileNode::Tabs { pages, active }) = &scope.root else {
            panic!("expected a merged stack");
        };
        assert_eq!(
            pages.iter().map(|page| page.id).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(pages[*active].id, 1);

        assert!(
            scope
                .drop_projection(1, 1, DropAction::SplitVerticalBefore, false)
                .unwrap()
        );
        let mut rects = Vec::new();
        scope.root.as_ref().unwrap().collect_rects(area, &mut rects);
        assert_eq!((rects[0].0, rects[0].1), (1, Rect::new(0, 0, 80, 12)));
        assert_eq!(scope.leaf_count(), 3);
        assert_eq!(scope.focused_id, 1);
    }

    #[test]
    fn find_neighbor_navigates_2d_grid() {
        let rects = vec![
            (0, Rect::new(0, 0, 50, 20), true),
            (1, Rect::new(0, 20, 50, 20), true),
            (2, Rect::new(50, 0, 50, 40), true),
        ];

        assert_eq!(
            find_neighbor(&rects, 0, rects[0].1, NavDirection::Down),
            Some(1)
        );
        assert_eq!(
            find_neighbor(&rects, 1, rects[1].1, NavDirection::Up),
            Some(0)
        );
        assert_eq!(
            find_neighbor(&rects, 0, rects[0].1, NavDirection::Right),
            Some(2)
        );
        assert_eq!(
            find_neighbor(&rects, 1, rects[1].1, NavDirection::Right),
            Some(2)
        );
        assert_eq!(
            find_neighbor(&rects, 2, rects[2].1, NavDirection::Left),
            Some(0)
        );
        assert_eq!(
            find_neighbor(&rects, 0, rects[0].1, NavDirection::Left),
            None
        );
    }

    #[test]
    fn pane_inner_accounts_for_borders() {
        let rect = Rect::new(50, 0, 50, 40);
        assert_eq!(pane_inner(rect), Rect::new(51, 1, 48, 38));
    }

    #[test]
    fn mouse_press_focuses_pane_under_cursor_but_hover_does_not() {
        let cwd = std::env::temp_dir();
        let mut scope = Scope::with_terminals_in(
            2,
            Direction::Horizontal,
            Rect::new(0, 0, 80, 24),
            &cwd,
            (8, 16),
        )
        .unwrap();
        assert_eq!(scope.focused_id, 0);

        // Pressing inside the right-hand pane moves focus there.
        let press = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 50,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        scope.handle_event(&press).unwrap();
        assert_eq!(scope.focused_id, 1);

        // Hovering back over the first pane leaves focus untouched.
        let hover = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 5,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        scope.handle_event(&hover).unwrap();
        assert_eq!(scope.focused_id, 1);

        // A scroll over the first pane both forwards and focuses it.
        let scroll = Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        scope.handle_event(&scroll).unwrap();
        assert_eq!(scope.focused_id, 0);
    }

    #[test]
    fn new_tab_wraps_leaf_and_switch_tab_moves_active() {
        let cwd = std::env::temp_dir();
        let mut scope = Scope::with_terminals_in(
            1,
            Direction::Horizontal,
            Rect::new(0, 0, 80, 24),
            &cwd,
            (8, 16),
        )
        .unwrap();
        assert_eq!(scope.leaf_count(), 1);

        let spec = ScopeSpec::default_zsh(cwd);
        scope.new_tab_with_spec(&spec).unwrap();
        assert_eq!(scope.leaf_count(), 2);
        let initial_focus = scope.focused_id;

        // switching next should move to the new tab (the second page)
        assert!(scope.switch_tab(crate::protocol::TabDir::Prev));
        assert_ne!(scope.focused_id, initial_focus);
        assert!(scope.switch_tab(crate::protocol::TabDir::Next));
        assert_eq!(scope.focused_id, initial_focus);
    }

    #[test]
    fn new_tab_adds_page_to_existing_tab_group() {
        let cwd = std::env::temp_dir();
        let mut scope = Scope::with_terminals_in(
            1,
            Direction::Horizontal,
            Rect::new(0, 0, 80, 24),
            &cwd,
            (8, 16),
        )
        .unwrap();
        let spec = ScopeSpec::default_zsh(cwd.clone());
        scope.new_tab_with_spec(&spec).unwrap();
        assert_eq!(scope.leaf_count(), 2);
        let first_tab_focus = scope.focused_id;

        // now inside a tab group; another new tab adds a third page without wrapping
        scope.new_tab_with_spec(&spec).unwrap();
        assert_eq!(scope.leaf_count(), 3);
        assert_ne!(scope.focused_id, first_tab_focus);

        // snapshot should be a Tabs node with 3 pages
        let snap = scope.snapshot(1, "test".to_string()).unwrap();
        match snap.root {
            SnapshotNode::Tabs { pages, active } => {
                assert_eq!(pages.len(), 3);
                assert_eq!(active, 2);
            }
            other => panic!("expected Tabs node, got {other:?}"),
        }
    }
}
