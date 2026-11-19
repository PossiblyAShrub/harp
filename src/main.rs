use std::io;

use crossterm::event::{self, KeyCode, KeyEventKind};
use ratatui::{DefaultTerminal, Frame, prelude::{Buffer, Rect}, widgets::{Paragraph, Widget}};

struct App {
    counter: u8,
    exit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            counter: 0,
            exit: false,
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.exit {
            terminal.draw(|frame| self.render(frame))?;
            self.handle_events()?;
        }

        Ok(())
    }

    fn render(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_events(&mut self) -> io::Result<()> {
        match event::read()? {
            event::Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                if key_event.code == KeyCode::Esc { self.exit = true }
                else { self.counter += 1 }
            }
            _ => {}
        }

        Ok(())
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(format!("Count: {}", self.counter)).render(area, buf);
    }
}

fn main() -> io::Result<()> {
    ratatui::run(|terminal| App::new().run(terminal))
}
