use std::{
    collections::VecDeque,
    fs::File,
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
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

/// A client connection and, for SSH connections, the tunnel that carries it.
/// Keeping the child here makes the forwarding process live exactly as long as
/// the client session.
pub struct ClientConnection {
    stream: EncryptedStream,
    _tunnel: Option<Child>,
    pending: VecDeque<(String, Instant)>,
    trace: VecDeque<ServerTraceEntry>,
}

#[derive(Clone, Debug)]
pub struct ServerTraceEntry {
    pub request: String,
    pub response: String,
    pub latency: Duration,
}

impl ClientConnection {
    pub fn send<T: serde::Serialize>(&mut self, value: &T) -> std::io::Result<()> {
        let label = message_label(value);
        self.stream.send(value)?;
        if label != "InputAsync" {
            self.pending.push_back((label, Instant::now()));
        }
        Ok(())
    }

    pub fn recv<T: serde::de::DeserializeOwned + serde::Serialize>(
        &mut self,
    ) -> std::io::Result<T> {
        let value: T = self.stream.recv()?;
        let response = message_label(&value);
        if let Some((request, started)) = self.pending.pop_front() {
            self.trace.push_back(ServerTraceEntry {
                request,
                response,
                latency: started.elapsed(),
            });
            while self.trace.len() > 500 {
                self.trace.pop_front();
            }
        }
        Ok(value)
    }

    pub fn debug_trace(&self) -> Vec<ServerTraceEntry> {
        self.trace.iter().cloned().collect()
    }
}

fn message_label<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| value.as_object().and_then(|map| map.keys().next().cloned()))
        })
        .unwrap_or_else(|| "Message".to_string())
}

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
    /// Character cell size in pixels reported by the client's terminal.
    cell_px: (u16, u16),
}

