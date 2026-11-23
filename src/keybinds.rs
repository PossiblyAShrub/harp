use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::{protocol::TabDir, scopes::NavDirection};

/// All actions that can be triggered from `Mode::Managing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Interact,
    Focus(NavDirection),
    SplitZsh(NavDirection),
    SplitPicker(NavDirection),
    MoveProjection(NavDirection),
    CloseProjection,
    NewTabZsh,
    NewTabPicker,
    SwitchTab(TabDir),
    OpenSwitcher,
    RenameScope,
    Detach,
    Paste,
    CopySelection,
    ScrollPage(i32),
}

/// How a key event matches a binding at the current chord level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTrigger {
    Exact(KeyCode, KeyModifiers),
    ExactOr(KeyCode, KeyCode, KeyModifiers),
    Arrows(KeyModifiers),
    PageNav,
}

/// A node in the hierarchical keybind tree.
#[derive(Debug, Clone, Copy)]
pub enum KeyNode {
    Action {
        label: &'static str,
        action: Action,
    },
    Arrows {
        label: &'static str,
        build: fn(NavDirection) -> Action,
    },
    PageNav {
        label: &'static str,
        build: fn(i32) -> Action,
    },
    Group {
        label: &'static str,
        children: &'static [Binding],
    },
}

/// A single keybind entry (either a leaf action or a chord prefix group).
#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub key: &'static str,
    pub trigger: KeyTrigger,
    pub node: KeyNode,
}

impl Binding {
    pub fn is_group(&self) -> bool {
        matches!(self.node, KeyNode::Group { .. })
    }

    pub fn label(&self) -> &'static str {
        match self.node {
            KeyNode::Action { label, .. }
            | KeyNode::Arrows { label, .. }
            | KeyNode::PageNav { label, .. }
            | KeyNode::Group { label, .. } => label,
        }
    }
}

fn split_zsh(dir: NavDirection) -> Action {
    Action::SplitZsh(dir)
}

fn split_picker(dir: NavDirection) -> Action {
    Action::SplitPicker(dir)
}

fn move_projection(dir: NavDirection) -> Action {
    Action::MoveProjection(dir)
}

fn focus_dir(dir: NavDirection) -> Action {
    Action::Focus(dir)
}

fn scroll_page(dir: i32) -> Action {
    Action::ScrollPage(dir)
}

const MOVE_BINDS: &[Binding] = &[Binding {
    key: "Arrows/hjkl",
    trigger: KeyTrigger::Arrows(KeyModifiers::NONE),
    node: KeyNode::Arrows {
        label: "move projection",
        build: move_projection,
    },
}];

const SPLIT_PICKER_BINDS: &[Binding] = &[Binding {
    key: "Arrows/hjkl",
    trigger: KeyTrigger::Arrows(KeyModifiers::NONE),
    node: KeyNode::Arrows {
        label: "split picker",
        build: split_picker,
    },
}];

const PROJECTION_BINDS: &[Binding] = &[
    Binding {
        key: "Arrows/hjkl",
        trigger: KeyTrigger::Arrows(KeyModifiers::NONE),
        node: KeyNode::Arrows {
            label: "split zsh",
            build: split_zsh,
        },
    },
    Binding {
        key: "Shift+Arrows",
        trigger: KeyTrigger::Arrows(KeyModifiers::SHIFT),
        node: KeyNode::Arrows {
            label: "split picker",
            build: split_picker,
        },
    },
    Binding {
        key: "n",
        trigger: KeyTrigger::Exact(KeyCode::Char('n'), KeyModifiers::NONE),
        node: KeyNode::Group {
            label: "split picker",
            children: SPLIT_PICKER_BINDS,
        },
    },
    Binding {
        key: "m",
        trigger: KeyTrigger::Exact(KeyCode::Char('m'), KeyModifiers::NONE),
        node: KeyNode::Group {
            label: "move",
            children: MOVE_BINDS,
        },
    },
    Binding {
        key: "q",
        trigger: KeyTrigger::Exact(KeyCode::Char('q'), KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "close",
            action: Action::CloseProjection,
        },
    },
];

const TAB_BINDS: &[Binding] = &[
    Binding {
        key: "t",
        trigger: KeyTrigger::Exact(KeyCode::Char('t'), KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "new tab",
            action: Action::NewTabZsh,
        },
    },
    Binding {
        key: "p",
        trigger: KeyTrigger::Exact(KeyCode::Char('p'), KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "new tab picker",
            action: Action::NewTabPicker,
        },
    },
    Binding {
        key: "Left/h",
        trigger: KeyTrigger::ExactOr(KeyCode::Left, KeyCode::Char('h'), KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "prev tab",
            action: Action::SwitchTab(TabDir::Prev),
        },
    },
    Binding {
        key: "Right/l",
        trigger: KeyTrigger::ExactOr(KeyCode::Right, KeyCode::Char('l'), KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "next tab",
            action: Action::SwitchTab(TabDir::Next),
        },
    },
];

