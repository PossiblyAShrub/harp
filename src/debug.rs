use ratatui::{
    layout::Rect,
    prelude::Buffer,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Widget},
};

use crate::server::ServerTraceEntry;

/// A lightweight in-client projection for inspecting protocol replies and
/// their client-observed request latency.
pub struct ServerDebugProjection<'a> {
    entries: &'a [ServerTraceEntry],
}

impl<'a> ServerDebugProjection<'a> {
    pub fn new(entries: &'a [ServerTraceEntry]) -> Self {
        Self { entries }
    }
}

impl Widget for ServerDebugProjection<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let visible = usize::from(area.height.saturating_sub(2));
        let skip = self.entries.len().saturating_sub(visible);
        let lines: Vec<Line<'_>> = self
            .entries
            .iter()
            .skip(skip)
            .map(|entry| {
                Line::from(vec![
                    Span::styled(
                        format!("{:>8.2} ms  ", entry.latency.as_secs_f64() * 1000.0),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(&entry.request, Style::default().fg(Color::Yellow)),
                    Span::raw(" → "),
                    Span::styled(&entry.response, Style::default().fg(Color::Green)),
                ])
            })
            .collect();

        let block = Block::bordered()
            .title(" Server messages and request latency (D to close) ")
            .border_style(Style::default().fg(Color::Cyan));
        Paragraph::new(lines).block(block).render(area, buf);
    }
}
