use std::path::PathBuf;

use crossterm::event::Event;
use serde::{Deserialize, Serialize};

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
        cols: u16,
        rows: u16,
        state: Vec<u8>,
        scrollback: Vec<u8>,
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
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Reports the real terminal's character cell size in pixels so the server
    /// can report pixel sizes through TIOCGWINSZ and answer size queries.
    SetPixelSize {
        cell_w: u16,
        cell_h: u16,
    },
    ListScopes,
    CreateScope {
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
    MoveFocus {
        scope_id: u64,
        dir: NavDir,
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
    Scopes(Vec<ScopeInfo>),
    Snapshot(ScopeSnapshot),
    Unchanged,
    ScopeClosed,
    Error(String),
}