const SCOPE_BINDS: &[Binding] = &[
    Binding {
        key: "s",
        trigger: KeyTrigger::Exact(KeyCode::Char('s'), KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "switch scope",
            action: Action::OpenSwitcher,
        },
    },
    Binding {
        key: "r",
        trigger: KeyTrigger::Exact(KeyCode::Char('r'), KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "rename scope",
            action: Action::RenameScope,
        },
    },
    Binding {
        key: "d",
        trigger: KeyTrigger::Exact(KeyCode::Char('d'), KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "detach",
            action: Action::Detach,
        },
    },
];

const CLOSE_BINDS: &[Binding] = &[Binding {
    key: "q",
    trigger: KeyTrigger::Exact(KeyCode::Char('q'), KeyModifiers::NONE),
    node: KeyNode::Action {
        label: "close projection",
        action: Action::CloseProjection,
    },
}];

/// Root keybindings for `Mode::Managing`.
pub const MANAGING_BINDS: &[Binding] = &[
    Binding {
        key: "Enter",
        trigger: KeyTrigger::Exact(KeyCode::Enter, KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "interact",
            action: Action::Interact,
        },
    },
    Binding {
        key: "Arrows/hjkl",
        trigger: KeyTrigger::Arrows(KeyModifiers::NONE),
        node: KeyNode::Arrows {
            label: "focus",
            build: focus_dir,
        },
    },
    Binding {
        key: "Ctrl+Arrows",
        trigger: KeyTrigger::Arrows(KeyModifiers::CONTROL),
        node: KeyNode::Arrows {
            label: "split zsh",
            build: split_zsh,
        },
    },
    Binding {
        key: "p",
        trigger: KeyTrigger::Exact(KeyCode::Char('p'), KeyModifiers::NONE),
        node: KeyNode::Group {
            label: "projection",
            children: PROJECTION_BINDS,
        },
    },
    Binding {
        key: "t",
        trigger: KeyTrigger::Exact(KeyCode::Char('t'), KeyModifiers::NONE),
        node: KeyNode::Group {
            label: "tab",
            children: TAB_BINDS,
        },
    },
    Binding {
        key: "s",
        trigger: KeyTrigger::Exact(KeyCode::Char('s'), KeyModifiers::NONE),
        node: KeyNode::Group {
            label: "scope",
            children: SCOPE_BINDS,
        },
    },
    Binding {
        key: "v",
        trigger: KeyTrigger::Exact(KeyCode::Char('v'), KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "paste",
            action: Action::Paste,
        },
    },
    Binding {
        key: "y",
        trigger: KeyTrigger::Exact(KeyCode::Char('y'), KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "copy",
            action: Action::CopySelection,
        },
    },
    Binding {
        key: "PgUp/Dn",
        trigger: KeyTrigger::PageNav,
        node: KeyNode::PageNav {
            label: "scroll",
            build: scroll_page,
        },
    },
    Binding {
        key: "d",
        trigger: KeyTrigger::Exact(KeyCode::Char('d'), KeyModifiers::NONE),
        node: KeyNode::Action {
            label: "detach",
            action: Action::Detach,
        },
    },
    Binding {
        key: "q",
        trigger: KeyTrigger::Exact(KeyCode::Char('q'), KeyModifiers::NONE),
        node: KeyNode::Group {
            label: "close",
            children: CLOSE_BINDS,
        },
    },
];

/// Result of feeding a `KeyEvent` to `KeybindState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    Execute(Action),
    EnteredGroup,
    SteppedBack,
    Cancelled,
    Unbound,
    Unhandled,
}

/// Tracks the active chord path inside `MANAGING_BINDS`.
#[derive(Debug, Default, Clone)]
pub struct KeybindState {
    path: Vec<usize>,
}

impl KeybindState {
    pub fn new() -> Self {
        Self { path: Vec::new() }
    }

    pub fn reset(&mut self) {
        self.path.clear();
    }

    pub fn is_in_chord(&self) -> bool {
        !self.path.is_empty()
    }

