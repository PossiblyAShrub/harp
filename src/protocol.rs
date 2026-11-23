use std::path::PathBuf;

use crossterm::event::Event;
use serde::{Deserialize, Serialize};

pub const DEBUG_PROJECTION_PROGRAM: &str = "__harp_server_debug__";
pub const DEBUG_LOG_PROJECTION_PROGRAM: &str = "__harp_debug_log__";
pub const PROTOCOL_VERSION: u32 = 3;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NavDir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TabDir {
    Next,
    Prev,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DropAction {
    Swap,
    Tab,
    TabBefore,
    SplitHorizontal,
    SplitHorizontalBefore,
    SplitVertical,
    SplitVerticalBefore,
}

impl DropAction {
    pub fn is_split(self) -> bool {
        !matches!(self, Self::Swap | Self::Tab | Self::TabBefore)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteScopeSpec {
    pub machine: String,
    pub cwd: PathBuf,
    pub program: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScopeInfo {
    pub id: u64,
    pub name: String,
    pub cwd: PathBuf,
    pub pane_count: usize,
    /// Owning node label. Empty means the node serving this client.
    #[serde(default)]
    pub host: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    pub name: String,
    pub ssh_host: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AskpassConfig {
    pub program: PathBuf,
    pub addr: String,
    pub token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabPageSnapshot {
    pub id: usize,
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub state: Vec<u8>,
    /// Serialized scrollback history. Empty when it hasn't changed since the
    /// previous snapshot (or when the pane is on the alternate screen).
    pub scrollback: Vec<u8>,
    /// Whether the application is using the alternate screen buffer.
    pub alternate_screen: bool,
    #[serde(default)]
    pub is_debug: bool,
    #[serde(default)]
    pub is_debug_log: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotChild {
    pub fill: u16,
    pub node: SnapshotNode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SnapshotNode {
    Leaf {
        id: usize,
        #[serde(default)]
        name: String,
        cols: u16,
        rows: u16,
        state: Vec<u8>,
        scrollback: Vec<u8>,
        /// Whether the application is using the alternate screen buffer.
        alternate_screen: bool,
    },
    Debug {
        id: usize,
    },
    DebugLog {
        id: usize,
    },
    Split {
        direction: SplitDir,
        children: Vec<SnapshotChild>,
    },
    Tabs {
        pages: Vec<TabPageSnapshot>,
        active: usize,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScopeSnapshot {
    pub scope_id: u64,
    pub scope_name: String,
    pub revision: u64,
    pub focused_id: usize,
    pub root: SnapshotNode,
    /// Terminal escape sequences (e.g. kitty graphics protocol APC codes) that
    /// the client must write through to the real terminal for display.
    /// Always ASCII (base64 payloads), so a `String` keeps the JSON framing
    /// compact instead of one array entry per byte.
    pub passthrough: String,
    #[serde(default)]
    pub debug_log: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    CheckVersion {
        protocol: u32,
        version: String,
    },
    /// Reports the real terminal's character cell size in pixels so the server
    /// can report pixel sizes through TIOCGWINSZ and answer size queries.
    SetPixelSize {
        cell_w: u16,
        cell_h: u16,
    },
    ListScopes,
    ListHosts,
    AddHost {
        ssh_host: String,
    },
    ConnectPeer {
        ssh_host: String,
        askpass: AskpassConfig,
    },
    SyncRemoteScopes {
        host: String,
        scopes: Vec<ScopeInfo>,
    },
    CreateScope {
        cwd: PathBuf,
        cols: u16,
        rows: u16,
    },
    CreateScopeOn {
        host: String,
        cwd: PathBuf,
        cols: u16,
        rows: u16,
    },
    AttachScope {
        scope_id: u64,
        cols: u16,
        rows: u16,
    },
    RenameScope {
        scope_id: u64,
        name: String,
    },
    Poll {
        scope_id: u64,
        last_revision: u64,
    },
    Resize {
        scope_id: u64,
        cols: u16,
        rows: u16,
    },
    Input {
        scope_id: u64,
        event: Event,
    },
    /// Input on the interactive fast path. The server processes it without a
    /// reply; the client's next Poll retrieves the resulting screen state.
    InputAsync {
        scope_id: u64,
        event: Event,
    },
    /// Opts this connection into server-pushed snapshots for `scope_id`
    /// whenever its revision advances past `last_revision`. No reply.
    Subscribe {
        scope_id: u64,
        last_revision: u64,
    },
    CloseProjection {
        scope_id: u64,
        projection_id: usize,
    },
    MoveFocus {
        scope_id: u64,
        dir: NavDir,
    },
    DropProjection {
        scope_id: u64,
        source_id: usize,
        target_id: usize,
        action: DropAction,
        whole_stack: bool,
    },
    MoveProjection {
        scope_id: u64,
        dir: NavDir,
    },
    ActivateProjection {
        scope_id: u64,
        projection_id: usize,
    },
    Split {
        scope_id: u64,
        dir: NavDir,
        spec: RemoteScopeSpec,
    },
    NewTab {
        scope_id: u64,
        spec: RemoteScopeSpec,
    },
    SwitchTab {
        scope_id: u64,
        dir: TabDir,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    Ok,
    Version {
        protocol: u32,
        version: String,
    },
    Scopes(Vec<ScopeInfo>),
    Hosts(Vec<HostInfo>),
    Snapshot(ScopeSnapshot),
    PushSnapshot(ScopeSnapshot),
    PushScopeClosed(u64),
    Unchanged,
    ScopeClosed,
    Error(String),
}
