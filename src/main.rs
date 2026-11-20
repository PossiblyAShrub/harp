use std::{collections::HashMap, io, io::Write, path::PathBuf, time::Duration};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Rect},
    prelude::Buffer,
    widgets::Widget,
};
use tui_term::vt100;

mod clipboard;
mod config;
mod copymode;
mod crypto;
mod picker;
mod projections;
mod protocol;
mod scopes;
mod server;
mod switcher;
mod view;

use copymode::{CopyAction, CopyMode};
use crypto::EncryptedStream;
use picker::{PickerAction, ScopePicker};
use projections::{Mode, ProjectionStatus};
use protocol::{
    ClientMsg, NavDir, RemoteScopeSpec, ScopeSnapshot, ServerMsg, SnapshotNode, TabDir,
};
use scopes::NavDirection;
use switcher::{RenameAction, RenameModal, ScopeSwitcher, SwitcherAction};
use view::SnapshotView;

struct App {
    client: EncryptedStream,
    mode: Mode,
    status: ProjectionStatus,
    default_cwd: PathBuf,
    workspace_area: Rect,
    snapshot: Option<ScopeSnapshot>,
    parsers: HashMap<usize, vt100::Parser>,
    picker: Option<ScopePicker>,
    switcher: Option<ScopeSwitcher>,
    rename: Option<RenameModal>,
    copy_mode: Option<CopyMode>,
    exit: bool,
}

impl App {
    fn new(client: EncryptedStream, area: Rect) -> Result<Self> {
        let mode = Mode::Managing;
        let default_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let workspace_area = Rect::new(
            area.x,
            area.y,
            area.width.max(1),
            area.height.saturating_sub(1).max(1),
        );

        let mut app = Self {
            client,
            mode,
            status: ProjectionStatus::new(mode),
            default_cwd: default_cwd.clone(),
            workspace_area,
            snapshot: None,
            parsers: HashMap::new(),
            picker: None,
            switcher: None,
            rename: None,
            copy_mode: None,
            exit: false,
        };

        app.client.send(&ClientMsg::ListScopes)?;
        match app.client.recv::<ServerMsg>()? {
            ServerMsg::Scopes(scopes) if scopes.is_empty() => {
                app.create_new_scope()?;
            }
            ServerMsg::Scopes(scopes) => {
                app.switcher = Some(ScopeSwitcher::new(scopes, 0, false));
            }
            ServerMsg::Error(e) => {
                app.status.set_error(Some(e));
            }
            _ => {}
        }

        Ok(app)
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.status.set_mode(mode);
    }

    fn apply_snapshot(&mut self, snap: ScopeSnapshot) {
        let mut active_ids = Vec::new();
        self.sync_parsers(&snap.root, &mut active_ids);
        self.parsers.retain(|id, _| active_ids.contains(id));
        self.status.set_scope_name(Some(snap.scope_name.clone()));
        self.snapshot = Some(snap);
    }

    fn sync_parsers(&mut self, node: &SnapshotNode, active_ids: &mut Vec<usize>) {
        match node {
            SnapshotNode::Leaf {
                id,
                cols,
                rows,
                state,
            } => {
                active_ids.push(*id);
                let mut parser = vt100::Parser::new((*rows).max(1), (*cols).max(1), 0);
                parser.process(state);
                self.parsers.insert(*id, parser);
            }
            SnapshotNode::Split { children, .. } => {
                for child in children {
                    self.sync_parsers(&child.node, active_ids);
                }
            }
            SnapshotNode::Tabs { pages, .. } => {
                for page in pages {
                    active_ids.push(page.id);
                    let mut parser = vt100::Parser::new(page.rows.max(1), page.cols.max(1), 0);
                    parser.process(&page.state);
                    self.parsers.insert(page.id, parser);
                }
            }
        }
    }

    fn handle_server_reply(&mut self, reply: ServerMsg) -> io::Result<()> {
        match reply {
            ServerMsg::Snapshot(snap) => {
                self.status.set_error(None);
                self.apply_snapshot(snap);
            }
            ServerMsg::Unchanged => {}
            ServerMsg::ScopeClosed => {
                self.snapshot = None;
                self.status.set_scope_name(None);
                self.copy_mode = None;
                self.parsers.clear();
                self.client.send(&ClientMsg::ListScopes)?;
                match self.client.recv::<ServerMsg>()? {
                    ServerMsg::Scopes(scopes) if !scopes.is_empty() => {
                        self.set_mode(Mode::Managing);
                        self.switcher = Some(ScopeSwitcher::new(scopes, 0, false));
                    }
                    _ => {
                        self.exit = true;
                    }
                }
            }
            ServerMsg::Error(e) => {
                self.status.set_error(Some(e));
            }
            ServerMsg::Scopes(_) => {}
        }
        Ok(())
    }