    pub fn current_bindings(&self) -> &'static [Binding] {
        let mut slice = MANAGING_BINDS;
        for &idx in &self.path {
            match slice.get(idx).map(|b| b.node) {
                Some(KeyNode::Group { children, .. }) => slice = children,
                _ => return MANAGING_BINDS,
            }
        }
        slice
    }

    pub fn breadcrumbs(&self) -> Vec<&'static str> {
        let mut crumbs = Vec::with_capacity(self.path.len());
        let mut slice = MANAGING_BINDS;
        for &idx in &self.path {
            if let Some(binding) = slice.get(idx)
                && let KeyNode::Group { label, children } = binding.node
            {
                crumbs.push(label);
                slice = children;
            }
        }
        crumbs
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        if self.is_in_chord() {
            if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
                self.reset();
                return KeyOutcome::Cancelled;
            }
            if key.code == KeyCode::Backspace && key.modifiers == KeyModifiers::NONE {
                self.path.pop();
                return KeyOutcome::SteppedBack;
            }
        }

        let bindings = self.current_bindings();
        for (idx, binding) in bindings.iter().enumerate() {
            if let Some(outcome) = match_binding(binding, key) {
                return match outcome {
                    Matched::Group => {
                        self.path.push(idx);
                        KeyOutcome::EnteredGroup
                    }
                    Matched::Action(action) => {
                        self.reset();
                        KeyOutcome::Execute(action)
                    }
                };
            }
        }

        if self.is_in_chord() {
            self.reset();
            KeyOutcome::Unbound
        } else {
            KeyOutcome::Unhandled
        }
    }
}

enum Matched {
    Group,
    Action(Action),
}

