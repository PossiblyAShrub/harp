use std::{
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    path::Path,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
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
                    " Ctrl+Space: manage | Ctrl+Shift+C: copy | Ctrl+Shift+V: paste",
                ),
                Mode::Managing => (
                    " MANAGING ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                    " Enter: interact | Arrows: focus | Ctrl+Arrows: split | Shift+Arrows: custom | Alt+Arrows: tabs | t/Shift+t: new tab | s: scopes | r: rename | q: detach",
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

pub struct ProjectionTerm {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    exited: Arc<AtomicBool>,
    revision: Arc<AtomicU64>,
    notify: Arc<(Mutex<u64>, Condvar)>,
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
        // Dropping the pty writer sends EOF to the child, which lets the
        // reader thread's blocking read return and the child be reaped.
        *self.writer.lock().unwrap() = None;
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
        let mut child = pty.slave.spawn_command(cmd)?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));
        let exited = Arc::new(AtomicBool::new(false));
        let revision = Arc::new(AtomicU64::new(1));
        let notify = Arc::new((Mutex::new(0u64), Condvar::new()));

        let mut reader = pty.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(Some(pty.master.take_writer()?)));
        let cell_px = Arc::new(Mutex::new((0u16, 0u16)));
        let passthrough = Arc::new(Mutex::new(Vec::new()));
        let parser_clone = Arc::clone(&parser);
        let exited_clone = Arc::clone(&exited);
        let revision_clone = Arc::clone(&revision);
        let notify_clone = Arc::clone(&notify);
        let writer_clone = Arc::clone(&writer);
        let cell_px_clone = Arc::clone(&cell_px);
        let passthrough_clone = Arc::clone(&passthrough);

        thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            let mut filter = PassthroughFilter::new();

            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }

                let mut screen = Vec::new();
                let mut passthrough = Vec::new();
                let mut responses = Vec::new();
                let cell_px = *cell_px_clone.lock().unwrap();
                let size = {
                    let parser = parser_clone.lock().unwrap();
                    parser.screen().size()
                };
                filter.feed(
                    &buffer[..n],
                    &mut screen,
                    &mut passthrough,
                    &mut responses,
                    cell_px,
                    size,
                );

                if !screen.is_empty() {
                    parser_clone.lock().unwrap().process(&screen);
                }
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
                revision_clone.fetch_add(1, Ordering::Release);
                let (lock, cvar) = &*notify_clone;
                let mut seq = lock.lock().unwrap();
                *seq = seq.wrapping_add(1);
                cvar.notify_all();
            }

            let _ = child.wait();
            exited_clone.store(true, Ordering::Release);
            revision_clone.fetch_add(1, Ordering::Release);
            let (lock, cvar) = &*notify_clone;
            let mut seq = lock.lock().unwrap();
            *seq = seq.wrapping_add(1);
            cvar.notify_all();
        });

        Ok(Self {
            master: pty.master,
            writer,
            parser,
            exited,
            revision,
            notify,
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
        Path::new(&self.program)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.program.clone())
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub fn state_snapshot(&self) -> (u16, u16, Vec<u8>, Vec<u8>) {
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

        (cols, rows, state, scrollback)
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

    pub fn handle_key_event(&mut self, key_event: KeyEvent) -> io::Result<()> {
        let application_cursor = self.parser.lock().unwrap().screen().application_cursor();
        if let Some(bytes) = encode_key_event(key_event, application_cursor) {
            let seq = *self.notify.0.lock().unwrap();
            if let Some(writer) = self.writer.lock().unwrap().as_mut() {
                writer.write_all(&bytes)?;
                writer.flush()?;
            }
            let guard = self.notify.0.lock().unwrap();
            let _ = self
                .notify
                .1
                .wait_timeout_while(guard, Duration::from_millis(12), |s| *s == seq);
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
        let (cols, rows) = self.size;
        let (cell_w, cell_h) = *self.cell_px.lock().unwrap();
        self.master
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
        let (cell_w, cell_h) = *self.cell_px.lock().unwrap();
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: cell_w.saturating_mul(cols),
                pixel_height: cell_h.saturating_mul(rows),
            })
            .map_err(io::Error::other)?;
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
}

enum FilterState {
    Normal,
    Esc,
    /// Inside a CSI sequence; collects parameter bytes until the final byte.
    Csi {
        params: Vec<u8>,
        private: bool,
    },
    /// Inside an APC sequence (graphics transmission), collected for
    /// forwarding.
    Apc {
        buf: Vec<u8>,
        saw_esc: bool,
    },
    /// Inside an OSC sequence; fed straight through to the screen parser.
    Osc {
        saw_esc: bool,
    },
}

impl PassthroughFilter {
    fn new() -> Self {
        Self {
            state: FilterState::Normal,
        }
    }

    /// Processes one chunk of pty output. `screen` gets the bytes that should
    /// go into the vt100 parser, `passthrough` the bytes to forward to the
    /// real terminal, and `responses` the bytes to write back into the pty.
    fn feed(
        &mut self,
        bytes: &[u8],
        screen: &mut Vec<u8>,
        passthrough: &mut Vec<u8>,
        responses: &mut Vec<u8>,
        cell_px: (u16, u16),
        size: (u16, u16),
    ) {
        for &b in bytes {
            self.feed_byte(b, screen, passthrough, responses, cell_px, size);
        }
    }

    fn feed_byte(
        &mut self,
        b: u8,
        screen: &mut Vec<u8>,
        passthrough: &mut Vec<u8>,
        responses: &mut Vec<u8>,
        cell_px: (u16, u16),
        size: (u16, u16),
    ) {
        match std::mem::replace(&mut self.state, FilterState::Normal) {
            FilterState::Normal => {
                if b == 0x1b {
                    self.state = FilterState::Esc;
                } else {
                    screen.push(b);
                    self.state = FilterState::Normal;
                }
            }
            FilterState::Esc => match b {
                b'[' => {
                    self.state = FilterState::Csi {
                        params: Vec::new(),
                        private: false,
                    }
                }
                b'_' => {
                    self.state = FilterState::Apc {
                        buf: vec![0x1b, b'_'],
                        saw_esc: false,
                    }
                }
                b']' => {
                    screen.extend_from_slice(b"\x1b]");
                    self.state = FilterState::Osc { saw_esc: false };
                }
                _ => {
                    screen.push(0x1b);
                    screen.push(b);
                    self.state = FilterState::Normal;
                }
            },
            FilterState::Csi {
                mut params,
                mut private,
            } => {
                if (0x40..=0x7e).contains(&b) {
                    // Final byte: answer known size queries, forward the rest
                    // to the screen parser untouched.
                    if !private && self.answer_size_query(&params, b, cell_px, size, responses) {
                        self.state = FilterState::Normal;
                    } else {
                        screen.extend_from_slice(b"\x1b[");
                        screen.extend_from_slice(&params);
                        screen.push(b);
                        self.state = FilterState::Normal;
                    }
                } else {
                    if b == b'?' && params.is_empty() && !private {
                        private = true;
                    }
                    params.push(b);
                    self.state = FilterState::Csi { params, private };
                }
            }
            FilterState::Apc { mut buf, saw_esc } => {
                buf.push(b);
                match (saw_esc, b) {
                    (true, b'\\') => {
                        passthrough.extend_from_slice(&buf);
                        self.state = FilterState::Normal;
                    }
                    (false, 0x07) => {
                        passthrough.extend_from_slice(&buf);
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
            FilterState::Osc { saw_esc } => match (saw_esc, b) {
                (true, b'\\') => {
                    screen.extend_from_slice(b"\x1b\\");
                    self.state = FilterState::Normal;
                }
                (true, _) => {
                    screen.push(0x1b);
                    screen.push(b);
                    self.state = FilterState::Osc { saw_esc: false };
                }
                (false, 0x07) => {
                    screen.push(b);
                    self.state = FilterState::Normal;
                }
                (false, 0x1b) => {
                    screen.push(b);
                    self.state = FilterState::Osc { saw_esc: true };
                }
                (false, _) => {
                    screen.push(b);
                    self.state = FilterState::Osc { saw_esc: false };
                }
            },
        }
    }

    /// Answers `CSI 16 t` (cell size) and `CSI 14 t` (pane size) queries.
    /// Returns true when the sequence was consumed.
    fn answer_size_query(
        &self,
        params: &[u8],
        final_byte: u8,
        cell_px: (u16, u16),
        size: (u16, u16),
        responses: &mut Vec<u8>,
    ) -> bool {
        if final_byte != b't' {
            return false;
        }
        let (cell_w, cell_h) = cell_px;
        if cell_w == 0 || cell_h == 0 {
            return false;
        }
        match params {
            b"16" => {
                responses.extend_from_slice(format!("\x1b[6;{cell_h};{cell_w}t").as_bytes());
                true
            }
            b"14" => {
                let (rows, cols) = size;
                let w = u32::from(cell_w) * u32::from(cols);
                let h = u32::from(cell_h) * u32::from(rows);
                responses.extend_from_slice(format!("\x1b[4;{h};{w}t").as_bytes());
                true
            }
            _ => false,
        }
    }
}

/// Serializes every scrolled-out row of the pane's main screen as plain text
/// (one line per grid row, `\r\n` separated) so the client can rebuild the
/// same history in its own parser. The currently visible rows are not
/// included; they arrive via `state_formatted`.
///
/// Scrollback offset 0 shows the bottom of the screen; `total` is the number
/// of scrolled-out rows. The walk starts at the top (offset `total`) and works
/// down, taking only the history portion of each window.
fn serialize_history(parser: &mut vt100::Parser) -> Vec<u8> {
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
        for line in screen.rows(0, cols).take(take) {
            out.extend_from_slice(line.as_bytes());
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
        let (_, _, _, first) = term.state_snapshot();
        let (_, _, _, second) = term.state_snapshot();
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
            );
        }
        (screen, passthrough, responses)
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
        let (_, _, state, _) = term.state_snapshot();
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(&state);
        let contents = parser.screen().contents();
        assert!(
            contents.contains("RESP:"),
            "child should have received the size answer, got: {contents:?}"
        );

        let _ = std::fs::remove_file(&script_path);
    }
}
