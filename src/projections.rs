use std::{
    io::{self, Read, Write}, sync::{Arc, Mutex}, thread,
};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ratatui::widgets::Widget;
use tui_term::{vt100, widget::PseudoTerminal};

pub struct ProjectionTerm {
    writer: Box<dyn Write>,
    parser: Arc<Mutex<vt100::Parser>>,
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

        let parser = Arc::new(Mutex::new(
            vt100::Parser::new(24, 80, 1000)
        ));

        let mut reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;
        let parser_clone = Arc::clone(&parser);

        thread::spawn(move || {
            let mut buffer = [0u8; 4096];

            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }

                parser_clone.lock().unwrap().process(&buffer[..n]);
            }

            child.kill().unwrap();
        });

        Ok(Self { parser, writer })
    }

    pub fn handle_key_event(&mut self, key_event: KeyEvent) -> io::Result<()> {
        if key_event.kind == KeyEventKind::Press {
            match key_event.code {
                KeyCode::Backspace => write!(&mut self.writer, "\x08")?,
                KeyCode::Enter => write!(&mut self.writer, "\x0A")?,
                KeyCode::Char(c) => write!(&mut self.writer, "{}", c)?,
                _ => {}
            }
        }

        Ok(())
    }
}

impl Widget for &ProjectionTerm {
    fn render(self, area: ratatui::prelude::Rect, buf: &mut ratatui::prelude::Buffer) {
        let parser = self.parser.lock().unwrap();
        let widget = PseudoTerminal::new(parser.screen());
        widget.render(area, buf);
    }
}
