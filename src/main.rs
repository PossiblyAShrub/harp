use std::io;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{DefaultTerminal, Frame, prelude::{Buffer, Rect}, widgets::Widget};

mod projections;

struct App {
    term: projections::ProjectionTerm,
    exit: bool,
}

impl App {
    fn new() -> Result<Self> {
        Ok(Self {
            term: projections::ProjectionTerm::new()?,
            exit: false,
        })
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.exit {
            self.handle_events()?;
            terminal.draw(|frame| self.render(frame))?;
        }

        Ok(())
    }

    fn render(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_events(&mut self) -> io::Result<()> {
        match event::read()? {
            Event::Key(key_event) if key_event.kind == KeyEventKind::Press && key_event.code == KeyCode::Esc => {
                self.exit = true;
            }
            Event::Key(key_event) => self.term.handle_key_event(key_event)?,
            _ => {}
        }

        Ok(())
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.term.render(area, buf);
    }
}

fn main() -> Result<()> {
    let mut app = App::new()?;
    ratatui::run(|terminal| app.run(terminal))?;

    Ok(())
}