impl ServerState {
    fn new() -> Self {
        Self {
            scopes: Vec::new(),
            next_id: 1,
            had_scopes: false,
            cell_px: (0, 0),
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
            ClientMsg::SetPixelSize { cell_w, cell_h } => {
                self.cell_px = (cell_w, cell_h);
                for s in &mut self.scopes {
                    let _ = s.scope.set_cell_pixels(cell_w, cell_h);
                }
                ServerMsg::Ok
            }
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
                match Scope::with_terminals_in(1, Direction::Horizontal, area, &cwd, self.cell_px) {
                    Ok(mut scope) => {
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
                if let Err(e) = entry
                    .scope
                    .resize(Rect::new(0, 0, cols.max(1), rows.max(1)))
                {
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
                if let Err(e) = entry
                    .scope
                    .resize(Rect::new(0, 0, cols.max(1), rows.max(1)))
                {
                    return ServerMsg::Error(e.to_string());
                }
                let name = entry.name.clone();
                match entry.scope.snapshot(scope_id, name) {
                    Some(s) => ServerMsg::Snapshot(s),
                    None => ServerMsg::ScopeClosed,
                }
            }
            ClientMsg::Input { scope_id, event } | ClientMsg::InputAsync { scope_id, event } => {
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
            ClientMsg::CloseProjection {
                scope_id,
                projection_id,
            } => {
                let Some(entry) = self.find_mut(scope_id) else {
                    return ServerMsg::ScopeClosed;
                };
                match entry.scope.close_projection(projection_id) {
                    Ok(true) => {}
                    Ok(false) => return ServerMsg::Error("projection not found".to_string()),
                    Err(e) => return ServerMsg::Error(e.to_string()),
                }
                if let Some(entry) = self.find_mut(scope_id)
                    && let Some(snapshot) = entry.scope.snapshot(scope_id, entry.name.clone())
                {
                    return ServerMsg::Snapshot(snapshot);
                }
                self.scopes.retain(|entry| !entry.scope.is_empty());
                ServerMsg::Scopes(
                    self.scopes
                        .iter()
                        .map(|entry| ScopeInfo {
                            id: entry.id,
                            name: entry.name.clone(),
                            cwd: entry.cwd.clone(),
                            pane_count: entry.scope.leaf_count(),
                        })
                        .collect(),
                )
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
                        let reply_expected = !matches!(&msg, ClientMsg::InputAsync { .. });
                        let (reply, exit_after) = {
                            let mut guard = state.lock().unwrap();
                            let reply = guard.handle_msg(msg);
                            let exit_after = guard.should_exit();
                            (reply, exit_after)
                        };
                        if (reply_expected && encrypted.send(&reply).is_err()) || exit_after {
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

pub fn connect_or_spawn(addr: &str) -> Result<ClientConnection> {
    if let Ok(stream) = TcpStream::connect(addr) {
        return Ok(ClientConnection {
            stream: EncryptedStream::handshake_client(stream)?,
            _tunnel: None,
            pending: VecDeque::new(),
            trace: VecDeque::new(),
        });
    }

    if is_local_addr(addr) {
        spawn_background_server(addr)?;
        for _ in 0..40 {
            if let Ok(stream) = TcpStream::connect(addr) {
                return Ok(ClientConnection {
                    stream: EncryptedStream::handshake_client(stream)?,
                    _tunnel: None,
                    pending: VecDeque::new(),
                    trace: VecDeque::new(),
                });
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    let stream = TcpStream::connect(addr)?;
    Ok(ClientConnection {
        stream: EncryptedStream::handshake_client(stream)?,
        _tunnel: None,
        pending: VecDeque::new(),
        trace: VecDeque::new(),
    })
}

/// Connect to a machine through OpenSSH. The remote `harp --ensure-server`
/// command starts the persistent server if needed, then `ssh -N -L` carries
/// all client protocol traffic over the authenticated SSH connection.
pub fn connect_ssh(host: &str) -> Result<ClientConnection> {
    anyhow::ensure!(!host.trim().is_empty(), "SSH host cannot be empty");
    let remote_ensure = remote_ensure_command();
    let status = Command::new("ssh").arg(host).arg(&remote_ensure).status()?;
    if !status.success() {
        upload_and_start_remote(host)?;
    }

    let reservation = TcpListener::bind("127.0.0.1:0")?;
    let local_port = reservation.local_addr()?.port();
    drop(reservation);

    let mut tunnel = Command::new("ssh")
        .arg("-N")
        .arg("-L")
        .arg(format!("127.0.0.1:{local_port}:127.0.0.1:7420"))
        .arg(host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let addr = format!("127.0.0.1:{local_port}");
    for _ in 0..100 {
        if let Ok(stream) = TcpStream::connect(&addr) {
            match EncryptedStream::handshake_client(stream) {
                Ok(stream) => {
                    return Ok(ClientConnection {
                        stream,
                        _tunnel: Some(tunnel),
                        pending: VecDeque::new(),
                        trace: VecDeque::new(),
                    });
                }
                Err(_) => {}
            }
        }
        if let Some(status) = tunnel.try_wait()? {
            anyhow::bail!("SSH tunnel exited before connecting ({status})");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = tunnel.kill();
    let _ = tunnel.wait();
    anyhow::bail!("timed out opening SSH tunnel to {host}")
}

/// Send the currently running executable over SSH when the remote `harp`
/// command is unavailable. The remote copy is private, executable, and removed
/// as soon as it has launched the detached server.
fn upload_and_start_remote(host: &str) -> Result<()> {
    let exe = File::open(std::env::current_exe()?)?;
    let script = format!(
        "umask 077; f=$(mktemp /tmp/harp.XXXXXX) || exit 1; cat > \"$f\" || {{ rm -f \"$f\"; exit 1; }}; chmod 700 \"$f\" || {{ rm -f \"$f\"; exit 1; }}; {} \"$f\" --ensure-server; r=$?; rm -f \"$f\"; exit \"$r\"",
        remote_env_prefix()
    );
    let mut child = Command::new("ssh")
        .arg(host)
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("could not open SSH upload stream"))?;
    let mut exe = exe;
    std::io::copy(&mut exe, &mut stdin)?;
    stdin.flush()?;
    drop(stdin);
    let status = child.wait()?;
    anyhow::ensure!(
        status.success(),
        "could not upload and start Harp on SSH host {host}; the binary must match the remote machine's operating system and CPU architecture"
    );
    Ok(())
}

fn remote_ensure_command() -> String {
    format!("{} harp --ensure-server", remote_env_prefix())
}

/// Preserve the local terminal type and UTF-8 locale when the SSH command
/// starts the remote server without allocating a terminal.
fn remote_env_prefix() -> String {
    [
        "TERM",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "LC_MESSAGES",
        "COLORTERM",
    ]
    .into_iter()
    .filter_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| format!("{name}={}", shell_quote(&value)))
    })
    .fold("env".to_string(), |prefix, assignment| {
        format!("{prefix} {assignment}")
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
