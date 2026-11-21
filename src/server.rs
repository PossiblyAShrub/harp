use std::{
    collections::VecDeque,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::Result;
use ratatui::layout::{Direction, Rect};

use crate::{
    crypto::EncryptedStream,
    protocol::{ClientMsg, HostInfo, NavDir, ScopeInfo, ServerMsg},
    scopes::{NavDirection, Scope, ScopeSpec},
};

pub use crate::protocol::AskpassConfig;

pub const DEFAULT_ADDR: &str = "127.0.0.1:7420";

pub struct AskpassRequest {
    pub prompt: String,
    response: Sender<String>,
}

impl AskpassRequest {
    pub fn respond(self, value: String) {
        let _ = self.response.send(value);
    }
}

pub struct AskpassBroker {
    config: AskpassConfig,
    requests: Receiver<AskpassRequest>,
    shutdown: Arc<AtomicBool>,
}

impl AskpassBroker {
    pub fn new(program: PathBuf) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?.to_string();
        let mut token_bytes = [0u8; 32];
        getrandom::fill(&mut token_bytes).map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let token = token_bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let config = AskpassConfig {
            program,
            addr,
            token,
        };
        let (request_tx, requests) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_token = config.token.clone();
        thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let request_tx = request_tx.clone();
                        let token = thread_token.clone();
                        thread::spawn(move || handle_askpass_stream(stream, &token, request_tx));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(15));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            config,
            requests,
            shutdown,
        })
    }

    pub fn config(&self) -> AskpassConfig {
        self.config.clone()
    }

    pub fn try_recv(&self) -> Option<AskpassRequest> {
        self.requests.try_recv().ok()
    }
}

impl Drop for AskpassBroker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

fn handle_askpass_stream(
    mut stream: TcpStream,
    expected_token: &str,
    requests: Sender<AskpassRequest>,
) {
    let Ok(reader_stream) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader_stream);
    let mut token = String::new();
    let mut prompt = String::new();
    if reader.read_line(&mut token).is_err() || reader.read_line(&mut prompt).is_err() {
        return;
    }
    if token.trim_end() != expected_token {
        return;
    }
    let (response_tx, response_rx) = mpsc::channel();
    if requests
        .send(AskpassRequest {
            prompt: prompt.trim_end().to_string(),
            response: response_tx,
        })
        .is_err()
    {
        return;
    }
    if let Ok(response) = response_rx.recv() {
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
    }
}

pub fn run_askpass_helper(config: &AskpassConfig, prompt: &str) -> Result<()> {
    let mut stream = TcpStream::connect(&config.addr)?;
    writeln!(stream, "{}", config.token)?;
    writeln!(stream, "{}", prompt.replace(['\r', '\n'], " "))?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    io::stdout().write_all(&response)?;
    io::stdout().flush()?;
    Ok(())
}

