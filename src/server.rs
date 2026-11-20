use std::{
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::Result;
use ratatui::layout::{Direction, Rect};

use crate::{
    crypto::EncryptedStream,
    protocol::{ClientMsg, NavDir, ScopeInfo, ServerMsg},
    scopes::{NavDirection, Scope, ScopeSpec},
};

pub const DEFAULT_ADDR: &str = "127.0.0.1:7420";

struct ServerScope {
    id: u64,
    name: String,
    cwd: PathBuf,
    scope: Scope,
}

struct ServerState {
    scopes: Vec<ServerScope>,
    next_id: u64,
    had_scopes: bool,
}

impl ServerState {
    fn new() -> Self {
        Self {
            scopes: Vec::new(),
            next_id: 1,
            had_scopes: false,
        }
    }

    fn prune_all(&mut self) {
        for s in &mut self.scopes {
            let _ = s.scope.prune_exited();
        }
        self.scopes.retain(|s| !s.scope.is_empty());
    }

    fn should_exit(&self) -> bool {
        self.had_scopes && self.scopes.is_empty()
    }

    fn find_mut(&mut self, scope_id: u64) -> Option<&mut ServerScope> {
        self.scopes.iter_mut().find(|s| s.id == scope_id)
    }

    fn handle_msg(&mut self, msg: ClientMsg) -> ServerMsg {
        self.prune_all();

        match msg {
            ClientMsg::ListScopes => {
                let infos = self
                    .scopes
                    .iter()
                    .map(|s| ScopeInfo {
                        id: s.id,
                        name: s.name.clone(),
                        cwd: s.cwd.clone(),
                        pane_count: s.scope.leaf_count(),
                    })
                    .collect();
                ServerMsg::Scopes(infos)
            }
            ClientMsg::CreateScope { cwd, cols, rows } => {
                let id = self.next_id;
                let area = Rect::new(0, 0, cols.max(1), rows.max(1));
                match Scope::with_terminals_in(1, Direction::Horizontal, area, &cwd) {
                    Ok(scope) => {
                        self.next_id += 1;
                        let name = cwd
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .filter(|n| !n.is_empty())
                            .unwrap_or_else(|| format!("scope-{id}"));
                        let snap = scope.snapshot(id, name.clone());
                        self.scopes.push(ServerScope {
                            id,
                            name,
                            cwd,
                            scope,
                        });
                        self.had_scopes = true;
                        match snap {
                            Some(s) => ServerMsg::Snapshot(s),
                            None => ServerMsg::ScopeClosed,
                        }
                    }
                    Err(e) => ServerMsg::Error(e.to_string()),
                }
            }
            ClientMsg::AttachScope {
                scope_id,
                cols,
                rows,
            } => {
                let Some(entry) = self.find_mut(scope_id) else {
                    return ServerMsg::ScopeClosed;
                };
                if let Err(e) = entry.scope.resize(Rect::new(0, 0, cols.max(1), rows.max(1))) {
                    return ServerMsg::Error(e.to_string());
                }
                let name = entry.name.clone();
                match entry.scope.snapshot(scope_id, name) {
                    Some(s) => ServerMsg::Snapshot(s),
                    None => ServerMsg::ScopeClosed,
                }
            }
            ClientMsg::RenameScope { scope_id, name } => {
                let Some(entry) = self.find_mut(scope_id) else {
                    return ServerMsg::ScopeClosed;
                };
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    entry.name = trimmed.to_string();
                    entry.scope.bump_revision();
                }
                let name = entry.name.clone();
                match entry.scope.snapshot(scope_id, name) {
                    Some(s) => ServerMsg::Snapshot(s),
                    None => ServerMsg::ScopeClosed,
                }
            }
            ClientMsg::Poll {
                scope_id,
                last_revision,
            } => {
                let Some(entry) = self.find_mut(scope_id) else {
                    return ServerMsg::ScopeClosed;
                };
                if entry.scope.revision() == last_revision {
                    ServerMsg::Unchanged
                } else {
                    let name = entry.name.clone();
                    match entry.scope.snapshot(scope_id, name) {
                        Some(s) => ServerMsg::Snapshot(s),
                        None => ServerMsg::ScopeClosed,
                    }
                }
            }
            ClientMsg::Resize {
                scope_id,
                cols,
                rows,
            } => {
                let Some(entry) = self.find_mut(scope_id) else {
                    return ServerMsg::ScopeClosed;
                };
                if let Err(e) = entry.scope.resize(Rect::new(0, 0, cols.max(1), rows.max(1))) {
                    return ServerMsg::Error(e.to_string());
                }
                let name = entry.name.clone();
                match entry.scope.snapshot(scope_id, name) {
                    Some(s) => ServerMsg::Snapshot(s),
                    None => ServerMsg::ScopeClosed,
                }
            }
            ClientMsg::Input { scope_id, event } => {
                let Some(entry) = self.find_mut(scope_id) else {
                    return ServerMsg::ScopeClosed;
                };
                let before = entry.scope.revision();
                if let Err(e) = entry.scope.handle_event(&event) {
                    return ServerMsg::Error(e.to_string());
                }
                let _ = entry.scope.prune_exited();
                self.prune_all();
                let Some(entry) = self.find_mut(scope_id) else {
                    return ServerMsg::ScopeClosed;
                };
                // Mouse motion or events a pane doesn't want may change nothing;
                // avoid shipping a full snapshot for those. The client's next
                // Poll picks up any deferred screen changes.
                if entry.scope.revision() == before {
                    return ServerMsg::Unchanged;
                }
                let name = entry.name.clone();
                match entry.scope.snapshot(scope_id, name) {
                    Some(s) => ServerMsg::Snapshot(s),
                    None => ServerMsg::ScopeClosed,
                }
            }
            ClientMsg::MoveFocus { scope_id, dir } => {
                let Some(entry) = self.find_mut(scope_id) else {
                    return ServerMsg::ScopeClosed;
                };
                entry.scope.move_focus(to_nav_dir(dir));
                let name = entry.name.clone();
                match entry.scope.snapshot(scope_id, name) {
                    Some(s) => ServerMsg::Snapshot(s),
                    None => ServerMsg::ScopeClosed,
                }
            }
            ClientMsg::Split {
                scope_id,
                dir,
                spec,
            } => {
                let Some(entry) = self.find_mut(scope_id) else {
                    return ServerMsg::ScopeClosed;
                };
                let scope_spec = ScopeSpec {
                    machine: spec.machine,
                    cwd: spec.cwd,
                    program: spec.program,
                };
                if let Err(e) = entry
                    .scope
                    .split_focused_with_spec(to_nav_dir(dir), &scope_spec)
                {
                    return ServerMsg::Error(e.to_string());
                }
                let name = entry.name.clone();
                match entry.scope.snapshot(scope_id, name) {
                    Some(s) => ServerMsg::Snapshot(s),
                    None => ServerMsg::ScopeClosed,
                }
            }
            ClientMsg::NewTab { scope_id, spec } => {
                let Some(entry) = self.find_mut(scope_id) else {
                    return ServerMsg::ScopeClosed;
                };
                let scope_spec = ScopeSpec {
                    machine: spec.machine,
                    cwd: spec.cwd,
                    program: spec.program,
                };
                if let Err(e) = entry.scope.new_tab_with_spec(&scope_spec) {
                    return ServerMsg::Error(e.to_string());
                }
                let name = entry.name.clone();
                match entry.scope.snapshot(scope_id, name) {
                    Some(s) => ServerMsg::Snapshot(s),
                    None => ServerMsg::ScopeClosed,
                }
            }
            ClientMsg::SwitchTab { scope_id, dir } => {
                let Some(entry) = self.find_mut(scope_id) else {
                    return ServerMsg::ScopeClosed;
                };
                entry.scope.switch_tab(dir);
                let name = entry.name.clone();
                match entry.scope.snapshot(scope_id, name) {
                    Some(s) => ServerMsg::Snapshot(s),
                    None => ServerMsg::ScopeClosed,
                }
            }
        }
    }
}

