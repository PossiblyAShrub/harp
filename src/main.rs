use std::{
    collections::HashMap,
    io,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Position, Rect},
    prelude::Buffer,
    widgets::Widget,
};
use tui_term::vt100;

mod clipboard;
mod config;
mod copymode;
mod crypto;
mod debug;
mod picker;
mod projections;
mod protocol;
mod scopes;
mod server;
mod switcher;
mod view;

use copymode::{CopyAction, CopyMode};
#[cfg(test)]
use crypto::EncryptedStream;
use picker::{PickerAction, ScopePicker};
use projections::{Mode, ProjectionStatus, SCROLLBACK_LINES};
use protocol::{
    ClientMsg, NavDir, RemoteScopeSpec, ScopeSnapshot, ServerMsg, SnapshotNode, TabDir,
};
use scopes::NavDirection;
use switcher::{RenameAction, RenameModal, ScopeSwitcher, SwitcherAction};
use view::{SnapshotView, pane_at, pane_content_rect};

struct App {
    client: server::ClientConnection,
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
    /// Per-projection scrollback view offset (rows above the bottom).
    scroll: HashMap<usize, usize>,
    /// Previous local left click, used to recognize a double-click.
    last_click: Option<(Instant, usize, u16, u16)>,
    exit: bool,
}

impl App {
    fn new(client: server::ClientConnection, area: Rect) -> Result<Self> {
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
            scroll: HashMap::new(),
            last_click: None,
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
        // Offsets are measured from the bottom. When new output adds rows to
        // history, increase a scrolled-up pane's offset by the same amount so
        // its current content stays anchored instead of drifting toward the
        // bottom as snapshots arrive.
        let previous_totals: HashMap<_, _> = self
            .parsers
            .iter_mut()
            .map(|(&id, parser)| (id, total_scrollback(parser)))
            .collect();
        let mut active_ids = Vec::new();
        self.sync_parsers(&snap.root, &mut active_ids);
        self.parsers.retain(|id, _| active_ids.contains(id));
        self.scroll.retain(|id, _| active_ids.contains(id));
        self.status.set_scope_name(Some(snap.scope_name.clone()));
        self.snapshot = Some(snap);

        // Re-apply each projection's saved scroll position onto its parser.
        // Parsers keep their scrollback offset across polls, so this only
        // matters after a rebuild or a fresh parser.
        for (id, offset) in &mut self.scroll {
            if let Some(parser) = self.parsers.get_mut(id) {
                let total = total_scrollback(parser);
                if *offset > 0 {
                    let previous = previous_totals.get(id).copied().unwrap_or(total);
                    *offset = offset.saturating_add(total.saturating_sub(previous));
                }
                *offset = (*offset).min(total);
                parser.screen_mut().set_scrollback((*offset).min(total));
            }
        }
    }

    fn sync_parsers(&mut self, node: &SnapshotNode, active_ids: &mut Vec<usize>) {
        match node {
            SnapshotNode::Leaf {
                id,
                cols,
                rows,
                state,
                scrollback,
                alternate_screen: _,
            } => {
                active_ids.push(*id);
                self.upsert_parser(*id, *cols, *rows, state, scrollback);
            }
            SnapshotNode::Debug { .. } => {}
            SnapshotNode::Split { children, .. } => {
                for child in children {
                    self.sync_parsers(&child.node, active_ids);
                }
            }
            SnapshotNode::Tabs { pages, .. } => {
                for page in pages {
                    if !page.is_debug {
                        active_ids.push(page.id);
                        self.upsert_parser(
                            page.id,
                            page.cols,
                            page.rows,
                            &page.state,
                            &page.scrollback,
                        );
                    }
                }
            }
        }
    }