/// A client connection and, for SSH connections, the tunnel that carries it.
/// Keeping the child here makes the forwarding process live exactly as long as
/// the client session.
pub struct ClientConnection {
    stream: EncryptedStream,
    peers: Vec<(HostInfo, Box<ClientConnection>)>,
    pending_peer: Option<usize>,
    aggregating_scopes: bool,
    cell_px: Option<(u16, u16)>,
    _tunnel: Option<Child>,
    _ssh_control_dir: Option<PathBuf>,
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
    pub fn send(&mut self, value: &ClientMsg) -> std::io::Result<()> {
        let label = message_label(value);
        if let ClientMsg::SetPixelSize { cell_w, cell_h } = value {
            self.cell_px = Some((*cell_w, *cell_h));
        }
        self.pending_peer = None;
        self.aggregating_scopes = matches!(value, ClientMsg::ListScopes);
        if self.aggregating_scopes {
            self.stream.send(value)?;
            for (_, peer) in &mut self.peers {
                peer.send(value)?;
            }
            return Ok(());
        }

        if let ClientMsg::CreateScopeOn {
            host,
            cwd,
            cols,
            rows,
        } = value
        {
            let Some((index, (_, peer))) = self
                .peers
                .iter_mut()
                .enumerate()
                .find(|(_, (info, _))| info.name == *host || info.ssh_host == *host)
            else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("unknown SSH host: {host}"),
                ));
            };
            self.pending_peer = Some(index);
            return peer.send(&ClientMsg::CreateScope {
                cwd: cwd.clone(),
                cols: *cols,
                rows: *rows,
            });
        }

        if let Some(scope_id) = client_scope_id(value) {
            let peer_index = (scope_id >> 48) as usize;
            if peer_index > 0 {
                let Some((_, peer)) = self.peers.get_mut(peer_index - 1) else {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "scope owner is no longer connected",
                    ));
                };
                self.pending_peer = Some(peer_index - 1);
                return peer.send(&rewrite_scope_id(
                    value.clone(),
                    scope_id & 0x0000_FFFF_FFFF_FFFF,
                ));
            }
        }

        self.stream.send(value)?;
        if label != "InputAsync" {
            self.pending.push_back((label, Instant::now()));
        }
        Ok(())
    }

    pub fn recv<T: serde::de::DeserializeOwned + serde::Serialize>(
        &mut self,
    ) -> std::io::Result<T> {
        if self.aggregating_scopes {
            self.aggregating_scopes = false;
            let mut scopes: Vec<ScopeInfo> = self.stream.recv::<ServerMsg>()?.into_scopes()?;
            for (index, (host, peer)) in self.peers.iter_mut().enumerate() {
                let mut remote: Vec<ScopeInfo> = peer.recv::<ServerMsg>()?.into_scopes()?;
                let catalog = remote.clone();
                self.stream.send(&ClientMsg::SyncRemoteScopes {
                    host: host.name.clone(),
                    scopes: catalog,
                })?;
                match self.stream.recv::<ServerMsg>()? {
                    ServerMsg::Ok => {}
                    ServerMsg::Error(error) => return Err(io::Error::other(error)),
                    _ => return Err(io::Error::other("invalid remote scope catalog response")),
                }
                for scope in &mut remote {
                    scope.id = (((index + 1) as u64) << 48) | (scope.id & 0x0000_FFFF_FFFF_FFFF);
                    scope.host = host.name.clone();
                }
                scopes.extend(remote);
            }
            let reply = ServerMsg::Scopes(scopes);
            return serde_json::from_value(serde_json::to_value(reply).map_err(io::Error::other)?)
                .map_err(io::Error::other);
        }
        let route = self.pending_peer.take();
        let mut wire: serde_json::Value = match route {
            Some(index) => self.peers[index].1.recv()?,
            None => self.stream.recv()?,
        };
        if let Some(index) = route {
            if let Ok(ServerMsg::Snapshot(mut snapshot)) =
                serde_json::from_value::<ServerMsg>(wire.clone())
            {
                snapshot.scope_id =
                    (((index + 1) as u64) << 48) | (snapshot.scope_id & 0x0000_FFFF_FFFF_FFFF);
                wire = serde_json::to_value(ServerMsg::Snapshot(snapshot))
                    .map_err(io::Error::other)?;
            }
        }
        let value: T = serde_json::from_value(wire).map_err(io::Error::other)?;
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

    pub fn add_peer_connection(
        &mut self,
        host: HostInfo,
        mut connection: ClientConnection,
    ) -> Result<()> {
        if self
            .peers
            .iter()
            .any(|(known, _)| known.ssh_host == host.ssh_host)
        {
            return Ok(());
        }
        if let Some((cell_w, cell_h)) = self.cell_px {
            connection.send(&ClientMsg::SetPixelSize { cell_w, cell_h })?;
            let _ = connection.recv::<ServerMsg>()?;
        }
        self.peers.push((host, Box::new(connection)));
        Ok(())
    }

    pub fn peer_connected(&self, ssh_host: &str) -> bool {
        self.peers.iter().any(|(host, _)| host.ssh_host == ssh_host)
    }

    pub fn peer_name(&self, index: usize) -> Option<&str> {
        self.peers.get(index).map(|(host, _)| host.name.as_str())
    }
}