fn to_nav_dir(dir: NavDir) -> NavDirection {
    match dir {
        NavDir::Up => NavDirection::Up,
        NavDir::Down => NavDirection::Down,
        NavDir::Left => NavDirection::Left,
        NavDir::Right => NavDirection::Right,
    }
}

pub fn run_server(bind_addr: &str) -> Result<()> {
    let listener = TcpListener::bind(bind_addr)?;
    listener.set_nonblocking(true)?;
    let state = Arc::new(Mutex::new(ServerState::new()));

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    let Ok(mut encrypted) = EncryptedStream::handshake_server(stream) else {
                        return;
                    };
                    while let Ok(msg) = encrypted.recv::<ClientMsg>() {
                        let (reply, exit_after) = {
                            let mut guard = state.lock().unwrap();
                            let reply = guard.handle_msg(msg);
                            let exit_after = guard.should_exit();
                            (reply, exit_after)
                        };
                        if encrypted.send(&reply).is_err() || exit_after {
                            break;
                        }
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }

        let mut guard = state.lock().unwrap();
        guard.prune_all();
        if guard.should_exit() {
            break;
        }
    }

    Ok(())
}

pub fn connect_or_spawn(addr: &str) -> Result<EncryptedStream> {
    if let Ok(stream) = TcpStream::connect(addr) {
        return Ok(EncryptedStream::handshake_client(stream)?);
    }

    if is_local_addr(addr) {
        spawn_background_server(addr)?;
        for _ in 0..40 {
            if let Ok(stream) = TcpStream::connect(addr) {
                return Ok(EncryptedStream::handshake_client(stream)?);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    let stream = TcpStream::connect(addr)?;
    Ok(EncryptedStream::handshake_client(stream)?)
}

fn is_local_addr(addr: &str) -> bool {
    if let Ok(sock) = addr.parse::<SocketAddr>() {
        return sock.ip().is_loopback();
    }
    addr.starts_with("localhost:")
}

fn spawn_background_server(addr: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--serve")
        .arg("--bind")
        .arg(addr)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    cmd.process_group(0);

    cmd.spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_exits_only_after_scopes_have_been_created_and_all_removed() {
        let mut state = ServerState::new();
        assert!(!state.should_exit());

        state.had_scopes = true;
        assert!(state.should_exit());
    }
}