    fn create_new_scope(&mut self) -> io::Result<()> {
        self.client.send(&ClientMsg::CreateScope {
            cwd: self.default_cwd.clone(),
            cols: self.workspace_area.width,
            rows: self.workspace_area.height,
        })?;
        let reply = self.client.recv::<ServerMsg>()?;
        self.handle_server_reply(reply)
    }

    fn attach_scope(&mut self, scope_id: u64) -> io::Result<()> {
        self.client.send(&ClientMsg::AttachScope {
            scope_id,
            cols: self.workspace_area.width,
            rows: self.workspace_area.height,
        })?;
        let reply = self.client.recv::<ServerMsg>()?;
        self.handle_server_reply(reply)
    }

    fn open_switcher(&mut self, allow_cancel: bool) -> io::Result<()> {
        self.client.send(&ClientMsg::ListScopes)?;
        match self.client.recv::<ServerMsg>()? {
            ServerMsg::Scopes(scopes) => {
                let scope_id = self.snapshot.as_ref().map(|s| s.scope_id);
                let scope_num = scopes
                    .iter()
                    .enumerate()
                    .find(|(_, s)| Some(s.id) == scope_id)
                    .map(|(num, _)| num)
                    .unwrap_or(0);
                self.switcher = Some(ScopeSwitcher::new(scopes, scope_num, allow_cancel));
            }
            ServerMsg::Error(e) => self.status.set_error(Some(e)),
            _ => {}
        }
        Ok(())
    }