impl Drop for ClientConnection {
    fn drop(&mut self) {
        if let Some(tunnel) = &mut self._tunnel {
            let _ = tunnel.kill();
            let _ = tunnel.wait();
        }
    }
}

trait IntoScopes {
    fn into_scopes(self) -> io::Result<Vec<ScopeInfo>>;
}

impl IntoScopes for ServerMsg {
    fn into_scopes(self) -> io::Result<Vec<ScopeInfo>> {
        match self {
            ServerMsg::Scopes(scopes) => Ok(scopes),
            ServerMsg::Error(error) => Err(io::Error::other(error)),
            _ => Err(io::Error::other("expected scope list from node")),
        }
    }
}

fn client_scope_id(msg: &ClientMsg) -> Option<u64> {
    match msg {
        ClientMsg::AttachScope { scope_id, .. }
        | ClientMsg::RenameScope { scope_id, .. }
        | ClientMsg::Poll { scope_id, .. }
        | ClientMsg::Resize { scope_id, .. }
        | ClientMsg::Input { scope_id, .. }
        | ClientMsg::InputAsync { scope_id, .. }
        | ClientMsg::CloseProjection { scope_id, .. }
        | ClientMsg::MoveFocus { scope_id, .. }
        | ClientMsg::DropProjection { scope_id, .. }
        | ClientMsg::MoveProjection { scope_id, .. }
        | ClientMsg::ActivateProjection { scope_id, .. }
        | ClientMsg::Split { scope_id, .. }
        | ClientMsg::NewTab { scope_id, .. }
        | ClientMsg::SwitchTab { scope_id, .. } => Some(*scope_id),
        _ => None,
    }
}

fn rewrite_scope_id(mut msg: ClientMsg, id: u64) -> ClientMsg {
    match &mut msg {
        ClientMsg::AttachScope { scope_id, .. }
        | ClientMsg::RenameScope { scope_id, .. }
        | ClientMsg::Poll { scope_id, .. }
        | ClientMsg::Resize { scope_id, .. }
        | ClientMsg::Input { scope_id, .. }
        | ClientMsg::InputAsync { scope_id, .. }
        | ClientMsg::CloseProjection { scope_id, .. }
        | ClientMsg::MoveFocus { scope_id, .. }
        | ClientMsg::DropProjection { scope_id, .. }
        | ClientMsg::MoveProjection { scope_id, .. }
        | ClientMsg::ActivateProjection { scope_id, .. }
        | ClientMsg::Split { scope_id, .. }
        | ClientMsg::NewTab { scope_id, .. }
        | ClientMsg::SwitchTab { scope_id, .. } => *scope_id = id,
        _ => {}
    }
    msg
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

fn peer_store_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config").join("harp").join("peers.json"))
}

fn remote_scope_store_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config").join("harp").join("remote-scopes.json"))
}

