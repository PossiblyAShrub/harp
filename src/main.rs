use std::{io, time::Duration};

use anyhow::Result;
use crossterm::event;
use ratatui::{DefaultTerminal, Frame, prelude::{Buffer, Rect}, widgets::Widget};

mod projections;

struct App {
    term: projections::ProjectionTerm,
}

impl App {
    fn new() -> Result<Self> {
        Ok(Self {
            term: projections::ProjectionTerm::new()?,
        })
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let size = terminal.size()?;
        self.term.resize(size.width, size.height)?;

        while !self.term.is_exited() {
            terminal.draw(|frame| self.render(frame))?;
            if event::poll(Duration::from_millis(16))? {
                self.handle_events()?;
            }
        }

        Ok(())
    }

    fn render(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_events(&mut self) -> io::Result<()> {
        loop {
            let event = event::read()?;
            self.term.handle_event(&event)?;

            if self.term.is_exited() || !event::poll(Duration::ZERO)? {
                break;
            }
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
