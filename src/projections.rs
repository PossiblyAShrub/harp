use std::{
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    path::Path,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

static WATCHERS: OnceLock<(Mutex<u64>, Condvar)> = OnceLock::new();

fn watchers() -> &'static (Mutex<u64>, Condvar) {
    WATCHERS.get_or_init(|| (Mutex::new(0), Condvar::new()))
}

pub fn wake_watchers() {
    let (lock, cvar) = watchers();
    let mut seq = lock.lock().unwrap();
    *seq = seq.wrapping_add(1);
    cvar.notify_all();
}

pub fn watcher_seq() -> u64 {
    *watchers().0.lock().unwrap()
}

pub fn wait_watchers(last_seq: u64, timeout: Duration) -> u64 {
    let (lock, cvar) = watchers();
    let seq = lock.lock().unwrap();
    if *seq != last_seq {
        return *seq;
    }
    let (guard, _) = cvar.wait_timeout(seq, timeout).unwrap();
    *guard
}

use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::Buffer,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use tui_term::{
    vt100::{self, MouseProtocolEncoding, MouseProtocolMode},
    widget::PseudoTerminal,
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    Interacting,
    Managing,
}

/// Maximum number of scrolled-off rows kept per terminal projection.
pub const SCROLLBACK_LINES: usize = 1000;

pub struct ProjectionStatus {
    mode: Mode,
    error: Option<String>,
    scope_name: Option<String>,
}

impl ProjectionStatus {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            error: None,
            scope_name: None,
        }
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    pub fn set_scope_name(&mut self, scope_name: Option<String>) {
        self.scope_name = scope_name;
    }
}

impl Widget for &ProjectionStatus {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let scope_label = self.scope_name.as_deref().map(|name| format!(" {name} "));
        let right_width = scope_label
            .as_ref()
            .map_or(0, |s| s.chars().count().min(area.width as usize) as u16);

        let chunks = Layout::new(
            Direction::Horizontal,
            [Constraint::Min(0), Constraint::Length(right_width)],
        )
        .split(area);

        if let Some(err) = &self.error {
            let line = Line::from(vec![
                Span::styled(
                    " ERROR ",
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {err}"), Style::default().fg(Color::Red)),
            ]);
            Paragraph::new(line).render(chunks[0], buf);
        } else {
            let (badge, badge_style, hint) = match self.mode {
                Mode::Interacting => (
                    " INTERACTING ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                    " Ctrl+Space: manage | App keys pass through",
                ),
                Mode::Managing => (
                    " MANAGING ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                    " c: copy selection | v: paste | Enter: interact | Arrows: focus | Ctrl+Arrows: split | Shift+Arrows: projection | Alt+Arrows: tabs | Ctrl+Alt+Arrows: move | drag top border or tab | Shift+drop: tab | t/Shift+t: new tab | s: scopes | r: rename | q q: close | d: detach",
                ),
            };

            let line = Line::from(vec![
                Span::styled(badge, badge_style),
                Span::styled(hint, Style::default().fg(Color::Gray)),
            ]);

            Paragraph::new(line).render(chunks[0], buf);
        }

        if let Some(label) = scope_label {
            Paragraph::new(Span::styled(
                label,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .render(chunks[1], buf);
        }
    }
}

pub trait ProjectionProgram: Sync {
    fn label(&self) -> &str;
    fn program(&self) -> &str;
    fn launch(&self, cols: u16, rows: u16, cwd: &Path) -> Result<ProjectionTerm>;
}

pub struct CommandProgram<'a> {
    pub label: &'a str,
    pub program: &'a str,
}

impl ProjectionProgram for CommandProgram<'_> {
    fn label(&self) -> &str {
        self.label
    }

    fn program(&self) -> &str {
        self.program
    }

    fn launch(&self, cols: u16, rows: u16, cwd: &Path) -> Result<ProjectionTerm> {
        ProjectionTerm::with_command(cols, rows, self.program, cwd)
    }
}

#[derive(Default)]
struct TitleCallbacks {
    window_title: Option<String>,
    icon_name: Option<String>,
}

impl TitleCallbacks {
    fn title(&self) -> Option<&str> {
        self.window_title.as_deref().or(self.icon_name.as_deref())
    }
}

impl vt100::Callbacks for TitleCallbacks {
    fn set_window_icon_name(&mut self, _: &mut vt100::Screen, icon_name: &[u8]) {
        let raw = String::from_utf8_lossy(icon_name);
        crate::debug::log_debug_event(format!("title received (OSC 1 icon): {raw:?}"));
        self.icon_name = sanitize_title(icon_name);
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        let raw = String::from_utf8_lossy(title);
        crate::debug::log_debug_event(format!("title received (OSC 0/2): {raw:?}"));
        self.window_title = sanitize_title(title);
    }
}