fn match_binding(binding: &Binding, key: KeyEvent) -> Option<Matched> {
    match (binding.trigger, binding.node) {
        (KeyTrigger::Exact(code, mods), KeyNode::Action { action, .. }) => {
            if key.code == code && normalize_modifiers(key) == mods {
                Some(Matched::Action(action))
            } else {
                None
            }
        }
        (KeyTrigger::ExactOr(c1, c2, mods), KeyNode::Action { action, .. }) => {
            if (key.code == c1 || key.code == c2) && normalize_modifiers(key) == mods {
                Some(Matched::Action(action))
            } else {
                None
            }
        }
        (KeyTrigger::Exact(code, mods), KeyNode::Group { .. }) => {
            if key.code == code && normalize_modifiers(key) == mods {
                Some(Matched::Group)
            } else {
                None
            }
        }
        (KeyTrigger::ExactOr(c1, c2, mods), KeyNode::Group { .. }) => {
            if (key.code == c1 || key.code == c2) && normalize_modifiers(key) == mods {
                Some(Matched::Group)
            } else {
                None
            }
        }
        (KeyTrigger::Arrows(mods), KeyNode::Arrows { build, .. }) => {
            nav_direction(key, mods).map(|dir| Matched::Action(build(dir)))
        }
        (KeyTrigger::PageNav, KeyNode::PageNav { build, .. }) => {
            if key.modifiers == KeyModifiers::NONE {
                match key.code {
                    KeyCode::PageUp => Some(Matched::Action(build(1))),
                    KeyCode::PageDown => Some(Matched::Action(build(-1))),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn normalize_modifiers(key: KeyEvent) -> KeyModifiers {
    // Strip SHIFT for Char keys where the character itself encodes shift (or Enter).
    if matches!(key.code, KeyCode::Char(_) | KeyCode::Enter) {
        key.modifiers - KeyModifiers::SHIFT
    } else {
        key.modifiers
    }
}

fn nav_direction(key: KeyEvent, mods: KeyModifiers) -> Option<NavDirection> {
    if key.modifiers == mods
        && let Some(dir) = match key.code {
            KeyCode::Up => Some(NavDirection::Up),
            KeyCode::Down => Some(NavDirection::Down),
            KeyCode::Left => Some(NavDirection::Left),
            KeyCode::Right => Some(NavDirection::Right),
            _ => None,
        }
    {
        return Some(dir);
    }

    if mods == KeyModifiers::SHIFT {
        if normalize_modifiers(key) == KeyModifiers::NONE {
            return match key.code {
                KeyCode::Char('H') => Some(NavDirection::Left),
                KeyCode::Char('J') => Some(NavDirection::Down),
                KeyCode::Char('K') => Some(NavDirection::Up),
                KeyCode::Char('L') => Some(NavDirection::Right),
                _ => None,
            };
        }
        return None;
    }

    if key.modifiers == mods {
        return match key.code {
            KeyCode::Char('h') => Some(NavDirection::Left),
            KeyCode::Char('j') => Some(NavDirection::Down),
            KeyCode::Char('k') => Some(NavDirection::Up),
            KeyCode::Char('l') => Some(NavDirection::Right),
            _ => None,
        };
    }

    None
}

/// Formats a list of `Binding`s into aligned multi-row which-key `Line`s that fit `width`.
pub fn build_which_key_lines(bindings: &[Binding], width: u16) -> Vec<Line<'static>> {
    if bindings.is_empty() || width == 0 {
        return vec![Line::default()];
    }

    let items: Vec<(String, String, bool)> = bindings
        .iter()
        .map(|b| {
            let label = if b.is_group() {
                format!("+{}", b.label())
            } else {
                b.label().to_string()
            };
            (b.key.to_string(), label, b.is_group())
        })
        .collect();

    // Each item displays as " <key> <label> "
    let max_item_width = items
        .iter()
        .map(|(k, l, _)| k.chars().count() + 1 + l.chars().count())
        .max()
        .unwrap_or(1);

    let col_gap = 2usize;
    let cell_width = (max_item_width + col_gap).min(width as usize).max(1);
    let num_cols = ((width as usize + col_gap) / cell_width).max(1).min(items.len());

    let mut lines = Vec::new();
    for chunk in items.chunks(num_cols) {
        let mut spans = Vec::with_capacity(chunk.len() * 4);
        for (col_idx, (key, label, is_group)) in chunk.iter().enumerate() {
            if col_idx == 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                key.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
            let label_style = if *is_group {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            spans.push(Span::styled(label.clone(), label_style));

            if col_idx + 1 < chunk.len() {
                let used = key.chars().count() + 1 + label.chars().count();
                let pad = cell_width.saturating_sub(used);
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                }
            }
        }
        lines.push(Line::from(spans));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    #[test]
    fn root_actions_execute_immediately() {
        let mut state = KeybindState::new();
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            KeyOutcome::Execute(Action::Interact)
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Left)),
            KeyOutcome::Execute(Action::Focus(NavDirection::Left))
        );
        assert!(!state.is_in_chord());
    }

    #[test]
    fn chord_enters_group_and_executes_nested_action() {
        let mut state = KeybindState::new();
        assert_eq!(
            state.handle_key(key(KeyCode::Char('p'))),
            KeyOutcome::EnteredGroup
        );
        assert!(state.is_in_chord());
        assert_eq!(state.breadcrumbs(), vec!["projection"]);

        assert_eq!(
            state.handle_key(key(KeyCode::Char('m'))),
            KeyOutcome::EnteredGroup
        );
        assert_eq!(state.breadcrumbs(), vec!["projection", "move"]);

        assert_eq!(
            state.handle_key(key(KeyCode::Right)),
            KeyOutcome::Execute(Action::MoveProjection(NavDirection::Right))
        );
        assert!(!state.is_in_chord());
    }

    #[test]
    fn shift_arrows_inside_projection_group_triggers_split_picker() {
        let mut state = KeybindState::new();
        assert_eq!(
            state.handle_key(key(KeyCode::Char('p'))),
            KeyOutcome::EnteredGroup
        );
        assert_eq!(
            state.handle_key(shift_key(KeyCode::Down)),
            KeyOutcome::Execute(Action::SplitPicker(NavDirection::Down))
        );
    }

    #[test]
    fn esc_cancels_and_backspace_steps_back() {
        let mut state = KeybindState::new();
        state.handle_key(key(KeyCode::Char('p')));
        state.handle_key(key(KeyCode::Char('m')));
        assert_eq!(state.breadcrumbs(), vec!["projection", "move"]);

        assert_eq!(state.handle_key(key(KeyCode::Backspace)), KeyOutcome::SteppedBack);
        assert_eq!(state.breadcrumbs(), vec!["projection"]);

        assert_eq!(state.handle_key(key(KeyCode::Esc)), KeyOutcome::Cancelled);
        assert!(!state.is_in_chord());
    }

    #[test]
    fn hjkl_and_ctrl_arrows_work_as_aliases() {
        let mut state = KeybindState::new();
        assert_eq!(
            state.handle_key(key(KeyCode::Char('h'))),
            KeyOutcome::Execute(Action::Focus(NavDirection::Left))
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('j'))),
            KeyOutcome::Execute(Action::Focus(NavDirection::Down))
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)),
            KeyOutcome::Execute(Action::SplitZsh(NavDirection::Right))
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            KeyOutcome::Execute(Action::SplitZsh(NavDirection::Up))
        );

        state.handle_key(key(KeyCode::Char('t')));
        assert_eq!(
            state.handle_key(key(KeyCode::Char('l'))),
            KeyOutcome::Execute(Action::SwitchTab(TabDir::Next))
        );
    }

    #[test]
    fn which_key_wraps_into_multiple_rows_for_narrow_widths() {
        let narrow = build_which_key_lines(MANAGING_BINDS, 45);
        assert!(narrow.len() >= 3, "expected >=3 rows, got {}", narrow.len());

        let wide = build_which_key_lines(MANAGING_BINDS, 160);
        assert!(wide.len() <= narrow.len());
    }
}
