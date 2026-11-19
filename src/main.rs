use std::{io, path::PathBuf, time::Duration};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Rect},
    prelude::Buffer,
    widgets::Widget,
};

mod picker;
mod projections;
mod scopes;

use picker::{PickerAction, ScopePicker};
use projections::{Mode, ProjectionStatus};
use scopes::{NavDirection, Projection, Scope, ScopeSpec};

struct App {
    scope: Scope,
    mode: Mode,
    default_cwd: PathBuf,
    picker: Option<ScopePicker>,
}

impl App {
    fn new(area: Rect) -> Result<Self> {
        let mode = Mode::Managing;
        let default_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let workspace_area = Rect::new(
            area.x,
            area.y,
            area.width,
            area.height.saturating_sub(1),
        );
        let workspace = Scope::with_terminals(2, Direction::Horizontal, workspace_area)?;
        let status = ProjectionStatus::new(mode);

        let scope = Scope::from_children(
            Direction::Vertical,
            vec![
                (Constraint::Fill(1), Projection::Scope(Box::new(workspace))),
                (Constraint::Length(1), Projection::Status(status)),
            ],
            area,
        )?;

        Ok(Self {
            scope,
            mode,
            default_cwd,
            picker: None,
        })
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.scope.set_mode(mode);
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let size = terminal.size()?;
        self.scope.resize(Rect::new(0, 0, size.width, size.height))?;

        while self.scope.has_interactive() {
            terminal.draw(|frame| self.render(frame))?;
            if event::poll(Duration::from_millis(16))? {
                self.handle_events()?;
            }
            self.scope.prune_exited()?;
        }

        Ok(())
    }

    fn render(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_events(&mut self) -> io::Result<()> {
        loop {
            let event = event::read()?;
            if let Some(picker) = &mut self.picker {
                match &event {
                    Event::Key(key_event) => match picker.handle_key(*key_event) {
                        PickerAction::None => {}
                        PickerAction::Cancel => {
                            self.picker = None;
                        }
                        PickerAction::Confirm { direction, spec } => {
                            self.picker = None;
                            self.scope
                                .split_focused_with_spec(direction, &spec)
                                .map_err(io::Error::other)?;
                        }
                    },
                    Event::Resize(_, _) => self.scope.handle_event(&event)?,
                    _ => {}
                }
            } else {
                match self.mode {
                    Mode::Interacting => match &event {
                        Event::Key(key_event)
                            if matches!(
                                key_event.kind,
                                KeyEventKind::Press | KeyEventKind::Repeat
                            ) && key_event.code == KeyCode::Char(' ')
                                && key_event.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            self.set_mode(Mode::Managing);
                        }
                        _ => self.scope.handle_event(&event)?,
                    },
                    Mode::Managing => match &event {
                        Event::Key(key_event)
                            if matches!(
                                key_event.kind,
                                KeyEventKind::Press | KeyEventKind::Repeat
                            ) =>
                        {
                            if key_event.code == KeyCode::Enter
                                && !key_event
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            {
                                self.set_mode(Mode::Interacting);
                            } else if let Some(dir) = arrow_direction(key_event.code) {
                                if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                                    self.picker = Some(ScopePicker::new(
                                        dir,
                                        self.default_cwd.clone(),
                                    ));
                                } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                                    let spec = ScopeSpec::default_zsh(self.default_cwd.clone());
                                    self.scope
                                        .split_focused_with_spec(dir, &spec)
                                        .map_err(io::Error::other)?;
                                } else {
                                    self.scope
                                        .handle_managing_key(*key_event)
                                        .map_err(io::Error::other)?;
                                }
                            } else {
                                self.scope
                                    .handle_managing_key(*key_event)
                                    .map_err(io::Error::other)?;
                            }
                        }
                        Event::Resize(_, _) | Event::Mouse(_) => {
                            self.scope.handle_event(&event)?;
                        }
                        _ => {}
                    },
                }
            }

            self.scope.prune_exited()?;

            if !self.scope.has_interactive() || !event::poll(Duration::ZERO)? {
                break;
            }
        }

        Ok(())
    }
}

fn arrow_direction(code: KeyCode) -> Option<NavDirection> {
    match code {
        KeyCode::Up => Some(NavDirection::Up),
        KeyCode::Down => Some(NavDirection::Down),
        KeyCode::Left => Some(NavDirection::Left),
        KeyCode::Right => Some(NavDirection::Right),
        _ => None,
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.scope.render(area, buf);
        if let Some(picker) = &self.picker {
            picker.render(area, buf);
        }
    }
}

fn main() -> Result<()> {
    ratatui::run(|terminal| {
        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let mut app = App::new(area).map_err(io::Error::other)?;
        app.run(terminal)
    })?;

    Ok(())
}