fn sanitize_title(bytes: &[u8]) -> Option<String> {
    let cleaned: String = String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub struct ProjectionTerm {
    master: Option<Box<dyn MasterPty + Send>>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    closed: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    parser: Arc<Mutex<vt100::Parser<TitleCallbacks>>>,
    exited: Arc<AtomicBool>,
    revision: Arc<AtomicU64>,
    size: (u16, u16),
    program: String,
    /// Character cell size in pixels, as reported by the real terminal. Used
    /// for the PTY's TIOCGWINSZ pixel size and to answer size queries.
    cell_px: Arc<Mutex<(u16, u16)>>,
    /// Escape sequences waiting to be forwarded to the real terminal.
    passthrough: Arc<Mutex<Vec<u8>>>,
    /// Hash of the scrollback serialization from the last snapshot, used to
    /// avoid re-sending unchanged history on every poll.
    last_scrollback_hash: AtomicU64,
}

impl Drop for ProjectionTerm {
    fn drop(&mut self) {
        // Signal any in-process builtin and drop the writer to send EOF,
        // letting the reader thread's blocking read return.
        self.closed.store(true, Ordering::Release);
        *self.writer.lock().unwrap() = None;
        wake_watchers();
    }
}

impl ProjectionTerm {
    pub fn new() -> Result<Self> {
        Self::with_size(80, 24)
    }

    pub fn with_size(cols: u16, rows: u16) -> Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new("/").to_path_buf());
        Self::with_command(cols, rows, "zsh", &cwd)
    }

    pub fn with_command(cols: u16, rows: u16, program: &str, cwd: &Path) -> Result<Self> {
        let cols = cols.max(1);
        let rows = rows.max(1);

        let pty = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(program);
        cmd.cwd(cwd);
        // Child applications run inside harp's vt100 terminal emulator rather
        // than directly in the host terminal. Force TERM=xterm-256color so
        // apps don't emit host-specific sequences (e.g. xterm-kitty/ghostty
        // HPA or kitty keyboard extensions) that vt100 doesn't implement.
        cmd.env("TERM", "xterm-256color");
        let mut child = pty.slave.spawn_command(cmd)?;
        crate::debug::log_debug_event(format!("started projection: {program}"));
        let killer = child.clone_killer();
        let reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;

        Self::from_io(
            reader,
            writer,
            Some(pty.master),
            Some(killer),
            cols,
            rows,
            program,
            move || {
                let _ = child.wait();
            },
        )
    }

    /// Creates an in-process projection connected to the other end of a
    /// bidirectional stream instead of spawning an external child process.
    pub fn with_builtin(
        cols: u16,
        rows: u16,
        name: &str,
        run: impl FnOnce(std::os::unix::net::UnixStream, &AtomicBool) + Send + 'static,
    ) -> Result<Self> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let (term_side, builtin_side) = std::os::unix::net::UnixStream::pair()?;
        let reader = Box::new(term_side.try_clone()?);
        let writer = Box::new(term_side);
        let term = Self::from_io(reader, writer, None, None, cols, rows, name, || {})?;
        let closed = Arc::clone(&term.closed);
        thread::spawn(move || {
            run(builtin_side, &closed);
        });
        Ok(term)
    }

    #[allow(clippy::too_many_arguments)]
    fn from_io(
        mut reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        master: Option<Box<dyn MasterPty + Send>>,
        killer: Option<Box<dyn ChildKiller + Send + Sync>>,
        cols: u16,
        rows: u16,
        program: &str,
        on_eof: impl FnOnce() + Send + 'static,
    ) -> Result<Self> {
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            SCROLLBACK_LINES,
            TitleCallbacks::default(),
        )));
        let exited = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let revision = Arc::new(AtomicU64::new(1));

        let writer = Arc::new(Mutex::new(Some(writer)));
        let cell_px = Arc::new(Mutex::new((0u16, 0u16)));
        let passthrough = Arc::new(Mutex::new(Vec::new()));
        let parser_clone = Arc::clone(&parser);
        let exited_clone = Arc::clone(&exited);
        let revision_clone = Arc::clone(&revision);
        let writer_clone = Arc::clone(&writer);
        let cell_px_clone = Arc::clone(&cell_px);
        let passthrough_clone = Arc::clone(&passthrough);

        thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            let mut filter = PassthroughFilter::new();
            let mut sync_buf = Vec::new();
            const MAX_SYNC_BUF: usize = 1024 * 1024;

            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }

                let mut screen = Vec::new();
                let mut passthrough = Vec::new();
                let mut responses = Vec::new();
                let cell_px = *cell_px_clone.lock().unwrap();
                let (size, cursor) = {
                    let parser = parser_clone.lock().unwrap();
                    (parser.screen().size(), parser.screen().cursor_position())
                };
                filter.feed(
                    &buffer[..n],
                    &mut screen,
                    &mut passthrough,
                    &mut responses,
                    cell_px,
                    size,
                    cursor,
                );

                if !responses.is_empty()
                    && let Some(writer) = writer_clone.lock().unwrap().as_mut()
                {
                    let _ = writer.write_all(&responses);
                    let _ = writer.flush();
                }
                if !passthrough.is_empty() {
                    passthrough_clone
                        .lock()
                        .unwrap()
                        .extend_from_slice(&passthrough);
                }

                if filter.in_sync_update() && sync_buf.len() + screen.len() <= MAX_SYNC_BUF {
                    sync_buf.extend_from_slice(&screen);
                    continue;
                }

                if !sync_buf.is_empty() {
                    sync_buf.extend_from_slice(&screen);
                    parser_clone.lock().unwrap().process(&sync_buf);
                    sync_buf.clear();
                } else if !screen.is_empty() {
                    parser_clone.lock().unwrap().process(&screen);
                }
                revision_clone.fetch_add(1, Ordering::Release);
                wake_watchers();
            }

            if !sync_buf.is_empty() {
                parser_clone.lock().unwrap().process(&sync_buf);
                revision_clone.fetch_add(1, Ordering::Release);
                wake_watchers();
            }

            on_eof();
            exited_clone.store(true, Ordering::Release);
            revision_clone.fetch_add(1, Ordering::Release);
            wake_watchers();
        });

        Ok(Self {
            master,
            killer,
            closed,
            writer,
            parser,
            exited,
            revision,
            size: (cols, rows),
            program: program.to_string(),
            cell_px,
            passthrough,
            last_scrollback_hash: AtomicU64::new(u64::MAX),
        })
    }

    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    pub fn title(&self) -> String {
        if let Some(title) = self.parser.lock().unwrap().callbacks().title() {
            return title.to_string();
        }
        Path::new(&self.program)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.program.clone())
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub fn state_snapshot(&self) -> (u16, u16, Vec<u8>, Vec<u8>, bool) {
        let mut parser = self.parser.lock().unwrap();
        let (rows, cols) = parser.screen().size();
        let state = parser.screen().state_formatted();

        // Scrollback only exists for the main screen; while an application is
        // on the alternate screen (vim, less, ...) there is nothing to scroll
        // and the history must not be repainted underneath it.
        let scrollback = if parser.screen().alternate_screen() {
            Vec::new()
        } else {
            let history = serialize_history(&mut parser);
            let hash = hash_bytes(&history);
            if hash == self.last_scrollback_hash.load(Ordering::Acquire) {
                Vec::new()
            } else {
                self.last_scrollback_hash.store(hash, Ordering::Release);
                pad_scrollback(history, rows)
            }
        };

        let alternate_screen = parser.screen().alternate_screen();
        (cols, rows, state, scrollback, alternate_screen)
    }

    pub fn handle_event(&mut self, event: &Event) -> io::Result<()> {
        match event {
            Event::Key(key_event) => self.handle_key_event(*key_event),
            Event::Mouse(mouse_event) => self.handle_mouse_event(*mouse_event),
            Event::Paste(text) => self.handle_paste(text),
            Event::Resize(cols, rows) => self.resize(*cols, *rows),
            Event::FocusGained | Event::FocusLost => Ok(()),
        }
    }

    pub fn force_close(&mut self) {
        self.closed.store(true, Ordering::Release);
        if let Some(killer) = &mut self.killer {
            let _ = killer.kill();
        }
        *self.writer.lock().unwrap() = None;
        wake_watchers();
    }

    pub fn handle_key_event(&mut self, key_event: KeyEvent) -> io::Result<()> {
        let application_cursor = self.parser.lock().unwrap().screen().application_cursor();
        if let Some(bytes) = encode_key_event(key_event, application_cursor)
            && let Some(writer) = self.writer.lock().unwrap().as_mut()
        {
            writer.write_all(&bytes)?;
            writer.flush()?;
        }

        Ok(())
    }

    pub fn handle_mouse_event(&mut self, mouse_event: MouseEvent) -> io::Result<()> {
        let (mode, encoding) = {
            let parser = self.parser.lock().unwrap();
            let screen = parser.screen();
            (
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
            )
        };

        if let Some(bytes) = encode_mouse_event(mouse_event, mode, encoding)
            && let Some(writer) = self.writer.lock().unwrap().as_mut()
        {
            writer.write_all(&bytes)?;
            writer.flush()?;
        }

        Ok(())
    }

    pub fn handle_paste(&mut self, text: &str) -> io::Result<()> {
        let bracketed = self.parser.lock().unwrap().screen().bracketed_paste();
        let mut guard = self.writer.lock().unwrap();
        let Some(writer) = guard.as_mut() else {
            return Ok(());
        };
        if bracketed {
            writer.write_all(b"\x1b[200~")?;
            let sanitized = text.replace("\x1b[201~", "");
            writer.write_all(sanitized.as_bytes())?;
            writer.write_all(b"\x1b[201~")?;
        } else {
            writer.write_all(text.replace('\n', "\r").as_bytes())?;
        }
        writer.flush()
    }

    /// Sets the character cell size in pixels and applies it to the PTY so the
    /// child sees it through TIOCGWINSZ and size queries.
    pub fn set_cell_size(&mut self, cell_w: u16, cell_h: u16) -> io::Result<()> {
        *self.cell_px.lock().unwrap() = (cell_w, cell_h);
        self.apply_pixel_size()
    }

    fn apply_pixel_size(&mut self) -> io::Result<()> {
        let Some(master) = &self.master else {
            return Ok(());
        };
        let (cols, rows) = self.size;
        let (cell_w, cell_h) = *self.cell_px.lock().unwrap();
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: cell_w.saturating_mul(cols),
                pixel_height: cell_h.saturating_mul(rows),
            })
            .map_err(io::Error::other)
    }

    /// Drains the escape sequences queued up to be forwarded to the real
    /// terminal (kitty graphics protocol and friends).
    pub fn take_passthrough(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.passthrough.lock().unwrap())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let cols = cols.max(1);
        let rows = rows.max(1);

        if self.size == (cols, rows) {
            return Ok(());
        }

        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
        if let Some(master) = &self.master {
            let (cell_w, cell_h) = *self.cell_px.lock().unwrap();
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: cell_w.saturating_mul(cols),
                    pixel_height: cell_h.saturating_mul(rows),
                })
                .map_err(io::Error::other)?;
        }
        self.size = (cols, rows);
        self.revision.fetch_add(1, Ordering::Release);

        Ok(())
    }
}

