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
    },
    Split {
        direction: SplitDir,
        children: Vec<SnapshotChild>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScopeSnapshot {
    pub scope_id: u64,
    pub scope_name: String,
    pub revision: u64,
    pub focused_id: usize,
    pub root: SnapshotNode,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMsg {
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
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    Scopes(Vec<ScopeInfo>),
    Snapshot(ScopeSnapshot),
    Unchanged,
    ScopeClosed,
    Error(String),
}