    /// True when a key event is the `Ctrl+Shift+C` copy-mode shortcut.
    fn is_copy_shortcut(key_event: &KeyEvent) -> bool {
        key_event.modifiers.contains(KeyModifiers::CONTROL)
            && key_event.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key_event.code, KeyCode::Char('c') | KeyCode::Char('C'))
    }

    /// True when a key event is the `Ctrl+Shift+V` paste shortcut.
    fn is_paste_shortcut(key_event: &KeyEvent) -> bool {
        key_event.modifiers.contains(KeyModifiers::CONTROL)
            && key_event.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key_event.code, KeyCode::Char('v') | KeyCode::Char('V'))
    }

    /// Enters copy mode over the currently focused projection, if any.
    fn enter_copy_mode(&mut self) {
        if self.copy_mode.is_some() {
            return;
        }
        let Some(snap) = &self.snapshot else {
            return;
        };
        let Some(parser) = self.parsers.get(&snap.focused_id) else {
            return;
        };
        if let Some(mode) = CopyMode::new(snap.focused_id, parser) {
            self.copy_mode = Some(mode);
        }
    }

    /// Pastes the OS clipboard into the focused projection.
    ///
    /// Reading the clipboard keeps the paste entirely inside harp, so the
    /// terminal emulator's "sanitize or not" confirmation dialog never shows
    /// up. The text is sanitized before being injected, mirroring kitty's
    /// sanitize paste action.
    fn paste_from_clipboard(&mut self) -> io::Result<()> {
        let Some(text) = clipboard::clipboard_get().map(|t| clipboard::sanitize_for_paste(&t))
        else {
            return Ok(());
        };
        if text.is_empty() {
            return Ok(());
        }
        let Some(snap) = &self.snapshot else {
            return Ok(());
        };
        self.client.send(&ClientMsg::Input {
            scope_id: snap.scope_id,
            event: Event::Paste(text),
        })?;
        let reply = self.client.recv::<ServerMsg>()?;
        self.handle_server_reply(reply)
    }

    /// Event handling while copy mode is active. Copy mode owns all input
    /// until the user copies, quits, or jumps back to managing mode.
    fn handle_copy_mode_event(&mut self, event: &Event) -> io::Result<()> {
        if let Event::Resize(cols, rows) = event {
            self.workspace_area = Rect::new(0, 0, (*cols).max(1), rows.saturating_sub(1).max(1));
            return Ok(());
        }

        let mut leave = CopyLeave::Stay;
        let mut copied: Option<String> = None;

        if let Some(mode) = &mut self.copy_mode
            && let Event::Key(key) = event
            && key.kind != KeyEventKind::Release
        {
            if key.code == KeyCode::Char(' ')
                && key.kind == KeyEventKind::Press
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                leave = CopyLeave::Manage;
            } else {
                match mode.handle_key(*key) {
                    CopyAction::None => {}
                    CopyAction::Exit => leave = CopyLeave::Exit,
                    CopyAction::CopyAndExit => copied = Some(mode.copy_text()),
                }
            }
        }

        if let Some(text) = copied {
            clipboard::clipboard_set(&text);
        }

        match leave {
            CopyLeave::Stay => {}
            CopyLeave::Exit => self.copy_mode = None,
            CopyLeave::Manage => {
                self.copy_mode = None;
                self.set_mode(Mode::Managing);
            }
        }
        Ok(())
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.exit {
            terminal.draw(|frame| self.render(frame))?;

            if event::poll(Duration::from_millis(16))? {
                self.handle_events()?;
            } else if !self.exit
                && let Some(snap) = &self.snapshot
            {
                self.client.send(&ClientMsg::Poll {
                    scope_id: snap.scope_id,
                    last_revision: snap.revision,
                })?;
                let reply = self.client.recv::<ServerMsg>()?;
                self.handle_server_reply(reply)?;
            }
        }

        Ok(())
    }

    fn render(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_events(&mut self) -> io::Result<()> {
        loop {
            let event = event::read()?;

            if self.copy_mode.is_some() {
                self.handle_copy_mode_event(&event)?;
            } else if let Event::Resize(cols, rows) = event {
                self.workspace_area = Rect::new(0, 0, cols.max(1), rows.saturating_sub(1).max(1));
                if let Some(snap) = &self.snapshot {
                    self.client.send(&ClientMsg::Resize {
                        scope_id: snap.scope_id,
                        cols: self.workspace_area.width,
                        rows: self.workspace_area.height,
                    })?;
                    let reply = self.client.recv::<ServerMsg>()?;
                    self.handle_server_reply(reply)?;
                }
            } else if let Some(switcher) = &mut self.switcher {
                if let Event::Key(key_event) = event {
                    match switcher.handle_key(key_event) {
                        SwitcherAction::None => {}
                        SwitcherAction::Cancel => {
                            if self.snapshot.is_some() {
                                self.switcher = None;
                            } else {
                                self.exit = true;
                            }
                        }
                        SwitcherAction::Attach(id) => {
                            self.switcher = None;
                            self.attach_scope(id)?;
                        }
                        SwitcherAction::CreateNew => {
                            self.switcher = None;
                            self.create_new_scope()?;
                        }
                    }
                }
            } else if let Some(rename) = &mut self.rename {
                if let Event::Key(key_event) = event {
                    match rename.handle_key(key_event) {
                        RenameAction::None => {}
                        RenameAction::Cancel => {
                            self.rename = None;
                        }
                        RenameAction::Confirm(name) => {
                            self.rename = None;
                            if let Some(snap) = &self.snapshot {
                                self.client.send(&ClientMsg::RenameScope {
                                    scope_id: snap.scope_id,
                                    name,
                                })?;
                                let reply = self.client.recv::<ServerMsg>()?;
                                self.handle_server_reply(reply)?;
                            }
                        }
                    }
                }
            } else if let Some(picker) = &mut self.picker {
                if let Event::Key(key_event) = event {
                    match picker.handle_key(key_event) {
                        PickerAction::None => {}
                        PickerAction::Cancel => {
                            self.picker = None;
                        }
                        PickerAction::Confirm { direction, spec } => {
                            if let Some(snap) = &self.snapshot {
                                self.client.send(&ClientMsg::Split {
                                    scope_id: snap.scope_id,
                                    dir: to_nav_dir(direction),
                                    spec: RemoteScopeSpec {
                                        machine: spec.machine,
                                        cwd: spec.cwd,
                                        program: spec.program,
                                    },
                                })?;
                                match self.client.recv::<ServerMsg>()? {
                                    ServerMsg::Error(err) => {
                                        picker.set_error(Some(format!("Failed to launch: {err}")));
                                    }
                                    reply => {
                                        self.picker = None;
                                        self.handle_server_reply(reply)?;
                                    }
                                }
                            }
                        }
                        PickerAction::ConfirmTab { spec } => {
                            if let Some(snap) = &self.snapshot {
                                self.client.send(&ClientMsg::NewTab {
                                    scope_id: snap.scope_id,
                                    spec: RemoteScopeSpec {
                                        machine: spec.machine,
                                        cwd: spec.cwd,
                                        program: spec.program,
                                    },
                                })?;
                                match self.client.recv::<ServerMsg>()? {
                                    ServerMsg::Error(err) => {
                                        picker.set_error(Some(format!("Failed to launch: {err}")));
                                    }
                                    reply => {
                                        self.picker = None;
                                        self.handle_server_reply(reply)?;
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                match self.mode {
                    Mode::Interacting => match &event {
                        Event::Key(key_event)
                            if matches!(
                                key_event.kind,
                                KeyEventKind::Press | KeyEventKind::Repeat
                            ) && key_event.code == KeyCode::Char(' ')
                                && key_event.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            self.set_mode(Mode::Managing);
                        }
                        Event::Key(key_event)
                            if key_event.kind == KeyEventKind::Press
                                && Self::is_paste_shortcut(key_event) =>
                        {
                            self.paste_from_clipboard()?;
                        }
                        Event::Key(key_event)
                            if key_event.kind == KeyEventKind::Press
                                && Self::is_copy_shortcut(key_event) =>
                        {
                            self.enter_copy_mode();
                        }
                        _ => {
                            let ignore = matches!(
                                &event,
                                Event::Key(k) if k.kind == KeyEventKind::Release
                            );
                            if !ignore && let Some(snap) = &self.snapshot {
                                self.client.send(&ClientMsg::Input {
                                    scope_id: snap.scope_id,
                                    event,
                                })?;
                                let reply = self.client.recv::<ServerMsg>()?;
                                self.handle_server_reply(reply)?;
                            }
                        }
                    },
                    Mode::Managing => match &event {
                        Event::Key(key_event)
                            if matches!(
                                key_event.kind,
                                KeyEventKind::Press | KeyEventKind::Repeat
                            ) =>
                        {
                            self.status.set_error(None);
                            if key_event.kind == KeyEventKind::Press
                                && Self::is_copy_shortcut(key_event)
                            {
                                self.enter_copy_mode();
                            } else if key_event.kind == KeyEventKind::Press
                                && Self::is_paste_shortcut(key_event)
                            {
                                self.paste_from_clipboard()?;
                            } else if key_event.code == KeyCode::Enter
                                && !key_event
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            {
                                self.set_mode(Mode::Interacting);
                            } else if key_event.code == KeyCode::Char('q')
                                && key_event.modifiers == KeyModifiers::NONE
                            {
                                self.exit = true;
                            } else if key_event.code == KeyCode::Char('s')
                                && key_event.modifiers == KeyModifiers::NONE
                            {
                                self.open_switcher(true)?;
                            } else if key_event.code == KeyCode::Char('r')
                                && key_event.modifiers == KeyModifiers::NONE
                            {
                                if let Some(snap) = &self.snapshot {
                                    self.rename = Some(RenameModal::new(snap.scope_name.clone()));
                                }
                            } else if key_event.code == KeyCode::Char('T')
                                && !key_event
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            {
                                self.picker = Some(ScopePicker::for_tab(self.default_cwd.clone()));
                            } else if key_event.code == KeyCode::Char('t')
                                && key_event.modifiers == KeyModifiers::NONE
                            {
                                if let Some(snap) = &self.snapshot {
                                    self.client.send(&ClientMsg::NewTab {
                                        scope_id: snap.scope_id,
                                        spec: RemoteScopeSpec {
                                            machine: "localhost".to_string(),
                                            cwd: self.default_cwd.clone(),
                                            program: "zsh".to_string(),
                                        },
                                    })?;
                                    let reply = self.client.recv::<ServerMsg>()?;
                                    self.handle_server_reply(reply)?;
                                }
                            } else if let Some(dir) = arrow_direction(key_event.code) {
                                if key_event.modifiers.contains(KeyModifiers::ALT) {
                                    if let Some(snap) = &self.snapshot {
                                        let tab_dir = match dir {
                                            NavDirection::Left | NavDirection::Up => TabDir::Prev,
                                            NavDirection::Right | NavDirection::Down => {
                                                TabDir::Next
                                            }
                                        };
                                        self.client.send(&ClientMsg::SwitchTab {
                                            scope_id: snap.scope_id,
                                            dir: tab_dir,
                                        })?;
                                        let reply = self.client.recv::<ServerMsg>()?;
                                        self.handle_server_reply(reply)?;
                                    }
                                } else if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                                    self.picker =
                                        Some(ScopePicker::new(dir, self.default_cwd.clone()));
                                } else if let Some(snap) = &self.snapshot {
                                    if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                                        self.client.send(&ClientMsg::Split {
                                            scope_id: snap.scope_id,
                                            dir: to_nav_dir(dir),
                                            spec: RemoteScopeSpec {
                                                machine: "localhost".to_string(),
                                                cwd: self.default_cwd.clone(),
                                                program: "zsh".to_string(),
                                            },
                                        })?;
                                    } else {
                                        self.client.send(&ClientMsg::MoveFocus {
                                            scope_id: snap.scope_id,
                                            dir: to_nav_dir(dir),
                                        })?;
                                    }
                                    let reply = self.client.recv::<ServerMsg>()?;
                                    self.handle_server_reply(reply)?;
                                }
                            }
                        }
                        Event::Mouse(_) => {
                            if let Some(snap) = &self.snapshot {
                                self.client.send(&ClientMsg::Input {
                                    scope_id: snap.scope_id,
                                    event,
                                })?;
                                let reply = self.client.recv::<ServerMsg>()?;
                                self.handle_server_reply(reply)?;
                            }
                        }
                        _ => {}
                    },
                }
            }

            if self.exit || !event::poll(Duration::ZERO)? {
                break;
            }
        }

        Ok(())
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let chunks = Layout::new(
            Direction::Vertical,
            [Constraint::Fill(1), Constraint::Length(1)],
        )
        .split(area);

        if let Some(snap) = &self.snapshot {
            SnapshotView::new(snap, &self.parsers).render(chunks[0], buf);
        }
        self.status.render(chunks[1], buf);

        if let Some(copy_mode) = &self.copy_mode {
            copy_mode.render(chunks[0], buf);
        }

        if let Some(picker) = &self.picker {
            picker.render(area, buf);
        }
        if let Some(switcher) = &self.switcher {
            switcher.render(area, buf);
        }
        if let Some(rename) = &self.rename {
            rename.render(area, buf);
        }
    }
}

/// How copy mode should end after handling an event.
enum CopyLeave {
    Stay,
    Exit,
    Manage,
}

fn arrow_direction(code: KeyCode) -> Option<NavDirection> {
    match code {
        KeyCode::Up => Some(NavDirection::Up),
        KeyCode::Down => Some(NavDirection::Down),
        KeyCode::Left => Some(NavDirection::Left),
        KeyCode::Right => Some(NavDirection::Right),
        _ => None,
    }
}

fn to_nav_dir(dir: NavDirection) -> NavDir {
    match dir {
        NavDirection::Up => NavDir::Up,
        NavDirection::Down => NavDir::Down,
        NavDirection::Left => NavDir::Left,
        NavDirection::Right => NavDir::Right,
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut serve = false;
    let mut addr = server::DEFAULT_ADDR.to_string();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--serve" => serve = true,
            "--bind" | "--connect" => {
                if let Some(val) = args.get(i + 1) {
                    addr = val.clone();
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    if serve {
        return server::run_server(&addr);
    }

    let client = server::connect_or_spawn(&addr)?;
    ratatui::run(|terminal| {
        // Without these sequences the terminal never reports mouse events, so
        // nothing could be forwarded to projections. The guard restores mouse
        // reporting on both normal and error/panic exits.
        execute!(io::stdout(), EnableMouseCapture).map_err(io::Error::other)?;
        let _capture = MouseCaptureGuard;

        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let mut app = App::new(client, area).map_err(io::Error::other)?;
        app.run(terminal)
    })?;

    Ok(())
}

/// Disables terminal mouse capture when dropped so that panics and early
/// returns leave the shell with normal mouse reporting restored.
struct MouseCaptureGuard;

impl Drop for MouseCaptureGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = io::stdout().flush();
    }
}
