use std::{io, path::PathBuf};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    prelude::Buffer,
    style::{Color, Style},
    widgets::{Block, Widget},
};

use crate::projections::{Mode, ProjectionStatus, ProjectionTerm};

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
    Status(ProjectionStatus),
    Scope(Box<Scope>),
}

impl Projection {
    fn is_exited(&self) -> bool {
        match self {
            Projection::Term(term) => term.is_exited(),
            Projection::Status(_) => false,
            Projection::Scope(scope) => scope.is_empty(),
        }
    }

    fn needs_border(&self) -> bool {
        matches!(self, Projection::Term(_))
    }

    fn resize(&mut self, rect: Rect) -> io::Result<()> {
        match self {
            Projection::Term(term) => {
                let inner = pane_inner(rect);
                term.resize(inner.width, inner.height)
            }
            Projection::Status(_) => Ok(()),
            Projection::Scope(scope) => scope.resize(rect),
        }
    }

    fn handle_event(&mut self, event: &Event) -> io::Result<()> {
        match self {
            Projection::Term(term) => term.handle_event(event),
            Projection::Status(_) => Ok(()),
            Projection::Scope(scope) => scope.handle_event(event),
        }
    }

    fn handle_mouse_event(&mut self, mouse_event: MouseEvent) -> io::Result<()> {
        match self {
            Projection::Term(term) => term.handle_mouse_event(mouse_event),
            Projection::Status(_) => Ok(()),
            Projection::Scope(scope) => scope.handle_event(&Event::Mouse(mouse_event)),
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        match self {
            Projection::Term(_) => {}
            Projection::Status(status) => status.set_mode(mode),
            Projection::Scope(scope) => scope.set_mode(mode),
        }
    }

    fn set_error(&mut self, error: Option<String>) {
        match self {
            Projection::Term(_) => {}
            Projection::Status(status) => status.set_error(error),
            Projection::Scope(scope) => scope.set_error(error),
        }
    }

    fn is_interactive(&self) -> bool {
        match self {
            Projection::Term(_) => true,
            Projection::Status(_) => false,
            Projection::Scope(scope) => scope.has_interactive(),
        }
    }

    fn prune_exited(&mut self) -> io::Result<bool> {
        match self {
            Projection::Term(_) | Projection::Status(_) => Ok(false),
            Projection::Scope(scope) => scope.prune_exited(),
        }
    }
}

impl Widget for &Projection {
    fn render(self, area: Rect, buf: &mut Buffer) {
        match self {
            Projection::Term(term) => term.render(area, buf),
            Projection::Status(status) => status.render(area, buf),
            Projection::Scope(scope) => scope.render(area, buf),
        }
    }
}

struct TileChild {
    constraint: Constraint,
    node: TileNode,
}

enum TileNode {
    Leaf { id: usize, projection: Projection },
    Split { direction: Direction, children: Vec<TileChild> },
}

impl TileNode {
    fn collect_rects(&self, area: Rect, out: &mut Vec<(usize, Rect, bool)>) {
        match self {
            TileNode::Leaf { id, projection } => {
                out.push((*id, area, projection.needs_border()));
            }
            TileNode::Split { direction, children } => {
                let constraints: Vec<Constraint> = children.iter().map(|c| c.constraint).collect();
                let rects = Layout::new(*direction, constraints).split(area);
                for (child, rect) in children.iter().zip(rects.iter()) {
                    child.node.collect_rects(*rect, out);
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
        }
    }

    fn first_leaf_id(&self) -> Option<usize> {
        match self {
            TileNode::Leaf { id, .. } => Some(*id),
            TileNode::Split { children, .. } => {
                children.iter().find_map(|c| c.node.first_leaf_id())
            }
        }
    }

    fn resize(&mut self, area: Rect) -> io::Result<()> {
        match self {
            TileNode::Leaf { projection, .. } => projection.resize(area),
            TileNode::Split { direction, children } => {
                let constraints: Vec<Constraint> = children.iter().map(|c| c.constraint).collect();
                let rects = Layout::new(*direction, constraints).split(area);
                for (child, rect) in children.iter_mut().zip(rects.iter()) {
                    child.node.resize(*rect)?;
                }
                Ok(())
            }
        }
    }

    fn render(&self, area: Rect, buf: &mut Buffer, focused_id: usize) {
        match self {
            TileNode::Leaf { id, projection } => {
                if projection.needs_border() {
                    let border_style = if *id == focused_id {
                        Style::default().fg(Color::White)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    let block = Block::bordered().border_style(border_style);
                    let inner = block.inner(area);
                    block.render(area, buf);
                    projection.render(inner, buf);
                } else {
                    projection.render(area, buf);
                }
            }
            TileNode::Split { direction, children } => {
                let constraints: Vec<Constraint> = children.iter().map(|c| c.constraint).collect();
                let rects = Layout::new(*direction, constraints).split(area);
                for (child, rect) in children.iter().zip(rects.iter()) {
                    child.node.render(*rect, buf, focused_id);
                }
            }
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        match self {
            TileNode::Leaf { projection, .. } => projection.set_mode(mode),
            TileNode::Split { children, .. } => {
                for child in children {
                    child.node.set_mode(mode);
                }
            }
        }
    }

    fn set_error(&mut self, error: Option<String>) {
        match self {
            TileNode::Leaf { projection, .. } => projection.set_error(error),
            TileNode::Split { children, .. } => {
                for child in children {
                    child.node.set_error(error.clone());
                }
            }
        }
    }

    fn has_interactive(&self) -> bool {
        match self {
            TileNode::Leaf { projection, .. } => projection.is_interactive(),
            TileNode::Split { children, .. } => children.iter().any(|c| c.node.has_interactive()),
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
                    let (first, second) = if dir.insert_before() {
                        (new_leaf, existing)
                    } else {
                        (existing, new_leaf)
                    };
                    *self = TileNode::Split {
                        direction: dir.axis(),
                        children: vec![
                            TileChild {
                                constraint: Constraint::Fill(1),
                                node: first,
                            },
                            TileChild {
                                constraint: Constraint::Fill(1),
                                node: second,
                            },
                        ],
                    };
                    Ok(())
                } else {
                    Err(new_projection)
                }
            }
            TileNode::Split { direction, children } => {
                if *direction == dir.axis() {
                    if let Some(pos) = children
                        .iter()
                        .position(|c| matches!(c.node, TileNode::Leaf { id, .. } if id == target_id))
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
        }

        Ok(changed)
    }
}

pub struct Scope {
    root: Option<TileNode>,
    focused_id: usize,
    next_id: usize,
    area: Rect,
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

        let focused_id = root.as_ref().and_then(|n| n.first_leaf_id()).unwrap_or(0);
        let mut scope = Self {
            root,
            focused_id,
            next_id,
            area,
        };
        scope.resize(area)?;
        Ok(scope)
    }

    pub fn with_terminals(count: usize, direction: Direction, area: Rect) -> Result<Self> {
        let constraints = vec![Constraint::Fill(1); count];
        let rects = Layout::new(direction, constraints).split(area);
        let mut children = Vec::with_capacity(count);
        for rect in rects.iter() {
            let inner = pane_inner(*rect);
            children.push((
                Constraint::Fill(1),
                Projection::Term(ProjectionTerm::with_size(inner.width, inner.height)?),
            ));
        }

        Ok(Self::from_children(direction, children, area)?)
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn has_interactive(&self) -> bool {
        self.root.as_ref().is_some_and(TileNode::has_interactive)
    }

    pub fn resize(&mut self, area: Rect) -> io::Result<()> {
        self.area = area;
        if let Some(root) = &mut self.root {
            root.resize(area)?;
        }
        Ok(())
    }

    pub fn set_mode(&mut self, mode: Mode) {
        if let Some(root) = &mut self.root {
            root.set_mode(mode);
        }
    }

    pub fn set_error(&mut self, error: Option<String>) {
        if let Some(root) = &mut self.root {
            root.set_error(error);
        }
    }

    pub fn prune_exited(&mut self) -> io::Result<bool> {
        let changed = TileNode::prune_exited(&mut self.root)?;
        if changed {
            if let Some(root) = &self.root {
                let mut rects = Vec::new();
                root.collect_rects(self.area, &mut rects);
                if !rects.iter().any(|(id, _, _)| *id == self.focused_id) {
                    if let Some((id, _, _)) = rects.first() {
                        self.focused_id = *id;
                    }
                }
            }
            self.resize(self.area)?;
        }
        Ok(changed)
    }

    pub fn move_focus(&mut self, dir: NavDirection) {
        let Some(root) = &self.root else {
            return;
        };
        let mut rects = Vec::new();
        root.collect_rects(self.area, &mut rects);

        let Some((_, current, _)) = rects.iter().find(|(id, _, _)| *id == self.focused_id).copied()
        else {
            return;
        };

        if let Some(next_id) = find_neighbor(&rects, self.focused_id, current, dir) {
            self.focused_id = next_id;
        }
    }

    pub fn split_focused(&mut self, dir: NavDirection) -> Result<()> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        self.split_focused_with_spec(dir, &ScopeSpec::default_zsh(cwd))
    }

    pub fn split_focused_with_spec(&mut self, dir: NavDirection, spec: &ScopeSpec) -> Result<()> {
        if let Some(root) = &mut self.root {
            if let Some(Projection::Scope(nested)) = root.find_mut(self.focused_id) {
                return nested.split_focused_with_spec(dir, spec);
            }
        }

        let Some(root) = &mut self.root else {
            let inner = pane_inner(self.area);
            let term = ProjectionTerm::with_command(
                inner.width,
                inner.height,
                &spec.program,
                &spec.cwd,
            )?;
            let id = self.next_id;
            self.next_id += 1;
            self.root = Some(TileNode::Leaf {
                id,
                projection: Projection::Term(term),
            });
            self.focused_id = id;
            return Ok(());
        };

        let new_id = self.next_id;
        self.next_id += 1;

        let placeholder =
            Projection::Term(ProjectionTerm::with_command(1, 1, &spec.program, &spec.cwd)?);
        if root
            .split_leaf(self.focused_id, dir, new_id, placeholder)
            .is_ok()
        {
            self.focused_id = new_id;
            self.resize(self.area)?;
        }

        Ok(())
    }

    pub fn handle_managing_key(&mut self, key_event: KeyEvent) -> Result<bool> {
        if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(false);
        }

        if let Some(root) = &mut self.root {
            if let Some(Projection::Scope(nested)) = root.find_mut(self.focused_id) {
                return nested.handle_managing_key(key_event);
            }
        }

        let ctrl = key_event.modifiers.contains(KeyModifiers::CONTROL);
        let dir = match key_event.code {
            KeyCode::Up => Some(NavDirection::Up),
            KeyCode::Down => Some(NavDirection::Down),
            KeyCode::Left => Some(NavDirection::Left),
            KeyCode::Right => Some(NavDirection::Right),
            _ => None,
        };

        if let Some(dir) = dir {
            if ctrl {
                self.split_focused(dir)?;
            } else {
                self.move_focus(dir);
            }
            return Ok(true);
        }

        Ok(false)
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
                    if target.contains(pos) {
                        let translated = MouseEvent {
                            column: mouse_event.column - target.x,
                            row: mouse_event.row - target.y,
                            ..*mouse_event
                        };
                        self.focused_id = id;
                        if let Some(proj) = root.find_mut(id) {
                            return proj.handle_mouse_event(translated);
                        }
                    }
                }
                Ok(())
            }
            _ => {
                if let Some(root) = &mut self.root {
                    if let Some(proj) = root.find_mut(self.focused_id) {
                        proj.handle_event(event)?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl Widget for &Scope {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if let Some(root) = &self.root {
            root.render(area, buf, self.focused_id);
        }
    }
}

fn pane_inner(rect: Rect) -> Rect {
    Block::bordered().inner(rect)
}

fn interval_distance(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    if a_end <= b_start {
        b_start - a_end
    } else if b_end <= a_start {
        a_start - b_end
    } else {
        0
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
        if best.map_or(true, |(_, best_score)| score < best_score) {
            best = Some((id, score));
        }
    }

    best.map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_neighbor_navigates_2d_grid() {
        let rects = vec![
            (0, Rect::new(0, 0, 50, 20), true),
            (1, Rect::new(0, 20, 50, 20), true),
            (2, Rect::new(50, 0, 50, 40), true),
        ];

        assert_eq!(find_neighbor(&rects, 0, rects[0].1, NavDirection::Down), Some(1));
        assert_eq!(find_neighbor(&rects, 1, rects[1].1, NavDirection::Up), Some(0));
        assert_eq!(find_neighbor(&rects, 0, rects[0].1, NavDirection::Right), Some(2));
        assert_eq!(find_neighbor(&rects, 1, rects[1].1, NavDirection::Right), Some(2));
        assert_eq!(find_neighbor(&rects, 2, rects[2].1, NavDirection::Left), Some(0));
        assert_eq!(find_neighbor(&rects, 0, rects[0].1, NavDirection::Left), None);
    }

    #[test]
    fn pane_inner_accounts_for_borders() {
        let rect = Rect::new(50, 0, 50, 40);
        assert_eq!(pane_inner(rect), Rect::new(51, 1, 48, 38));
    }
}
