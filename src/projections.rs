use std::{
    io::{self, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::widgets::Widget;
use tui_term::{
    vt100::{self, MouseProtocolEncoding, MouseProtocolMode},
    widget::PseudoTerminal,
};

pub struct ProjectionTerm {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    exited: Arc<AtomicBool>,
}

impl ProjectionTerm {
    pub fn new() -> Result<Self> {
        let pty = native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let cmd = CommandBuilder::new("zsh");
        let mut child = pty.slave.spawn_command(cmd)?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 1000)));
        let exited = Arc::new(AtomicBool::new(false));

        let mut reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;
        let parser_clone = Arc::clone(&parser);
        let exited_clone = Arc::clone(&exited);

        thread::spawn(move || {
            let mut buffer = [0u8; 4096];

            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }

                parser_clone.lock().unwrap().process(&buffer[..n]);
            }

            let _ = child.wait();
            exited_clone.store(true, Ordering::Release);
        });

        Ok(Self {
            master: pty.master,
            writer,
            parser,
            exited,
        })
    }

    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
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
            self.writer.write_all(&bytes)?;
            self.writer.flush()?;
        }

        Ok(())
    }

    pub fn handle_mouse_event(&mut self, mouse_event: MouseEvent) -> io::Result<()> {
        let (mode, encoding) = {
            let parser = self.parser.lock().unwrap();
            let screen = parser.screen();
            (screen.mouse_protocol_mode(), screen.mouse_protocol_encoding())
        };

        if let Some(bytes) = encode_mouse_event(mouse_event, mode, encoding) {
            self.writer.write_all(&bytes)?;
            self.writer.flush()?;
        }

        Ok(())
    }

    pub fn handle_paste(&mut self, text: &str) -> io::Result<()> {
        let bracketed = self.parser.lock().unwrap().screen().bracketed_paste();
        if bracketed {
            self.writer.write_all(b"\x1b[200~")?;
            let sanitized = text.replace("\x1b[201~", "");
            self.writer.write_all(sanitized.as_bytes())?;
            self.writer.write_all(b"\x1b[201~")?;
        } else {
            self.writer.write_all(text.replace('\n', "\r").as_bytes())?;
        }
        self.writer.flush()
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let cols = cols.max(1);
        let rows = rows.max(1);

        self.parser.lock().unwrap().screen_mut().set_size(rows, cols);
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }
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
            if alt {
                out.push(0x1b);
            }
            out.push(b'\r');
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
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left | KeyCode::Home | KeyCode::End | KeyCode::KeypadBegin => {
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
                key(KeyCode::Char('c'), KeyModifiers::CONTROL | KeyModifiers::ALT),
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
            Some(b"\x1b\r".to_vec())
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
            encode_mouse_event(up, MouseProtocolMode::PressRelease, MouseProtocolEncoding::Sgr),
            Some(b"\x1b[<0;10;5m".to_vec())
        );
        assert_eq!(
            encode_mouse_event(up, MouseProtocolMode::PressRelease, MouseProtocolEncoding::Default),
            Some(vec![0x1b, b'[', b'M', 35, 42, 37])
        );
    }
}