    /// Keeps one persistent parser per projection. A non-empty `scrollback`
    /// means the server's history changed and the parser must be rebuilt from
    /// scratch (scrollback can't be appended incrementally). Otherwise the
    /// existing parser is resized if needed and the visible state applied on
    /// top, which leaves its scrollback untouched.
    fn upsert_parser(&mut self, id: usize, cols: u16, rows: u16, state: &[u8], scrollback: &[u8]) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if !scrollback.is_empty() {
            let mut parser = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
            parser.process(scrollback);
            parser.process(state);
            self.parsers.insert(id, parser);
            return;
        }
        match self.parsers.get_mut(&id) {
            Some(parser) => {
                let (prows, pcols) = parser.screen().size();
                if (prows, pcols) != (rows, cols) {
                    parser.screen_mut().set_size(rows, cols);
                }
                parser.process(state);
            }
            None => {
                let mut parser = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
                parser.process(state);
                self.parsers.insert(id, parser);
            }
        }
    }

    fn handle_server_reply(&mut self, reply: ServerMsg) -> io::Result<()> {
        match reply {
            ServerMsg::Snapshot(snap) => {
                self.status.set_error(None);
                // Forward escape sequences the server couldn't render (kitty
                // graphics protocol) to the real terminal for display.
                if !snap.passthrough.is_empty() {
                    let mut stdout = io::stdout().lock();
                    let _ = stdout.write_all(snap.passthrough.as_bytes());
                    let _ = stdout.flush();
                }
                self.apply_snapshot(snap);
            }
            ServerMsg::Unchanged => {}
            ServerMsg::Ok => {}
            ServerMsg::ScopeClosed => {
                self.snapshot = None;
                self.status.set_scope_name(None);
                self.copy_mode = None;
                self.parsers.clear();
                self.scroll.clear();
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

    /// True when a projection is on its alternate screen (vim, less, ...),
    /// where there is no scrollback and scrolling is left to the program.
    fn pane_in_alternate_screen(&self, id: usize) -> bool {
        self.snapshot
            .as_ref()
            .and_then(|snap| snapshot_alternate_screen(&snap.root, id))
            .unwrap_or(false)
    }

    /// Scrolls `id`'s view by `delta` rows. Offset 0 is the bottom of the
    /// screen; positive `delta` scrolls up into history, negative toward it.
    fn scroll_pane(&mut self, id: usize, delta: i64) {
        let Some(parser) = self.parsers.get_mut(&id) else {
            return;
        };
        let total = total_scrollback(parser);
        let offset = i64::try_from(*self.scroll.get(&id).unwrap_or(&0))
            .unwrap_or_default()
            .saturating_add(delta)
            .clamp(0, i64::try_from(total).unwrap_or_default()) as usize;
        self.scroll.insert(id, offset);
        parser.screen_mut().set_scrollback(offset);
    }

    /// Scrolls `id`'s view by one page worth of rows.
    fn scroll_pane_page(&mut self, id: usize, dir: i64) {
        let page = self
            .parsers
            .get(&id)
            .map(|p| {
                let (rows, _) = p.screen().size();
                usize::from(rows.saturating_sub(2).max(1))
            })
            .unwrap_or(24);
        self.scroll_pane(id, dir * i64::try_from(page).unwrap_or(24));
    }

    /// Resets `id`'s view to the bottom of the scrollback.
    fn snap_to_bottom(&mut self, id: usize) {
        self.scroll.insert(id, 0);
        if let Some(parser) = self.parsers.get_mut(&id) {
            parser.screen_mut().set_scrollback(0);
        }
    }

    /// Routes a mouse wheel event: scroll local history on a normal main
    /// screen, and forward it when the pane is alternate-screen or the app
    /// has enabled a terminal mouse protocol.
    fn handle_wheel(&mut self, mouse: MouseEvent) -> io::Result<()> {
        let target = self
            .snapshot
            .as_ref()
            .and_then(|snap| pane_at(&snap.root, self.workspace_area, mouse.column, mouse.row));
        let Some(id) = target else {
            return Ok(());
        };
        let app_owns_mouse = self.parsers.get(&id).is_some_and(|parser| {
            parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
        });
        if self.pane_in_alternate_screen(id) || app_owns_mouse {
            return self.forward_event(Event::Mouse(mouse));
        }
        // offset 0 is the bottom; increasing it moves the view into history.
        let delta = if mouse.kind == MouseEventKind::ScrollUp {
            3
        } else {
            -3
        };
        self.scroll_pane(id, delta);
        Ok(())
    }

    /// Queues input without waiting on a network round trip. The regular poll
    /// retrieves the resulting snapshot after the remote process responds.
    fn forward_event(&mut self, event: Event) -> io::Result<()> {
        let Some(snap) = &self.snapshot else {
            return Ok(());
        };
        self.client.send(&ClientMsg::InputAsync {
            scope_id: snap.scope_id,
            event,
        })?;
        Ok(())
    }

    /// Event handling while copy mode is active. Copy mode owns all input
    /// until the user copies, quits, or jumps back to managing mode.
    fn handle_copy_mode_event(&mut self, event: &Event) -> io::Result<()> {
        if let Event::Resize(cols, rows) = event {
            self.workspace_area = Rect::new(0, 0, (*cols).max(1), rows.saturating_sub(1).max(1));
            return Ok(());
        }

        if let Event::Paste(text) = event {
            self.copy_mode = None;
            return self.forward_event(Event::Paste(text.clone()));
        }

        if let Event::Mouse(mouse) = event {
            let pane_id = self.copy_mode.as_ref().map(|mode| mode.pane_id);
            let content = pane_id.and_then(|id| {
                self.snapshot
                    .as_ref()
                    .and_then(|snap| pane_content_rect(&snap.root, self.workspace_area, id))
            });
            if let Some(content) = content {
                let col = mouse
                    .column
                    .saturating_sub(content.x)
                    .min(content.width.saturating_sub(1));
                let row = mouse
                    .row
                    .saturating_sub(content.y)
                    .min(content.height.saturating_sub(1));
                if let Some(mode) = &mut self.copy_mode {
                    match mouse.kind {
                        MouseEventKind::Drag(crossterm::event::MouseButton::Left)
                        | MouseEventKind::Moved => mode.mouse_move(col, row),
                        MouseEventKind::Up(crossterm::event::MouseButton::Left) => mode.mouse_up(),
                        _ => {}
                    }
                }
            }
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

    /// Starts a highlighted mouse selection on a pane whose app has not
    /// requested mouse input. Apps with mouse reporting keep their clicks.
    fn start_mouse_selection(&mut self, mouse: MouseEvent) -> bool {
        if mouse.kind != MouseEventKind::Down(crossterm::event::MouseButton::Left) {
            return false;
        }
        let Some(snap) = &self.snapshot else {
            return false;
        };
        let Some(id) = pane_at(&snap.root, self.workspace_area, mouse.column, mouse.row) else {
            return false;
        };
        let force_local_selection = mouse.modifiers.contains(KeyModifiers::SHIFT);
        let app_owns_mouse = self.parsers.get(&id).is_some_and(|parser| {
            parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
        });
        if !force_local_selection && (self.pane_in_alternate_screen(id) || app_owns_mouse) {
            return false;
        }
        let Some(content) = pane_content_rect(&snap.root, self.workspace_area, id) else {
            return false;
        };
        if !content.contains(Position::new(mouse.column, mouse.row)) {
            return false;
        }
        let col = mouse.column - content.x;
        let row = mouse.row - content.y;
        let now = Instant::now();
        let double_click =
            self.last_click
                .take()
                .is_some_and(|(last_at, last_id, last_col, last_row)| {
                    last_id == id
                        && last_col == col
                        && last_row == row
                        && now.duration_since(last_at) <= Duration::from_millis(450)
                });
        if !double_click {
            self.last_click = Some((now, id, col, row));
        }
        let Some(parser) = self.parsers.get(&id) else {
            return false;
        };
        let Some(mut mode) = CopyMode::new(id, parser) else {
            return false;
        };
        let started = if double_click {
            mode.select_word_at(col, row)
        } else {
            mode.mouse_down(col, row)
        };
        if !started {
            self.last_click = None;
            return false;
        }
        self.copy_mode = Some(mode);
        true
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

            if self
                .copy_mode
                .as_ref()
                .is_some_and(|mode| !mode.is_mouse_selection() || mode.is_dragging())
            {
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
            } else if let Event::Mouse(mouse) = &event
                && self.start_mouse_selection(*mouse)
            {
                // The selection overlay owns the drag until mouse-up.
            } else if let Event::Key(key) = &event
                && key.kind == KeyEventKind::Press
                && key.modifiers == KeyModifiers::NONE
                && matches!(key.code, KeyCode::Char('c'))
                && self.copy_mode.as_ref().is_some_and(CopyMode::has_selection)
            {
                if let Some(selection) = &self.copy_mode {
                    clipboard::clipboard_set(&selection.copy_text());
                }
            } else if let Event::Key(key) = &event
                && key.kind == KeyEventKind::Press
                && key.code == KeyCode::Esc
                && self
                    .copy_mode
                    .as_ref()
                    .is_some_and(CopyMode::is_mouse_selection)
            {
                self.copy_mode = None;
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
                        Event::Mouse(mouse_event)
                            if matches!(
                                mouse_event.kind,
                                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                            ) =>
                        {
                            self.handle_wheel(*mouse_event)?;
                        }
                        Event::Key(key_event)
                            if key_event.kind == KeyEventKind::Press
                                && matches!(
                                    key_event.code,
                                    KeyCode::PageUp | KeyCode::PageDown
                                )
                                && key_event.modifiers == KeyModifiers::NONE =>
                        {
                            let focused = self.snapshot.as_ref().map(|s| s.focused_id);
                            if let Some(id) = focused
                                && self.pane_in_alternate_screen(id)
                            {
                                self.forward_event(event)?;
                            } else if let Some(id) = focused {
                                let dir = if key_event.code == KeyCode::PageUp {
                                    1
                                } else {
                                    -1
                                };
                                self.scroll_pane_page(id, dir);
                            }
                        }
                        _ => {
                            let key_release = matches!(
                                &event,
                                Event::Key(k) if k.kind == KeyEventKind::Release
                            );
                            let is_mouse_event = matches!(&event, Event::Mouse(_));
                            if !key_release {
                                if !is_mouse_event
                                    && let Some(id) = self.snapshot.as_ref().map(|s| s.focused_id)
                                {
                                    self.snap_to_bottom(id);
                                }
                                self.forward_event(event)?;
                            }
                        }
                    },
                    Mode::Managing => match &event {
                        Event::Paste(_) => self.forward_event(event)?,
                        Event::Key(key_event)
                            if key_event.kind == KeyEventKind::Press
                                && key_event.code == KeyCode::Char('v')
                                && key_event.modifiers == KeyModifiers::NONE =>
                        {
                            self.paste_from_clipboard()?;
                        }
                        Event::Key(key_event)
                            if matches!(
                                key_event.kind,
                                KeyEventKind::Press | KeyEventKind::Repeat
                            ) =>
                        {
                            self.status.set_error(None);
                            if matches!(key_event.code, KeyCode::PageUp | KeyCode::PageDown)
                                && key_event.modifiers == KeyModifiers::NONE
                            {
                                let focused = self.snapshot.as_ref().map(|s| s.focused_id);
                                if let Some(id) = focused {
                                    if self.pane_in_alternate_screen(id) {
                                        self.forward_event(event)?;
                                    } else {
                                        let dir = if key_event.code == KeyCode::PageUp {
                                            1
                                        } else {
                                            -1
                                        };
                                        self.scroll_pane_page(id, dir);
                                    }
                                }
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
                        Event::Mouse(mouse_event) => {
                            if matches!(
                                mouse_event.kind,
                                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                            ) {
                                self.handle_wheel(*mouse_event)?;
                            } else {
                                self.forward_event(event)?;
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
            let trace = self.client.debug_trace();
            SnapshotView::new(snap, &self.parsers, &trace).render(chunks[0], buf);
        }
        self.status.render(chunks[1], buf);

        if let Some(copy_mode) = &self.copy_mode {
            if copy_mode.is_mouse_selection() {
                if let Some(snap) = &self.snapshot
                    && let Some(area) = pane_content_rect(&snap.root, chunks[0], copy_mode.pane_id)
                {
                    copy_mode.render(area, buf);
                }
            } else {
                copy_mode.render(chunks[0], buf);
            }
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

/// Number of scrolled-out rows currently available in a parser's scrollback.
fn total_scrollback(parser: &mut vt100::Parser) -> usize {
    parser.screen_mut().set_scrollback(usize::MAX);
    let total = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(0);
    total
}

fn snapshot_alternate_screen(node: &SnapshotNode, id: usize) -> Option<bool> {
    match node {
        SnapshotNode::Leaf {
            id: leaf_id,
            alternate_screen,
            ..
        } => (*leaf_id == id).then_some(*alternate_screen),
        SnapshotNode::Debug { .. } => None,
        SnapshotNode::Split { children, .. } => children
            .iter()
            .find_map(|child| snapshot_alternate_screen(&child.node, id)),
        SnapshotNode::Tabs { pages, .. } => pages
            .iter()
            .find(|page| page.id == id)
            .map(|page| page.alternate_screen),
    }
}

/// Queries the real terminal for its character cell size in pixels via
/// `CSI 16 t` (supported by kitty, wezterm, xterm, ...). Returns `None` when
/// the terminal doesn't answer.
fn query_pixel_sizes() -> Option<(u16, u16)> {
    #[cfg(unix)]
    {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::time::{Duration, Instant};

        // The response arrives on the terminal's input; read it from the
        // controlling tty directly so crossterm's own event reader (which
        // starts later) never sees these bytes.
        let mut tty = std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/tty")
            .ok()?;
        // The fd is a fresh open of /dev/tty, so the nonblocking flag only
        // affects this handle and disappears when it's dropped.
        unsafe {
            libc::fcntl(tty.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }

        let _ = crossterm::terminal::enable_raw_mode();
        let result = (|| {
            let mut stdout = io::stdout();
            stdout.write_all(b"\x1b[16t").ok()?;
            stdout.flush().ok()?;

            let mut buf = Vec::new();
            let mut chunk = [0u8; 128];
            let start = Instant::now();
            loop {
                if let Some((h, w)) = parse_pixel_response(&buf, b'6') {
                    return Some((w, h));
                }
                if start.elapsed() > Duration::from_millis(300) {
                    return None;
                }
                match tty.read(&mut chunk) {
                    Ok(0) => return None,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return None,
                }
            }
        })();
        let _ = crossterm::terminal::disable_raw_mode();
        result
    }
    #[cfg(not(unix))]
    None
}

/// Finds a `ESC [ <kind> ; <height> ; <width> t` response in `buf`.
fn parse_pixel_response(buf: &[u8], kind: u8) -> Option<(u16, u16)> {
    let mut i = 0;
    while i + 4 <= buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == kind && buf[i + 3] == b';' {
            let rest = &buf[i + 4..];
            let mut parts = rest.split(|&b| b == b';' || b == b't');
            let height = parse_num(parts.next()?)?;
            let width = parse_num(parts.next()?)?;
            return Some((height, width));
        }
        i += 1;
    }
    None
}

fn parse_num(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
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
    let mut ensure_server = false;
    let mut ssh_host: Option<String> = None;
    let mut addr = server::DEFAULT_ADDR.to_string();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--serve" => serve = true,
            "--ensure-server" => ensure_server = true,
            "--ssh" => {
                if let Some(val) = args.get(i + 1) {
                    ssh_host = Some(val.clone());
                    i += 1;
                }
            }
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

    if ensure_server {
        drop(server::connect_or_spawn(&addr)?);
        return Ok(());
    }

    let mut client = match ssh_host {
        Some(host) => server::connect_ssh(&host)?,
        None => server::connect_or_spawn(&addr)?,
    };

    // Report the real terminal's cell size in pixels so the server can put it
    // in the PTY's TIOCGWINSZ and answer size queries. Without it programs
    // like `kitty +kitten icat` think the terminal can't report pixel sizes.
    if let Some((cell_w, cell_h)) = query_pixel_sizes() {
        let _ = client.send(&ClientMsg::SetPixelSize { cell_w, cell_h });
        let _ = client.recv::<ServerMsg>();
    }

    ratatui::run(|terminal| {
        // Without these sequences the terminal never reports mouse events, so
        // nothing could be forwarded to projections. The guard restores mouse
        // reporting on both normal and error/panic exits.
        execute!(
            io::stdout(),
            EnableMouseCapture,
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
            ),
        )
        .map_err(io::Error::other)?;
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
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            PopKeyboardEnhancementFlags,
        );
        let _ = io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cell_size_response() {
        // CSI 16 t replies with ESC [ 6 ; <height> ; <width> t.
        assert_eq!(parse_pixel_response(b"\x1b[6;23;10t", b'6'), Some((23, 10)));
        // Extra bytes before the response are tolerated.
        assert_eq!(
            parse_pixel_response(b"\x1b[?2004l\x1b[6;24;12t", b'6'),
            Some((24, 12))
        );
        // Partial / malformed responses are ignored.
        assert_eq!(parse_pixel_response(b"\x1b[6;23", b'6'), None);
        assert_eq!(parse_pixel_response(b"nothing here", b'6'), None);
    }

    #[test]
    fn slice_contains_works() {
        let state = b"\x1b[?25h\x1b_Gi=1;OK\x1b\\\x1b[?1;2c";
        assert!(slice_contains(state, b"\x1b[?1;2c"), "da not found");
        assert!(slice_contains(state, b"\x1b_Gi=1;OK\x1b\\"), "ok not found");
        let root = SnapshotNode::Leaf {
            id: 1,
            cols: 10,
            rows: 10,
            state: state.to_vec(),
            scrollback: Vec::new(),
            alternate_screen: false,
        };
        assert!(
            root_has_raw_state(&root, &|s: &[u8]| slice_contains(s, b"\x1b[?1;2c")),
            "root da not found"
        );
    }

    #[test]
    fn repro_large_image_end_to_end() {
        use std::net::TcpStream;
        use std::time::Duration;

        let port = 7500 + (std::process::id() % 1000) as u16;
        let addr = format!("127.0.0.1:{port}");
        let script_path = std::env::temp_dir().join("harp_e2e_img.sh");
        let script = r#"#!/bin/bash
# Mimic icat's support detection: emit graphics capability probes (direct /
# file / shm query) plus a device-attributes query, then wait. The tty echoes
# the server's replies into the pane's screen, so the test can verify them.
python3 - <<'PY'
import sys
sys.stdout.buffer.write(b"\x1b_Ga=q,i=1,t=d,s=1,v=1,f=24,S=3;MTIz\x1b\\")
sys.stdout.buffer.write(b"\x1b_Ga=q,i=2,t=t,s=1,v=1,f=24,S=3;MTIz\x1b\\")
sys.stdout.buffer.write(b"\x1b_Ga=q,i=3,t=s,s=1,v=1,f=24,S=3;MTIz\x1b\\")
sys.stdout.buffer.write(b"\x1b[c")
sys.stdout.flush()
PY
sleep 30
echo E2E_DONE
"#;
        std::fs::write(&script_path, script).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let server_addr = addr.clone();
        std::thread::spawn(move || {
            let _ = crate::server::run_server(&server_addr);
        });
        // Wait for the server to accept.
        let mut stream = None;
        for _ in 0..50 {
            if let Ok(s) = TcpStream::connect(&addr) {
                stream = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let stream = stream.expect("server did not start");
        let mut client = crate::crypto::EncryptedStream::handshake_client(stream).unwrap();

        client
            .send(&ClientMsg::SetPixelSize {
                cell_w: 12,
                cell_h: 24,
            })
            .unwrap();
        let _ = client.recv::<ServerMsg>();

        let cwd = std::env::temp_dir();
        client
            .send(&ClientMsg::CreateScope {
                cwd: cwd.clone(),
                cols: 80,
                rows: 24,
            })
            .unwrap();
        match client.recv::<ServerMsg>().unwrap() {
            ServerMsg::Snapshot(_) => {}
            other => panic!("expected snapshot on create, got {other:?}"),
        }

        // Split to run the image script in a fresh pane.
        client
            .send(&ClientMsg::Split {
                scope_id: 1,
                dir: NavDir::Left,
                spec: RemoteScopeSpec {
                    machine: "localhost".into(),
                    cwd: cwd.clone(),
                    program: script_path.display().to_string(),
                },
            })
            .unwrap();
        match client.recv::<ServerMsg>().unwrap() {
            ServerMsg::Snapshot(_) => {}
            other => panic!("expected snapshot on split, got {other:?}"),
        }

        // Poll until the probe responses reach the child's screen.
        let start = std::time::Instant::now();
        let mut total_passthrough = 0usize;
        let mut last_snap: Option<ScopeSnapshot> = None;
        loop {
            if start.elapsed() > Duration::from_secs(10) {
                let mut dumps = Vec::new();
                if let Some(snap) = &last_snap {
                    dump_leaf_states(&mut dumps, &snap.root);
                }
                panic!(
                    "TIMED OUT waiting for probe responses; passthrough={total_passthrough}; states={dumps:?}"
                );
            }
            client
                .send(&ClientMsg::Poll {
                    scope_id: 1,
                    last_revision: 0,
                })
                .unwrap();
            match client.recv::<ServerMsg>().unwrap() {
                ServerMsg::Snapshot(snap) => {
                    total_passthrough += snap.passthrough.len();
                    last_snap = Some(snap.clone());
                    let hit_verdict = root_has_leaf_state(&snap.root, &|text: &str| {
                        text.contains("Gi=1;OK") && text.contains("?1;2c")
                    });
                    if hit_verdict {
                        // The replies were echoed into the pane's screen, which
                        // proves they were delivered into the pty. Check that
                        // direct was OK'ed and file/shm rejected.
                        let ok_and_ebadf = root_has_leaf_state(&snap.root, &|text: &str| {
                            text.contains("Gi=1;OK")
                                && text.contains("Gi=2;EBADF")
                                && text.contains("Gi=3;EBADF")
                        });
                        eprintln!(
                            "completed at {:?} with {} passthrough bytes, ok={ok_and_ebadf}",
                            start.elapsed(),
                            total_passthrough
                        );
                        assert!(
                            ok_and_ebadf,
                            "server must answer direct OK and reject file/shm"
                        );
                        break;
                    }
                }
                ServerMsg::Unchanged => {}
                other => panic!("unexpected reply {other:?}"),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&script_path);
    }

    /// Whether any leaf in the snapshot tree satisfies `pred` over its visible
    /// screen text.
    fn root_has_leaf_state(root: &SnapshotNode, pred: &dyn Fn(&str) -> bool) -> bool {
        match root {
            SnapshotNode::Leaf { state, .. } => pred(&String::from_utf8_lossy(state)),
            SnapshotNode::Debug { .. } => false,
            SnapshotNode::Split { children, .. } => {
                children.iter().any(|c| root_has_leaf_state(&c.node, pred))
            }
            SnapshotNode::Tabs { pages, .. } => pages
                .iter()
                .any(|p| pred(&String::from_utf8_lossy(&p.state))),
        }
    }

    /// Collects the visible text of every leaf in the snapshot tree.
    fn dump_leaf_states(out: &mut Vec<String>, root: &SnapshotNode) {
        match root {
            SnapshotNode::Leaf { state, .. } => {
                out.push(String::from_utf8_lossy(state).into_owned());
            }
            SnapshotNode::Debug { .. } => {}
            SnapshotNode::Split { children, .. } => {
                for c in children {
                    dump_leaf_states(out, &c.node);
                }
            }
            SnapshotNode::Tabs { pages, .. } => {
                for p in pages {
                    out.push(String::from_utf8_lossy(&p.state).into_owned());
                }
            }
        }
    }

    /// Whether any leaf's raw screen bytes satisfy `pred`.
    fn root_has_raw_state(root: &SnapshotNode, pred: &dyn Fn(&[u8]) -> bool) -> bool {
        match root {
            SnapshotNode::Leaf { state, .. } => pred(state),
            SnapshotNode::Debug { .. } => false,
            SnapshotNode::Split { children, .. } => {
                children.iter().any(|c| root_has_raw_state(&c.node, pred))
            }
            SnapshotNode::Tabs { pages, .. } => pages.iter().any(|p| pred(&p.state)),
        }
    }

    fn slice_contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