/// Splits a pane's output stream into bytes for the terminal screen, bytes to
/// pass through to the real terminal (kitty graphics protocol APC codes) and
/// answers to terminal size queries.
///
/// The stream is scanned one byte at a time and can span multiple `feed`
/// calls, so the state carries over between reads.
struct PassthroughFilter {
    state: FilterState,
    in_sync_update: bool,
    last_char: Option<[u8; 4]>,
    last_char_len: u8,
    utf8_buf: [u8; 4],
    utf8_len: u8,
    utf8_expected: u8,
    /// Tracked 0-based cursor column (`col == cols` represents pending wrap at
    /// the right margin). Used to clear `vt100`'s pending wrap before cursor
    /// moves (`CUU`/`CUD`/`CUB`/`VPA`/`BS`) that `vt100` otherwise leaves at
    /// `pos.col == cols`.
    col: u16,
    saved_col: u16,
    last_cols: u16,
}

enum FilterState {
    Normal,
    Esc,
    /// Inside a CSI sequence; collects parameter bytes until the final byte.
    Csi {
        params: Vec<u8>,
    },
    /// Inside an APC sequence (graphics transmission), collected for
    /// forwarding.
    Apc {
        buf: Vec<u8>,
        saw_esc: bool,
    },
    /// Inside an OSC sequence; short sequences are buffered to inspect queries,
    /// long ones stream straight through to the screen parser.
    Osc {
        buf: Vec<u8>,
        streaming: bool,
        saw_esc: bool,
    },
}

impl PassthroughFilter {
    fn new() -> Self {
        Self {
            state: FilterState::Normal,
            in_sync_update: false,
            last_char: None,
            last_char_len: 0,
            utf8_buf: [0; 4],
            utf8_len: 0,
            utf8_expected: 0,
            col: 0,
            saved_col: 0,
            last_cols: 0,
        }
    }

    fn in_sync_update(&self) -> bool {
        self.in_sync_update
    }

    /// Advances the tracked column after printing a glyph of `width` cells.
    /// When already at `cols` (pending wrap), the glyph wraps to column 0 of
    /// the next row first, landing at `width`.
    fn advance_col(&mut self, width: u16, cols: u16) {
        if cols == 0 || width == 0 {
            return;
        }
        if self.col >= cols {
            self.col = width.min(cols);
        } else {
            self.col = self.col.saturating_add(width).min(cols);
        }
    }

    /// `vt100` encodes pending wrap as `pos.col == cols` and does not clear it
    /// on `CUU`/`CUD`/`CUB`/`VPA`/`BS`, causing `CUB`/`BS` to be off by 1 and
    /// subsequent text after `CUU`/`CUD`/`VPA` to wrap onto the wrong row.
    /// Emitting `CHA cols` (`CSI cols G`) clamps `pos.col` back to `cols - 1`
    /// without changing the row.
    fn clear_pending_wrap(&mut self, cols: u16, screen: &mut Vec<u8>) {
        if cols > 0 && self.col >= cols {
            screen.extend_from_slice(format!("\x1b[{cols}G").as_bytes());
            self.col = cols - 1;
        }
    }

    /// Mirrors CSI horizontal cursor effects so `self.col` stays aligned with
    /// `vt100` across mid-chunk movements, tabs, REP, and alt-screen switches.
    fn update_col_for_csi(&mut self, params: &[u8], final_byte: u8, cols: u16) {
        if cols == 0 {
            return;
        }
        let max_col = cols.saturating_sub(1);
        match final_byte {
            b'G' | b'`' => {
                let c = parse_csi_u16(params).unwrap_or(1).max(1);
                self.col = c.saturating_sub(1).min(max_col);
            }
            b'H' | b'f' => {
                let c = params
                    .split(|&b| b == b';')
                    .nth(1)
                    .and_then(parse_csi_u16)
                    .unwrap_or(1)
                    .max(1);
                self.col = c.saturating_sub(1).min(max_col);
            }
            b'C' => {
                let n = parse_csi_u16(params).unwrap_or(1).max(1);
                self.col = self.col.saturating_add(n).min(max_col);
            }
            b'D' => {
                let n = parse_csi_u16(params).unwrap_or(1).max(1);
                self.col = self.col.saturating_sub(n);
            }
            b'E' | b'F' => self.col = 0,
            b'I' => {
                let count = parse_csi_u16(params).unwrap_or(1).clamp(1, 256);
                for _ in 0..count {
                    self.col = ((self.col / 8) + 1).saturating_mul(8).min(max_col);
                }
            }
            b'b' if self.last_char.is_some() => {
                let cap = usize::from(cols).clamp(256, 4096);
                let count = (parse_csi_u16(params).unwrap_or(1).max(1) as usize).min(cap);
                for _ in 0..count {
                    self.advance_col(1, cols);
                }
            }
            b'h' if params == b"?1049" => {
                self.saved_col = self.col;
                self.col = 0;
            }
            b'l' if params == b"?1049" => {
                self.col = self.saved_col.min(cols);
            }
            _ => {}
        }
    }

    fn record_normal_byte(&mut self, b: u8, cols: u16) {
        if b < 0x20 || b == 0x7f {
            self.utf8_len = 0;
            self.utf8_expected = 0;
            match b {
                b'\r' => self.col = 0,
                b'\t' if cols > 0 => {
                    self.col = ((self.col / 8) + 1)
                        .saturating_mul(8)
                        .min(cols.saturating_sub(1));
                }
                0x08 => self.col = self.col.saturating_sub(1),
                _ => {}
            }
            return;
        }
        if b < 0x80 {
            self.last_char = Some([b, 0, 0, 0]);
            self.last_char_len = 1;
            self.utf8_len = 0;
            self.utf8_expected = 0;
            self.advance_col(1, cols);
            return;
        }
        if b >= 0xc2 {
            self.utf8_buf[0] = b;
            self.utf8_len = 1;
            self.utf8_expected = if b < 0xe0 {
                2
            } else if b < 0xf0 {
                3
            } else {
                4
            };
        } else if (0x80..=0xbf).contains(&b) && self.utf8_len > 0 {
            let idx = usize::from(self.utf8_len);
            if idx < 4 {
                self.utf8_buf[idx] = b;
                self.utf8_len += 1;
                if self.utf8_len == self.utf8_expected {
                    self.last_char = Some(self.utf8_buf);
                    self.last_char_len = self.utf8_len;
                    self.utf8_len = 0;
                    self.utf8_expected = 0;
                    self.advance_col(1, cols);
                }
            }
        } else {
            self.utf8_len = 0;
            self.utf8_expected = 0;
        }
    }