fn save_peers(peers: &[HostInfo]) -> io::Result<()> {
    let Some(path) = peer_store_path() else {
        return Err(io::Error::other("HOME is not set"));
    };
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("invalid peer store path"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!("peers.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(peers).map_err(io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

fn save_remote_scopes(scopes: &[ScopeInfo]) -> io::Result<()> {
    let Some(path) = remote_scope_store_path() else {
        return Err(io::Error::other("HOME is not set"));
    };
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("invalid remote scope store path"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!("remote-scopes.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(scopes).map_err(io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

struct ServerScope {
    id: u64,
    name: String,
    cwd: PathBuf,
    scope: Scope,
}

struct ServerState {
    scopes: Vec<ServerScope>,
    peers: Vec<HostInfo>,
    peer_masters: Vec<Option<Child>>,
    connecting_peers: std::collections::HashSet<String>,
    remote_scopes: Vec<ScopeInfo>,
    next_id: u64,
    had_scopes: bool,
    /// Character cell size in pixels reported by the client's terminal.
    cell_px: (u16, u16),
}

impl ServerState {
    fn new() -> Self {
        let peers: Vec<HostInfo> = peer_store_path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let remote_scopes = remote_scope_store_path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            scopes: Vec::new(),
            peer_masters: std::iter::repeat_with(|| None).take(peers.len()).collect(),
            peers,
            remote_scopes,
            connecting_peers: std::collections::HashSet::new(),
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
        self.had_scopes && self.scopes.is_empty() && self.peers.is_empty()
    }

    fn find_mut(&mut self, scope_id: u64) -> Option<&mut ServerScope> {
        self.scopes.iter_mut().find(|s| s.id == scope_id)
    }

    fn change_layout(
        &mut self,
        scope_id: u64,
        change: impl FnOnce(&mut Scope) -> io::Result<bool>,
    ) -> ServerMsg {
        let Some(entry) = self.find_mut(scope_id) else {
            return ServerMsg::ScopeClosed;
        };
        match change(&mut entry.scope) {
            Ok(true) => entry
                .scope
                .snapshot(scope_id, entry.name.clone())
                .map_or(ServerMsg::ScopeClosed, ServerMsg::Snapshot),
            Ok(false) => ServerMsg::Unchanged,
            Err(err) => ServerMsg::Error(err.to_string()),
        }
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
                        host: String::new(),
                    })
                    .collect();
                ServerMsg::Scopes(infos)
            }
            ClientMsg::ListHosts => ServerMsg::Hosts(self.peers.clone()),
            ClientMsg::AddHost { ssh_host } => {
                let ssh_host = ssh_host.trim().to_string();
                if ssh_host.is_empty() {
                    return ServerMsg::Error("SSH host cannot be empty".into());
                }
                if let Some(existing) = self.peers.iter().find(|peer| peer.ssh_host == ssh_host) {
                    return ServerMsg::Hosts(vec![existing.clone()]);
                }
                let name = ssh_host.rsplit('@').next().unwrap_or(&ssh_host).to_string();
                let peer = HostInfo { name, ssh_host };
                self.peers.push(peer.clone());
                self.peer_masters.push(None);
                match save_peers(&self.peers) {
                    Ok(()) => ServerMsg::Hosts(vec![peer]),
                    Err(error) => {
                        self.peers.pop();
                        ServerMsg::Error(format!("could not persist SSH host: {error}"))
                    }
                }
            }
            ClientMsg::ConnectPeer { .. } => {
                ServerMsg::Error("ConnectPeer is handled outside the server state lock".into())
            }
            ClientMsg::SyncRemoteScopes { host, mut scopes } => {
                self.remote_scopes.retain(|scope| scope.host != host);
                for scope in &mut scopes {
                    scope.host = host.clone();
                }
                self.remote_scopes.extend(scopes);
                match save_remote_scopes(&self.remote_scopes) {
                    Ok(()) => ServerMsg::Ok,
                    Err(error) => {
                        ServerMsg::Error(format!("could not persist remote scopes: {error}"))
                    }
                }
            }
            ClientMsg::CreateScope { cwd, cols, rows } => {
                let id = self.next_id;
                let area = Rect::new(0, 0, cols.max(1), rows.max(1));
                // A client can request a scope on another machine while its
                // current directory only exists locally. Start in the remote
                // user's home directory in that case instead of letting the
                // remote shell exit immediately on an invalid cwd.
                let cwd = if cwd.is_dir() {
                    cwd
                } else {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .filter(|home| home.is_dir())
                        .or_else(|| std::env::current_dir().ok())
                        .unwrap_or_else(|| PathBuf::from("/"))
                };
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
            ClientMsg::CreateScopeOn { .. } => {
                ServerMsg::Error("CreateScopeOn is only valid on a client node".into())
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
                            host: String::new(),
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
            ClientMsg::DropProjection {
                scope_id,
                source_id,
                target_id,
                action,
                whole_stack,
            } => self.change_layout(scope_id, |scope| {
                scope.drop_projection(source_id, target_id, action, whole_stack)
            }),
            ClientMsg::MoveProjection { scope_id, dir } => {
                self.change_layout(scope_id, |scope| scope.move_projection(to_nav_dir(dir)))
            }
            ClientMsg::ActivateProjection {
                scope_id,
                projection_id,
            } => self.change_layout(scope_id, |scope| {
                Ok(scope.activate_projection(projection_id))
            }),
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

impl Drop for ServerState {
    fn drop(&mut self) {
        for child in self.peer_masters.iter_mut().flatten() {
            let _ = child.kill();
            let _ = child.wait();
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
                        if let ClientMsg::ConnectPeer { ssh_host, askpass } = msg {
                            let reply = ensure_peer_master(&state, &ssh_host, &askpass);
                            if encrypted.send(&reply).is_err() {
                                break;
                            }
                            continue;
                        }
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

fn ensure_peer_master(
    state: &Arc<Mutex<ServerState>>,
    ssh_host: &str,
    askpass: &AskpassConfig,
) -> ServerMsg {
    let mut index = None;
    for _ in 0..1200 {
        let mut guard = state.lock().unwrap();
        let Some(found) = guard
            .peers
            .iter()
            .position(|peer| peer.ssh_host == ssh_host)
        else {
            return ServerMsg::Error(format!("SSH host is not registered: {ssh_host}"));
        };
        if let Some(child) = &mut guard.peer_masters[found] {
            match child.try_wait() {
                Ok(None) => return ServerMsg::Ok,
                Ok(Some(_)) | Err(_) => guard.peer_masters[found] = None,
            }
        }
        if !guard.connecting_peers.contains(ssh_host) {
            guard.connecting_peers.insert(ssh_host.to_string());
            index = Some(found);
            break;
        }
        drop(guard);
        thread::sleep(Duration::from_millis(50));
    }
    let Some(index) = index else {
        return ServerMsg::Error(format!("timed out waiting for SSH peer {ssh_host}"));
    };

    let result = start_ssh_peer_master(ssh_host, askpass);
    let mut guard = state.lock().unwrap();
    guard.connecting_peers.remove(ssh_host);
    match result {
        Ok(child) => {
            guard.peer_masters[index] = child;
            ServerMsg::Ok
        }
        Err(error) => ServerMsg::Error(error.to_string()),
    }
}

pub fn connect_or_spawn(addr: &str) -> Result<ClientConnection> {
    if let Ok(stream) = TcpStream::connect(addr) {
        return Ok(ClientConnection {
            stream: EncryptedStream::handshake_client(stream)?,
            peers: Vec::new(),
            pending_peer: None,
            aggregating_scopes: false,
            cell_px: None,
            _tunnel: None,
            _ssh_control_dir: None,
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
                    peers: Vec::new(),
                    pending_peer: None,
                    aggregating_scopes: false,
                    cell_px: None,
                    _tunnel: None,
                    _ssh_control_dir: None,
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
        peers: Vec::new(),
        pending_peer: None,
        aggregating_scopes: false,
        cell_px: None,
        _tunnel: None,
        _ssh_control_dir: None,
        pending: VecDeque::new(),
        trace: VecDeque::new(),
    })
}

/// Ask the local Harp server to own the SSH master, then open a short-lived
/// forwarding channel over that master for this client.
pub fn connect_ssh_with_askpass(
    host: &str,
    askpass: &AskpassConfig,
    local_addr: &str,
) -> Result<ClientConnection> {
    anyhow::ensure!(!host.trim().is_empty(), "SSH host cannot be empty");
    let mut local = connect_or_spawn(local_addr)?;
    local.send(&ClientMsg::ConnectPeer {
        ssh_host: host.to_string(),
        askpass: askpass.clone(),
    })?;
    match local.recv::<ServerMsg>()? {
        ServerMsg::Ok => {}
        ServerMsg::Error(error) => anyhow::bail!("{error}"),
        _ => anyhow::bail!("invalid response while connecting to SSH peer {host}"),
    }
    connect_ssh_tunnel(host)
}

fn connect_ssh_tunnel(host: &str) -> Result<ClientConnection> {
    let control_path = control_path_for_host(host)?;

    let reservation = TcpListener::bind("127.0.0.1:0")?;
    let local_port = reservation.local_addr()?.port();
    drop(reservation);

    let mut tunnel = Command::new("ssh")
        .arg("-S")
        .arg(&control_path)
        .arg("-o")
        .arg("ControlMaster=no")
        .arg("-o")
        .arg("BatchMode=yes")
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
        if let Ok(stream) = TcpStream::connect(&addr)
            && let Ok(stream) = EncryptedStream::handshake_client(stream)
        {
            return Ok(ClientConnection {
                stream,
                peers: Vec::new(),
                pending_peer: None,
                aggregating_scopes: false,
                cell_px: None,
                _tunnel: Some(tunnel),
                _ssh_control_dir: control_path.parent().map(PathBuf::from),
                pending: VecDeque::new(),
                trace: VecDeque::new(),
            });
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

fn control_master_alive(host: &str, control_path: &std::path::Path) -> bool {
    Command::new("ssh")
        .arg("-S")
        .arg(control_path)
        .arg("-O")
        .arg("check")
        .arg(host)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn start_ssh_peer_master(host: &str, askpass: &AskpassConfig) -> Result<Option<Child>> {
    let control_path = control_path_for_host(host)?;
    if control_master_alive(host, &control_path) {
        return Ok(None);
    }
    let _ = std::fs::remove_file(&control_path);

    let mut master = ssh_command(Some(askpass))
        .arg("-M")
        .arg("-N")
        .arg("-S")
        .arg(&control_path)
        .arg("-o")
        .arg("ControlPersist=no")
        .arg(host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut ready = false;
    for _ in 0..200 {
        if control_path.exists() && control_master_alive(host, &control_path) {
            ready = true;
            break;
        }
        if let Some(status) = master.try_wait()? {
            anyhow::bail!("SSH master exited before connecting to {host} ({status})");
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !ready {
        let _ = master.kill();
        let _ = master.wait();
        anyhow::bail!("timed out opening SSH master connection to {host}");
    }

    let setup_result = (|| -> Result<()> {
        let remote_args = vec![
            "-S".to_string(),
            control_path.display().to_string(),
            "-o".into(),
            "ControlMaster=no".into(),
            host.to_string(),
            remote_ensure_command(),
        ];
        let status = ssh_command(Some(askpass))
            .args(&remote_args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            upload_and_start_remote(host, &control_path, Some(askpass))?;
        }
        Ok(())
    })();
    if let Err(error) = setup_result {
        let _ = master.kill();
        let _ = master.wait();
        return Err(error);
    }
    Ok(Some(master))
}

/// Send the currently running executable over SSH when the remote `harp`
/// command is unavailable. The remote copy is private, executable, and removed
/// as soon as it has launched the detached server.
fn upload_and_start_remote(
    host: &str,
    control_path: &std::path::Path,
    askpass: Option<&AskpassConfig>,
) -> Result<()> {
    let exe = File::open(std::env::current_exe()?)?;
    let script = format!(
        "umask 077; f=$(mktemp /tmp/harp.XXXXXX) || exit 1; cat > \"$f\" || {{ rm -f \"$f\"; exit 1; }}; chmod 700 \"$f\" || {{ rm -f \"$f\"; exit 1; }}; {} \"$f\" --ensure-server; r=$?; rm -f \"$f\"; exit \"$r\"",
        remote_env_prefix()
    );
    let mut child = ssh_command(askpass)
        .arg("-S")
        .arg(control_path)
        .arg("-o")
        .arg("ControlMaster=no")
        .arg(host)
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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

fn control_path_for_host(host: &str) -> Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    host.hash(&mut hasher);
    #[cfg(unix)]
    let user_id = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let user_id = 0u32;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = runtime.join(format!("harp-ssh-{user_id}"));
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir.join(format!("c-{:x}", hasher.finish())))
}

fn ssh_command(askpass: Option<&AskpassConfig>) -> Command {
    let mut command = Command::new("ssh");
    if let Some(config) = askpass {
        command
            .env("SSH_ASKPASS", &config.program)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("HARP_ASKPASS_HELPER", "1")
            .env("HARP_ASKPASS_ADDR", &config.addr)
            .env("HARP_ASKPASS_TOKEN", &config.token);
        if std::env::var_os("DISPLAY").is_none() {
            command.env("DISPLAY", ":0");
        }
    }
    command
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
