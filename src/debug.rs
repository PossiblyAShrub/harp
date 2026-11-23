use std::{
    collections::VecDeque,
    io::Write,
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Result;

use crate::projections::{CommandProgram, ProjectionProgram, ProjectionTerm};

const MAX_LOG_ENTRIES: usize = 500;

struct EventLog {
    entries: Mutex<VecDeque<(u64, String)>>,
    seq: AtomicU64,
}

impl EventLog {
    fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(MAX_LOG_ENTRIES)),
            seq: AtomicU64::new(0),
        }
    }

    fn push(&self, line: String) {
        let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= MAX_LOG_ENTRIES {
            entries.pop_front();
        }
        entries.push_back((seq, line));
        crate::projections::wake_watchers();
    }

    fn since(&self, after_seq: u64) -> (u64, Vec<String>) {
        let entries = self.entries.lock().unwrap();
        let mut latest = after_seq;
        let mut out = Vec::new();
        for (seq, line) in entries.iter() {
            if *seq > after_seq {
                latest = *seq;
                out.push(line.clone());
            }
        }
        (latest, out)
    }
}

static DEBUG_LOG: OnceLock<EventLog> = OnceLock::new();
static SERVER_TRACE_LOG: OnceLock<EventLog> = OnceLock::new();

fn debug_log() -> &'static EventLog {
    DEBUG_LOG.get_or_init(EventLog::new)
}

fn server_trace_log() -> &'static EventLog {
    SERVER_TRACE_LOG.get_or_init(EventLog::new)
}

pub fn log_debug_event(event: impl Into<String>) {
    let seq = debug_log().seq.load(Ordering::Relaxed) + 1;
    debug_log().push(format!("#{seq:<4} {}", event.into()));
}

pub fn log_server_trace(request: &str, response: &str, latency: Duration) {
    let ms = latency.as_secs_f64() * 1000.0;
    server_trace_log().push(format!(
        "\x1b[36m{ms:>8.2} ms\x1b[0m  \x1b[33m{request}\x1b[0m → \x1b[32m{response}\x1b[0m"
    ));
}

#[cfg(test)]
pub fn recent_debug_events_since(after_seq: u64) -> (u64, Vec<String>) {
    debug_log().since(after_seq)
}

fn stream_log(log: &EventLog, title: &str, mut stream: UnixStream, closed: &AtomicBool) {
    let _ = write!(stream, "\x1b]2;{title}\x07");
    let _ = stream.flush();

    let mut last_seq = 0u64;
    let mut watcher = crate::projections::watcher_seq();
    while !closed.load(Ordering::Acquire) {
        let (next_seq, lines) = log.since(last_seq);
        if !lines.is_empty() {
            for line in lines {
                if stream.write_all(line.as_bytes()).is_err() || stream.write_all(b"\r\n").is_err()
                {
                    return;
                }
            }
            if stream.flush().is_err() {
                return;
            }
            last_seq = next_seq;
        }
        watcher = crate::projections::wait_watchers(watcher, Duration::from_millis(250));
    }
}

pub struct ServerDebug;

impl ProjectionProgram for ServerDebug {
    fn label(&self) -> &str {
        "Server Debug"
    }

    fn program(&self) -> &str {
        "__harp_server_debug__"
    }

    fn launch(&self, cols: u16, rows: u16, _cwd: &Path) -> Result<ProjectionTerm> {
        ProjectionTerm::with_builtin(cols, rows, "Server Debug", |stream, closed| {
            stream_log(server_trace_log(), "Server Debug", stream, closed);
        })
    }
}

pub struct DebugLog;

impl ProjectionProgram for DebugLog {
    fn label(&self) -> &str {
        "Debug Log"
    }

    fn program(&self) -> &str {
        "__harp_debug_log__"
    }

    fn launch(&self, cols: u16, rows: u16, _cwd: &Path) -> Result<ProjectionTerm> {
        ProjectionTerm::with_builtin(cols, rows, "Debug Log", |stream, closed| {
            stream_log(debug_log(), "Debug Log", stream, closed);
        })
    }
}

pub fn builtins() -> &'static [&'static dyn ProjectionProgram] {
    &[&ServerDebug, &DebugLog]
}

/// Resolves any program string polymorphically to either a registered builtin
/// or an external terminal command.
pub fn launch_program(program: &str, cols: u16, rows: u16, cwd: &Path) -> Result<ProjectionTerm> {
    if let Some(&builtin) = builtins().iter().find(|b| b.program() == program) {
        return builtin.launch(cols, rows, cwd);
    }
    CommandProgram {
        label: program,
        program,
    }
    .launch(cols, rows, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_log_records_events_incrementally() {
        let (seq_before, _) = recent_debug_events_since(0);
        log_debug_event("title received (OSC 0/2): \"hello\"");
        let (seq_after, events) = recent_debug_events_since(seq_before);
        assert!(seq_after > seq_before);
        assert!(
            events
                .iter()
                .any(|e| e.contains("title received (OSC 0/2): \"hello\""))
        );
    }
}