    /// Processes one chunk of pty output. `screen` gets the bytes that should
    /// go into the vt100 parser, `passthrough` the bytes to forward to the
    /// real terminal, and `responses` the bytes to write back into the pty.
    #[allow(clippy::too_many_arguments)]
    fn feed(
        &mut self,
        bytes: &[u8],
        screen: &mut Vec<u8>,
        passthrough: &mut Vec<u8>,
        responses: &mut Vec<u8>,
        cell_px: (u16, u16),
        size: (u16, u16),
        cursor: (u16, u16),
    ) {
        if size.1 != self.last_cols {
            self.last_cols = size.1;
            self.col = self.col.min(size.1);
        }
        for &b in bytes {
            self.feed_byte(b, screen, passthrough, responses, cell_px, size, cursor);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn feed_byte(
        &mut self,
        b: u8,
        screen: &mut Vec<u8>,
        passthrough: &mut Vec<u8>,
        responses: &mut Vec<u8>,
        cell_px: (u16, u16),
        size: (u16, u16),
        cursor: (u16, u16),
    ) {
        match std::mem::replace(&mut self.state, FilterState::Normal) {
            FilterState::Normal => {
                if b == 0x1b {
                    self.utf8_len = 0;
                    self.utf8_expected = 0;
                    self.state = FilterState::Esc;
                } else {
                    if b == 0x08 {
                        self.clear_pending_wrap(size.1, screen);
                    }
                    self.record_normal_byte(b, size.1);
                    screen.push(b);
                    self.state = FilterState::Normal;
                }
            }
            FilterState::Esc => match b {
                b'[' => {
                    self.state = FilterState::Csi { params: Vec::new() };
                }
                b'_' => {
                    self.state = FilterState::Apc {
                        buf: vec![0x1b, b'_'],
                        saw_esc: false,
                    };
                }
                b']' => {
                    self.state = FilterState::Osc {
                        buf: Vec::new(),
                        streaming: false,
                        saw_esc: false,
                    };
                }
                _ => {
                    screen.push(0x1b);
                    screen.push(b);
                    self.state = FilterState::Normal;
                }
            },
            FilterState::Csi { mut params } => {
                if (0x40..=0x7e).contains(&b) {
                    // Final byte.
                    if params == b"?2026" && (b == b'h' || b == b'l') {
                        self.in_sync_update = b == b'h';
                    } else if let Some(response) = device_query_response(
                        &params,
                        b,
                        cell_px,
                        size,
                        cursor,
                        self.in_sync_update,
                    ) {
                        responses.extend_from_slice(&response);
                    } else {
                        let cols = size.1;
                        // Cursor-movement sequences clear pending wrap in
                        // standard terminals, whereas `vt100` leaves
                        // `pos.col == cols`. Erase sequences (`ED`/`EL`/`ECH`)
                        // are intentionally excluded because they do not clear
                        // pending wrap in standard terminals.
                        let clears_pending_wrap = matches!(b, b'A' | b'B' | b'D' | b'd');
                        if clears_pending_wrap {
                            self.clear_pending_wrap(cols, screen);
                        }
                        self.update_col_for_csi(&params, b, cols);
                        emit_translated_csi(
                            &params,
                            b,
                            size,
                            self.last_char
                                .as_ref()
                                .map(|c| &c[..usize::from(self.last_char_len)]),
                            screen,
                        );
                    }
                    self.state = FilterState::Normal;
                } else {
                    params.push(b);
                    self.state = FilterState::Csi { params };
                }
            }
            FilterState::Apc { mut buf, saw_esc } => {
                buf.push(b);
                match (saw_esc, b) {
                    (true, b'\\') | (false, 0x07) => {
                        // Graphics capability queries are answered here so the
                        // child doesn't wait forever for a reply that would
                        // land on the client's stdin.
                        if let Some(response) = answer_graphics_query(&buf) {
                            responses.extend_from_slice(&response);
                        } else {
                            passthrough.extend_from_slice(&buf);
                        }
                        self.state = FilterState::Normal;
                    }
                    (_, 0x1b) => {
                        self.state = FilterState::Apc { buf, saw_esc: true };
                    }
                    _ => {
                        self.state = FilterState::Apc {
                            buf,
                            saw_esc: false,
                        };
                    }
                }
            }
            FilterState::Osc {
                mut buf,
                mut streaming,
                saw_esc,
            } => {
                const MAX_OSC_QUERY_LEN: usize = 64;
                if streaming {
                    screen.push(b);
                    match (saw_esc, b) {
                        (true, b'\\') | (false, 0x07) => {
                            self.state = FilterState::Normal;
                        }
                        (false, 0x1b) => {
                            self.state = FilterState::Osc {
                                buf,
                                streaming: true,
                                saw_esc: true,
                            };
                        }
                        _ => {
                            self.state = FilterState::Osc {
                                buf,
                                streaming: true,
                                saw_esc: false,
                            };
                        }
                    }
                } else {
                    match (saw_esc, b) {
                        (true, b'\\') => {
                            // `buf` ends with the 0x1b of ST; strip it before matching.
                            let body = buf.strip_suffix(b"\x1b").unwrap_or(&buf);
                            if let Some(resp) = osc_query_response(body, b"\x1b\\") {
                                responses.extend_from_slice(&resp);
                            } else {
                                screen.extend_from_slice(b"\x1b]");
                                screen.extend_from_slice(body);
                                screen.extend_from_slice(b"\x1b\\");
                            }
                            self.state = FilterState::Normal;
                        }
                        (false, 0x07) => {
                            if let Some(resp) = osc_query_response(&buf, b"\x07") {
                                responses.extend_from_slice(&resp);
                            } else {
                                screen.extend_from_slice(b"\x1b]");
                                screen.extend_from_slice(&buf);
                                screen.push(0x07);
                            }
                            self.state = FilterState::Normal;
                        }
                        _ => {
                            buf.push(b);
                            let next_saw_esc = !saw_esc && b == 0x1b;
                            if buf.len() > MAX_OSC_QUERY_LEN {
                                screen.extend_from_slice(b"\x1b]");
                                screen.extend_from_slice(&buf);
                                buf.clear();
                                streaming = true;
                            }
                            self.state = FilterState::Osc {
                                buf,
                                streaming,
                                saw_esc: next_saw_esc,
                            };
                        }
                    }
                }
            }
        }
    }
}

/// Translates CSI sequences that `vt100` does not implement (HPA, HVP, REP,
/// CHT, and colon-separated or extended SGR parameters) into equivalent
/// sequences `vt100` understands so modern TUIs render accurately.
fn emit_translated_csi(
    params: &[u8],
    final_byte: u8,
    size: (u16, u16),
    last_char: Option<&[u8]>,
    screen: &mut Vec<u8>,
) {
    match final_byte {
        // HPA (`CSI n \``) -> CHA (`CSI n G`).
        b'`' => {
            screen.extend_from_slice(b"\x1b[");
            screen.extend_from_slice(params);
            screen.push(b'G');
        }
        // HVP (`CSI r;c f`) -> CUP (`CSI r;c H`).
        b'f' => {
            screen.extend_from_slice(b"\x1b[");
            screen.extend_from_slice(params);
            screen.push(b'H');
        }
        // REP (`CSI n b`): repeat the previous printed character `n` times.
        b'b' => {
            if let Some(ch) = last_char {
                let cap = usize::from(size.1).clamp(256, 4096);
                let count = parse_csi_u16(params).unwrap_or(1).max(1) as usize;
                for _ in 0..count.min(cap) {
                    screen.extend_from_slice(ch);
                }
            }
        }
        // CHT (`CSI n I`): cursor horizontal forward tab `n` times.
        b'I' => {
            let count = parse_csi_u16(params).unwrap_or(1).clamp(1, 256);
            for _ in 0..count {
                screen.push(b'\t');
            }
        }
        // SGR (`CSI ... m`): normalize colon subparameters and strip
        // unsupported codes (like 9/29/58/59) that cause vt100 to abort the
        // rest of the SGR sequence.
        b'm' if needs_sgr_normalization(params) => {
            emit_normalized_sgr(params, screen);
        }
        _ => {
            screen.extend_from_slice(b"\x1b[");
            screen.extend_from_slice(params);
            screen.push(final_byte);
        }
    }
}

fn parse_csi_u16(params: &[u8]) -> Option<u16> {
    if params.is_empty() {
        return None;
    }
    std::str::from_utf8(params).ok()?.parse::<u16>().ok()
}

fn needs_sgr_normalization(params: &[u8]) -> bool {
    params.contains(&b':')
        || params
            .split(|&b| b == b';')
            .any(|p| matches!(p, b"9" | b"29" | b"58" | b"59"))
}

fn emit_normalized_sgr(params: &[u8], screen: &mut Vec<u8>) {
    let mut out_parts: Vec<String> = Vec::new();
    let parts: Vec<&[u8]> = params.split(|&b| b == b';').collect();
    let mut i = 0;
    while i < parts.len() {
        let part = parts[i];
        if part.contains(&b':') {
            let sub: Vec<&[u8]> = part.split(|&b| b == b':').collect();
            match sub.as_slice() {
                // `38:2:[cs]:r:g:b` or `48:2:[cs]:r:g:b`
                [kind @ (b"38" | b"48"), b"2", _, r, g, b]
                | [kind @ (b"38" | b"48"), b"2", r, g, b] => {
                    out_parts.push(format!(
                        "{};2;{};{};{}",
                        String::from_utf8_lossy(kind),
                        String::from_utf8_lossy(r),
                        String::from_utf8_lossy(g),
                        String::from_utf8_lossy(b),
                    ));
                }
                // `38:5:idx` or `48:5:idx`
                [kind @ (b"38" | b"48"), b"5", idx] => {
                    out_parts.push(format!(
                        "{};5;{}",
                        String::from_utf8_lossy(kind),
                        String::from_utf8_lossy(idx),
                    ));
                }
                // Styled underline `4:0`..`4:5`
                [b"4", style] => {
                    if *style == b"0" {
                        out_parts.push("24".to_string());
                    } else {
                        out_parts.push("4".to_string());
                    }
                }
                // Underline color `58:...` -> drop so vt100 doesn't abort.
                [b"58", ..] => {}
                _ => {}
            }
            i += 1;
            continue;
        }
        match part {
            // Semicolon-separated underline color `58;2;r;g;b` or `58;5;i`.
            b"58" => {
                if parts.get(i + 1) == Some(&b"2".as_slice()) {
                    i += 5;
                } else if parts.get(i + 1) == Some(&b"5".as_slice()) {
                    i += 3;
                } else {
                    i += 1;
                }
            }
            b"9" | b"29" | b"59" => {
                i += 1;
            }
            _ => {
                out_parts.push(String::from_utf8_lossy(part).into_owned());
                i += 1;
            }
        }
    }
    if !out_parts.is_empty() {
        screen.extend_from_slice(b"\x1b[");
        screen.extend_from_slice(out_parts.join(";").as_bytes());
        screen.push(b'm');
    }
}

fn osc_query_response(body: &[u8], terminator: &[u8]) -> Option<Vec<u8>> {
    let prefix = match body {
        b"10;?" => b"\x1b]10;rgb:ffff/ffff/ffff".as_slice(),
        b"11;?" => b"\x1b]11;rgb:0000/0000/0000".as_slice(),
        _ => return None,
    };
    let mut resp = Vec::with_capacity(prefix.len() + terminator.len());
    resp.extend_from_slice(prefix);
    resp.extend_from_slice(terminator);
    Some(resp)
}

/// Builds the response to a terminal query, if it is one that the server can
/// answer on behalf of the real terminal.
fn device_query_response(
    params: &[u8],
    final_byte: u8,
    cell_px: (u16, u16),
    size: (u16, u16),
    cursor: (u16, u16),
    in_sync_update: bool,
) -> Option<Vec<u8>> {
    match (params, final_byte) {
        // CSI 16 t: character cell size in pixels.
        (b"16", b't') => {
            let (cell_w, cell_h) = cell_px;
            (cell_w > 0 && cell_h > 0).then(|| format!("\x1b[6;{cell_h};{cell_w}t").into_bytes())
        }
        // CSI 14 t: pane size in pixels.
        (b"14", b't') => {
            let (cell_w, cell_h) = cell_px;
            if cell_w == 0 || cell_h == 0 {
                return None;
            }
            let (rows, cols) = size;
            let w = u32::from(cell_w) * u32::from(cols);
            let h = u32::from(cell_h) * u32::from(rows);
            Some(format!("\x1b[4;{h};{w}t").into_bytes())
        }
        // CSI c / CSI 0 c: primary device attributes (DA). Answer as a VT100-style
        // terminal with advanced video, which is what icat's support
        // detection waits for.
        (b"" | b"0", b'c') => Some(b"\x1b[?1;2c".to_vec()),
        // CSI > ... c: secondary device attributes (DA2).
        ([b'>', ..], b'c') => Some(b"\x1b[>0;0;0c".to_vec()),
        // CSI 5 n: device status report (DSR) -> terminal OK.
        (b"5", b'n') => Some(b"\x1b[0n".to_vec()),
        // CSI 6 n: cursor position report (CPR) -> 1-based row;col.
        (b"6", b'n') => {
            let (row, col) = cursor;
            Some(format!("\x1b[{};{}R", row.saturating_add(1), col.saturating_add(1)).into_bytes())
        }
        // CSI ? 2026 $ p: DECRQM for synchronized output.
        (b"?2026$", b'p') => {
            let state = if in_sync_update { 1 } else { 2 };
            Some(format!("\x1b[?2026;{state}$y").into_bytes())
        }
        // CSI ? 2027 $ p: DECRQM for grapheme clustering (not supported by vt100).
        (b"?2027$", b'p') => Some(b"\x1b[?2027;0$y".to_vec()),
        // CSI ? u: kitty keyboard protocol query -> disabled.
        (b"?", b'u') => Some(b"\x1b[?0u".to_vec()),
        _ => None,
    }
}

/// Answers a kitty graphics protocol capability query (`a=q`). Direct
/// transmission is reported as supported; file/shared-memory transmission is
/// rejected with an error so clients fall back to in-band streaming, which is
/// the only mechanism that works across the client/server split. Returns
/// `None` for everything else (transmissions, placements, ...) which should be
/// passed through to the real terminal.
fn answer_graphics_query(apc: &[u8]) -> Option<Vec<u8>> {
    let body = apc.strip_prefix(b"\x1b_G")?;
    let body = body.strip_suffix(b"\x1b\\")?;
    let control = match body.iter().position(|&b| b == b';') {
        Some(p) => &body[..p],
        None => body,
    };

    let mut action = None;
    let mut id: Option<&[u8]> = None;
    let mut transmission = None;
    for pair in control.split(|&b| b == b',') {
        if let Some(eq) = pair.iter().position(|&b| b == b'=') {
            match &pair[..eq] {
                b"a" => action = Some(&pair[eq + 1..]),
                b"i" => id = Some(&pair[eq + 1..]),
                b"t" => transmission = Some(&pair[eq + 1..]),
                _ => {}
            }
        }
    }

    if action? != b"q" {
        return None;
    }
    let is_direct = transmission.is_none_or(|t| t == b"d");
    let mut response = Vec::new();
    response.extend_from_slice(b"\x1b_G");
    if let Some(id) = id {
        response.extend_from_slice(b"i=");
        response.extend_from_slice(id);
    }
    if is_direct {
        response.extend_from_slice(b";OK\x1b\\");
    } else {
        response.extend_from_slice(b";EBADF:Failed to read image data\x1b\\");
    }
    Some(response)
}

/// Serializes every scrolled-out row of the pane's main screen with its
/// formatting escape sequences (one line per grid row, `\r\n` separated) so
/// the client can rebuild the same history in its own parser. The currently
/// visible rows are not included; they arrive via `state_formatted`.
///
/// Scrollback offset 0 shows the bottom of the screen; `total` is the number
/// of scrolled-out rows. The walk starts at the top (offset `total`) and works
/// down, taking only the history portion of each window.
fn serialize_history<CB: vt100::Callbacks>(parser: &mut vt100::Parser<CB>) -> Vec<u8> {
    let screen = parser.screen_mut();
    screen.set_scrollback(usize::MAX);
    let total = screen.scrollback();
    screen.set_scrollback(0);
    let (rows, cols) = screen.size();

    let mut out = Vec::new();
    let mut start = 0usize;
    while start < total {
        screen.set_scrollback(total - start);
        let take = (total - start).min(usize::from(rows));
        for line in screen.rows_formatted(0, cols).take(take) {
            out.extend_from_slice(&line);
            out.extend_from_slice(b"\r\n");
        }
        start += take;
    }
    screen.set_scrollback(0);
    out
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Appends enough blank lines that, once a client overwrites the visible area
/// with the `state` bytes, the whole history has scrolled into the client
/// parser's scrollback.
///
/// Feeding `L` single-row lines into a `rows`-row grid leaves exactly
/// `max(0, L - rows + 1)` rows of scrollback (the trailing `\r\n` of the last
/// line leaves a blank bottom row), so padding the `total` history lines with
/// `rows - 1` blanks makes the client's scrollback hold exactly `total` rows.
fn pad_scrollback(mut history: Vec<u8>, rows: u16) -> Vec<u8> {
    for _ in 0..usize::from(rows.saturating_sub(1)) {
        history.extend_from_slice(b"\r\n");
    }
    history
}

fn xterm_modifier(modifiers: KeyModifiers) -> Option<u8> {
    let mut m = 1u8;
    if modifiers.contains(KeyModifiers::SHIFT) {
        m += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        m += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        m += 4;
    }
    (m > 1).then_some(m)
}

fn encode_ctrl_char(c: char) -> Option<u8> {
    match c {
        '@' | ' ' | '2' => Some(0x00),
        'a'..='z' => Some((c as u8 - b'a') + 1),
        'A'..='Z' => Some((c as u8 - b'A') + 1),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' | '~' => Some(0x1e),
        '_' | '7' | '/' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

fn shifted_char(c: char) -> Option<char> {
    Some(match c {
        'a'..='z' => c.to_ascii_uppercase(),
        '0' => ')',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '`' => '~',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        _ => return None,
    })
}

fn encode_key_event(key_event: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    let modifiers = key_event.modifiers;
    let alt = modifiers.contains(KeyModifiers::ALT);
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let shift = modifiers.contains(KeyModifiers::SHIFT);

    let mut out = Vec::new();

    match key_event.code {
        KeyCode::Char(c) => {
            let c = if shift {
                shifted_char(c).unwrap_or(c)
            } else {
                c
            };
            if alt {
                out.push(0x1b);
            }
            if ctrl {
                if let Some(b) = encode_ctrl_char(c) {
                    out.push(b);
                } else {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Backspace => {
            if alt {
                out.push(0x1b);
            }
            out.push(if ctrl { 0x08 } else { 0x7f });
        }
        KeyCode::Enter => {
            if let Some(m) = xterm_modifier(modifiers) {
                // Modified Enter (Shift/Ctrl/Alt+Enter) has no legacy
                // encoding; report it as a generic CSI-u key so programs that
                // understand the extended keyboard protocol see the modifier.
                out.extend_from_slice(format!("\x1b[13;{m}u").as_bytes());
            } else {
                out.push(b'\r');
            }
        }
        KeyCode::Tab => {
            if shift {
                out.extend_from_slice(b"\x1b[Z");
            } else {
                if alt {
                    out.push(0x1b);
                }
                out.push(b'\t');
            }
        }
        KeyCode::BackTab => {
            out.extend_from_slice(b"\x1b[Z");
        }
        KeyCode::Esc => {
            if alt {
                out.push(0x1b);
            }
            out.push(0x1b);
        }
        KeyCode::Null => {
            if alt {
                out.push(0x1b);
            }
            out.push(0x00);
        }
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Right
        | KeyCode::Left
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::KeypadBegin => {
            let letter = match key_event.code {
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Right => 'C',
                KeyCode::Left => 'D',
                KeyCode::Home => 'H',
                KeyCode::End => 'F',
                KeyCode::KeypadBegin => 'E',
                _ => unreachable!(),
            };
            if let Some(m) = xterm_modifier(modifiers) {
                out.extend_from_slice(format!("\x1b[1;{m}{letter}").as_bytes());
            } else if application_cursor {
                out.extend_from_slice(format!("\x1bO{letter}").as_bytes());
            } else {
                out.extend_from_slice(format!("\x1b[{letter}").as_bytes());
            }
        }
        KeyCode::Insert | KeyCode::Delete | KeyCode::PageUp | KeyCode::PageDown => {
            let code = match key_event.code {
                KeyCode::Insert => 2,
                KeyCode::Delete => 3,
                KeyCode::PageUp => 5,
                KeyCode::PageDown => 6,
                _ => unreachable!(),
            };
            if let Some(m) = xterm_modifier(modifiers) {
                out.extend_from_slice(format!("\x1b[{code};{m}~").as_bytes());
            } else {
                out.extend_from_slice(format!("\x1b[{code}~").as_bytes());
            }
        }
        KeyCode::F(n) => match n {
            1..=4 => {
                let letter = match n {
                    1 => 'P',
                    2 => 'Q',
                    3 => 'R',
                    4 => 'S',
                    _ => unreachable!(),
                };
                if let Some(m) = xterm_modifier(modifiers) {
                    out.extend_from_slice(format!("\x1b[1;{m}{letter}").as_bytes());
                } else {
                    out.extend_from_slice(format!("\x1bO{letter}").as_bytes());
                }
            }
            5..=20 => {
                let code = match n {
                    5 => 15,
                    6 => 17,
                    7 => 18,
                    8 => 19,
                    9 => 20,
                    10 => 21,
                    11 => 23,
                    12 => 24,
                    13 => 25,
                    14 => 26,
                    15 => 28,
                    16 => 29,
                    17 => 31,
                    18 => 32,
                    19 => 33,
                    20 => 34,
                    _ => unreachable!(),
                };
                if let Some(m) = xterm_modifier(modifiers) {
                    out.extend_from_slice(format!("\x1b[{code};{m}~").as_bytes());
                } else {
                    out.extend_from_slice(format!("\x1b[{code}~").as_bytes());
                }
            }
            _ => return None,
        },
        KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::Media(_)
        | KeyCode::Modifier(_) => return None,
    }

    Some(out)
}

fn mouse_button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn encode_mouse_event(
    mouse_event: MouseEvent,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let allowed = match mode {
        MouseProtocolMode::None => false,
        MouseProtocolMode::Press => matches!(
            mouse_event.kind,
            MouseEventKind::Down(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        MouseProtocolMode::PressRelease => matches!(
            mouse_event.kind,
            MouseEventKind::Down(_)
                | MouseEventKind::Up(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        MouseProtocolMode::ButtonMotion => matches!(
            mouse_event.kind,
            MouseEventKind::Down(_)
                | MouseEventKind::Up(_)
                | MouseEventKind::Drag(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        MouseProtocolMode::AnyMotion => true,
    };

    if !allowed {
        return None;
    }

    let mut cb = match mouse_event.kind {
        MouseEventKind::Down(button) => mouse_button_code(button),
        MouseEventKind::Up(button) => match encoding {
            MouseProtocolEncoding::Sgr => mouse_button_code(button),
            MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8 => 3,
        },
        MouseEventKind::Drag(button) => 32 + mouse_button_code(button),
        MouseEventKind::Moved => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };

    if mouse_event.modifiers.contains(KeyModifiers::SHIFT) {
        cb |= 4;
    }
    if mouse_event.modifiers.contains(KeyModifiers::ALT) {
        cb |= 8;
    }
    if mouse_event.modifiers.contains(KeyModifiers::CONTROL) {
        cb |= 16;
    }

    let x = mouse_event.column.saturating_add(1);
    let y = mouse_event.row.saturating_add(1);

    match encoding {
        MouseProtocolEncoding::Sgr => {
            let suffix = if matches!(mouse_event.kind, MouseEventKind::Up(_)) {
                'm'
            } else {
                'M'
            };
            Some(format!("\x1b[<{cb};{x};{y}{suffix}").into_bytes())
        }
        MouseProtocolEncoding::Default => {
            if x > 223 || y > 223 {
                return None;
            }
            Some(vec![0x1b, b'[', b'M', cb + 32, x as u8 + 32, y as u8 + 32])
        }
        MouseProtocolEncoding::Utf8 => {
            if x > 2015 || y > 2015 {
                return None;
            }
            let mut out = Vec::with_capacity(9);
            out.extend_from_slice(b"\x1b[M");
            let mut buf = [0u8; 4];
            for val in [u32::from(cb) + 32, u32::from(x) + 32, u32::from(y) + 32] {
                let ch = char::from_u32(val)?;
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            Some(out)
        }
    }
}

impl Widget for &ProjectionTerm {
    fn render(self, area: ratatui::prelude::Rect, buf: &mut ratatui::prelude::Buffer) {
        let parser = self.parser.lock().unwrap();
        let widget = PseudoTerminal::new(parser.screen());
        widget.render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn encodes_basic_chars_and_modifiers() {
        assert_eq!(
            encode_key_event(key(KeyCode::Char('a'), KeyModifiers::NONE), false),
            Some(b"a".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Char('C'), KeyModifiers::CONTROL), false),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Char('f'), KeyModifiers::ALT), false),
            Some(b"\x1bf".to_vec())
        );
        assert_eq!(
            encode_key_event(
                key(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT
                ),
                false
            ),
            Some(vec![0x1b, 0x03])
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Char(' '), KeyModifiers::CONTROL), false),
            Some(vec![0x00])
        );
    }

    #[test]
    fn encodes_enter_backspace_tab_esc() {
        assert_eq!(
            encode_key_event(key(KeyCode::Enter, KeyModifiers::NONE), false),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Enter, KeyModifiers::ALT), false),
            Some(b"\x1b[13;3u".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Enter, KeyModifiers::SHIFT), false),
            Some(b"\x1b[13;2u".to_vec())
        );
        assert_eq!(
            encode_key_event(
                key(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                false
            ),
            Some(b"\x1b[13;6u".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Backspace, KeyModifiers::NONE), false),
            Some(vec![0x7f])
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Backspace, KeyModifiers::CONTROL), false),
            Some(vec![0x08])
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Backspace, KeyModifiers::ALT), false),
            Some(vec![0x1b, 0x7f])
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Tab, KeyModifiers::NONE), false),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::BackTab, KeyModifiers::SHIFT), false),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Esc, KeyModifiers::NONE), false),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn encodes_arrows_and_navigation_keys() {
        assert_eq!(
            encode_key_event(key(KeyCode::Up, KeyModifiers::NONE), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Up, KeyModifiers::NONE), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Right, KeyModifiers::CONTROL), false),
            Some(b"\x1b[1;5C".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Home, KeyModifiers::NONE), false),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::End, KeyModifiers::NONE), true),
            Some(b"\x1bOF".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::Delete, KeyModifiers::NONE), false),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::PageUp, KeyModifiers::SHIFT), false),
            Some(b"\x1b[5;2~".to_vec())
        );
    }

    #[test]
    fn encodes_function_keys_and_ignores_release() {
        assert_eq!(
            encode_key_event(key(KeyCode::F(1), KeyModifiers::NONE), false),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::F(5), KeyModifiers::NONE), false),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            encode_key_event(key(KeyCode::F(12), KeyModifiers::CONTROL), false),
            Some(b"\x1b[24;5~".to_vec())
        );

        let mut release = key(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(encode_key_event(release, false), None);

        let mut repeat = key(KeyCode::Char('a'), KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        assert_eq!(encode_key_event(repeat, false), Some(b"a".to_vec()));
    }

    #[test]
    fn encodes_mouse_events() {
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 9,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            encode_mouse_event(down, MouseProtocolMode::None, MouseProtocolEncoding::Sgr),
            None
        );
        assert_eq!(
            encode_mouse_event(down, MouseProtocolMode::Press, MouseProtocolEncoding::Sgr),
            Some(b"\x1b[<0;10;5M".to_vec())
        );

        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 9,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            encode_mouse_event(
                up,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr
            ),
            Some(b"\x1b[<0;10;5m".to_vec())
        );
        assert_eq!(
            encode_mouse_event(
                up,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default
            ),
            Some(vec![0x1b, b'[', b'M', 35, 42, 37])
        );
    }

    #[test]
    fn scrollback_serialization_rebuilds_history_in_a_fresh_parser() {
        let mut server = vt100::Parser::new(4, 20, SCROLLBACK_LINES);
        for i in 0..20 {
            server.process(format!("line {i:02}\r\n").as_bytes());
        }
        let (rows, _) = server.screen().size();
        let history = serialize_history(&mut server);
        let state = server.screen().state_formatted();
        let payload = pad_scrollback(history, rows);

        // Simulate the client: rebuild a fresh parser from the payload, then
        // apply the visible state on top.
        let mut client = vt100::Parser::new(4, 20, SCROLLBACK_LINES);
        client.process(&payload);
        client.process(&state);

        // The rebuilt parser must hold exactly as many history rows as the
        // server's parser does.
        server.screen_mut().set_scrollback(usize::MAX);
        let total = server.screen().scrollback();
        server.screen_mut().set_scrollback(0);
        client.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(client.screen().scrollback(), total);
        client.screen_mut().set_scrollback(0);

        // The visible bottom matches the server's current screen.
        assert_eq!(client.screen().contents(), server.screen().contents());

        // Scrolling both to the top shows the same oldest content.
        client.screen_mut().set_scrollback(usize::MAX);
        server.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(client.screen().contents(), server.screen().contents());
        assert!(client.screen().contents().starts_with("line 00\nline 01"));
    }

    #[test]
    fn scrollback_snapshot_skips_resending_unchanged_history() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new("/").to_path_buf());
        let term = ProjectionTerm::with_command(20, 4, "true", &cwd).unwrap();
        let (_, _, _, first, _) = term.state_snapshot();
        let (_, _, _, second, _) = term.state_snapshot();
        // "true" writes nothing, but the first snapshot still pads the
        // (empty) history; the second must report no change.
        assert!(!first.is_empty());
        assert!(second.is_empty());
    }

    fn feed_split(
        chunks: &[&[u8]],
        cell_px: (u16, u16),
        size: (u16, u16),
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut filter = PassthroughFilter::new();
        let mut screen = Vec::new();
        let mut passthrough = Vec::new();
        let mut responses = Vec::new();
        for chunk in chunks {
            filter.feed(
                chunk,
                &mut screen,
                &mut passthrough,
                &mut responses,
                cell_px,
                size,
                (0, 0),
            );
        }
        (screen, passthrough, responses)
    }

    #[test]
    fn passthrough_filter_translates_hpa_hvp_rep_cht_and_colon_sgr() {
        // HPA (`) -> CHA (G), HVP (f) -> CUP (H), REP (b) repeats last char.
        let (screen, _, _) = feed_split(
            &["\x1b[6`─\x1b[3b\x1b[2;4f\x1b[38:2::10:20:30;4:3;9mX\x1b[m".as_bytes()],
            (10, 20),
            (24, 80),
        );
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(&screen);
        let s = parser.screen();
        // Row 0, cols 5..9 (1-based col 6) should have 4 '─' characters.
        assert_eq!(s.cell(0, 5).unwrap().contents(), "─");
        assert_eq!(s.cell(0, 8).unwrap().contents(), "─");
        // Row 1, col 3 (1-based 2;4) should have 'X' with RGB(10,20,30) and underline.
        let x_cell = s.cell(1, 3).unwrap();
        assert_eq!(x_cell.contents(), "X");
        assert_eq!(x_cell.fgcolor(), vt100::Color::Rgb(10, 20, 30));
        assert!(x_cell.underline());
    }

    #[test]
    fn passthrough_filter_answers_modern_tui_queries_and_sync_output() {
        let (screen, _, responses) = feed_split(
            &[b"\x1b[?2026$p\x1b[?2027$p\x1b]11;?\x07\x1b[?u\x1b[6n\x1b[?2026hhi\x1b[?2026l"],
            (10, 20),
            (24, 80),
        );
        assert_eq!(
            responses,
            b"\x1b[?2026;2$y\x1b[?2027;0$y\x1b]11;rgb:0000/0000/0000\x07\x1b[?0u\x1b[1;1R"
        );
        assert_eq!(screen, b"hi");
    }

    #[test]
    fn passthrough_filter_keeps_plain_text() {
        let (screen, passthrough, responses) = feed_split(&[b"hello\r\nworld"], (10, 20), (80, 24));
        assert_eq!(screen, b"hello\r\nworld");
        assert!(passthrough.is_empty());
        assert!(responses.is_empty());
    }

    #[test]
    fn passthrough_filter_forwards_graphics_apc() {
        let img = b"\x1b_Gf=100;SGVsbG8=\x1b\\";
        let (screen, passthrough, responses) = feed_split(&[img], (10, 20), (80, 24));
        assert_eq!(passthrough, img);
        assert!(screen.is_empty());
        assert!(responses.is_empty());
    }

    #[test]
    fn passthrough_filter_handles_apc_split_across_reads() {
        let img: &[u8] = b"\x1b_Gm=1;AAAA\x1b\\";
        let (screen, passthrough, _) =
            feed_split(&[&img[..7], &img[7..12], &img[12..]], (10, 20), (80, 24));
        assert_eq!(passthrough, img);
        assert!(screen.is_empty());
    }

    #[test]
    fn passthrough_filter_answers_cell_size_query() {
        let (screen, passthrough, responses) = feed_split(&[b"\x1b[16t"], (12, 24), (80, 24));
        assert_eq!(responses, b"\x1b[6;24;12t");
        assert!(screen.is_empty());
        assert!(passthrough.is_empty());
    }

    #[test]
    fn passthrough_filter_answers_device_attributes() {
        let (screen, passthrough, responses) = feed_split(&[b"\x1b[c"], (12, 24), (80, 24));
        assert_eq!(responses, b"\x1b[?1;2c");
        assert!(screen.is_empty());
        assert!(passthrough.is_empty());
    }

    #[test]
    fn passthrough_filter_answers_graphics_query_but_not_transmissions() {
        // Direct query (a=q,t=d,i=1) -> OK, not forwarded.
        let (screen, passthrough, responses) = feed_split(
            &[b"\x1b_Ga=q,i=1,t=d,s=1,v=1,f=24,S=3;MTIz\x1b\\"],
            (12, 24),
            (80, 24),
        );
        assert_eq!(responses, b"\x1b_Gi=1;OK\x1b\\");
        assert!(screen.is_empty());
        assert!(passthrough.is_empty());

        // File/shared-memory queries -> error (so clients use stream mode).
        let (_, _, responses) = feed_split(
            &[b"\x1b_Ga=q,i=2,t=s,s=1,v=1,f=24,S=3;MTIz\x1b\\"],
            (12, 24),
            (80, 24),
        );
        assert_eq!(
            responses,
            b"\x1b_Gi=2;EBADF:Failed to read image data\x1b\\"
        );

        // A real transmission (a=T) is not a query -> forwarded unchanged.
        let (_, passthrough, responses) = feed_split(
            &[b"\x1b_Ga=T,f=100,q=2,s=8,v=8,m=1;AAAA\x1b\\"],
            (12, 24),
            (80, 24),
        );
        assert_eq!(passthrough, b"\x1b_Ga=T,f=100,q=2,s=8,v=8,m=1;AAAA\x1b\\");
        assert!(responses.is_empty());
    }

    #[test]
    fn passthrough_filter_answers_pane_size_query() {
        // size is (rows, cols) = 24 rows x 80 cols; cell is 12x24 px.
        let (screen, _, responses) = feed_split(&[b"\x1b[14t"], (12, 24), (24, 80));
        // pane pixels = 12*80 x 24*24 = 960 x 576
        assert_eq!(responses, b"\x1b[4;576;960t");
        assert!(screen.is_empty());
    }

    #[test]
    fn passthrough_filter_leaves_other_csi_and_osc_alone() {
        let (screen, passthrough, responses) =
            feed_split(&[b"\x1b[31mred\x1b[0m\x1b]0;title\x07"], (12, 24), (80, 24));
        assert_eq!(screen, b"\x1b[31mred\x1b[0m\x1b]0;title\x07");
        assert!(passthrough.is_empty());
        assert!(responses.is_empty());
    }

    #[test]
    fn term_forwards_graphics_and_answers_size_queries() {
        let cwd = std::env::temp_dir();
        let script_path = cwd.join("harp_graphics_test.sh");
        let script = r#"#!/bin/bash
printf '\033_Gf=100;SGVsbG8=\033\\'
printf '\033[16t'
r=''
while IFS= read -rn1 c; do
  r="$r$c"
  [ "$c" = t ] && break
done
printf 'RESP:%s\n' "$r"
"#;
        std::fs::write(&script_path, script).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
        let program = script_path.display().to_string();

        let mut term = ProjectionTerm::with_command(80, 24, &program, &cwd).unwrap();
        term.set_cell_size(12, 24).unwrap();

        // Give the child time to emit the graphics code and query.
        std::thread::sleep(Duration::from_millis(500));

        // The graphics APC must have been captured for forwarding to the real
        // terminal instead of being fed to the screen parser.
        assert_eq!(term.take_passthrough(), b"\x1b_Gf=100;SGVsbG8=\x1b\\");

        // The cell-size query must have been answered back into the pty: the
        // child's read loop only finishes once it sees a response ending in
        // 't', and it then prints RESP:.
        let (_, _, state, _, _) = term.state_snapshot();
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(&state);
        let contents = parser.screen().contents();
        assert!(
            contents.contains("RESP:"),
            "child should have received the size answer, got: {contents:?}"
        );

        let _ = std::fs::remove_file(&script_path);
    }

    #[test]
    fn title_callbacks_track_osc_titles_and_clear_on_empty() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, TitleCallbacks::default());
        assert_eq!(parser.callbacks().title(), None);

        parser.process(b"\x1b]0;build: app\x07");
        assert_eq!(parser.callbacks().title(), Some("build: app"));

        parser.process(b"\x1b]2;vim main.rs\x1b\\");
        assert_eq!(parser.callbacks().title(), Some("vim main.rs"));

        // Ensure PassthroughFilter preserves ST-terminated OSC sequences cleanly for vt100.
        let (screen, _, _) = feed_split(&[b"\x1b]2;filtered-st\x1b\\"], (12, 24), (80, 24));
        assert_eq!(screen, b"\x1b]2;filtered-st\x1b\\");
        parser.process(&screen);
        assert_eq!(parser.callbacks().title(), Some("filtered-st"));

        // Clearing window title falls back to icon_name if set, or None if cleared via OSC 0.
        parser.process(b"\x1b]0;\x07");
        assert_eq!(parser.callbacks().title(), None);

        // OSC 1 alone sets icon_name as fallback when window_title is unset.
        parser.process(b"\x1b]1;short\x07");
        assert_eq!(parser.callbacks().title(), Some("short"));

        // Control characters are stripped and whitespace-only titles are ignored.
        parser.process(b"\x1b]2;  hello\nworld  \x07");
        assert_eq!(parser.callbacks().title(), Some("helloworld"));
        parser.process(b"\x1b]0;   \x07");
        assert_eq!(parser.callbacks().title(), None);
    }

    #[test]
    fn term_updates_title_from_osc_sequence() {
        let cwd = std::env::temp_dir();
        let script_path = cwd.join("harp_title_test.sh");
        let script = "#!/bin/bash\nprintf '\\033]2;custom-pane-title\\007'\nsleep 0.2\n";
        std::fs::write(&script_path, script).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
        let program = script_path.display().to_string();

        let term = ProjectionTerm::with_command(80, 24, &program, &cwd).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(term.title(), "custom-pane-title");

        let _ = std::fs::remove_file(&script_path);
    }

    #[test]
    fn builtin_projection_streams_output_and_sets_osc_title() {
        let term = ProjectionTerm::with_builtin(40, 10, "Builtin", |mut stream, _closed| {
            let _ = stream.write_all(b"\x1b]2;Debug Log\x07hello builtin\r\n");
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(200));
        })
        .unwrap();

        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(term.title(), "Debug Log");

        let (_, _, state, _, _) = term.state_snapshot();
        let mut parser = vt100::Parser::new(10, 40, 0);
        parser.process(&state);
        assert!(parser.screen().contents().contains("hello builtin"));
    }
}
