use std::{
    collections::VecDeque,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use ratatui::{
    layout::Rect,
    prelude::Buffer,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Widget},
};

use crate::server::ServerTraceEntry;

const MAX_DEBUG_LOG_ENTRIES: usize = 500;
static DEBUG_LOG: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
static DEBUG_LOG_REV: AtomicU64 = AtomicU64::new(0);

fn debug_log_store() -> &'static Mutex<VecDeque<String>> {
    DEBUG_LOG.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_DEBUG_LOG_ENTRIES)))
}

pub fn log_debug_event(event: impl Into<String>) {
    let seq = DEBUG_LOG_REV.fetch_add(1, Ordering::AcqRel) + 1;
    let mut log = debug_log_store().lock().unwrap();
    if log.len() >= MAX_DEBUG_LOG_ENTRIES {
        log.pop_front();
    }
    log.push_back(format!("#{seq:<4} {}", event.into()));
    crate::projections::wake_watchers();
}

pub fn debug_log_revision() -> u64 {
    DEBUG_LOG_REV.load(Ordering::Acquire)
}

pub fn recent_debug_events() -> Vec<String> {
    debug_log_store().lock().unwrap().iter().cloned().collect()
}

pub struct DebugLogProjection<'a> {
    entries: &'a [String],
}

impl<'a> DebugLogProjection<'a> {
    pub fn new(entries: &'a [String]) -> Self {
        Self { entries }
    }
}

impl Widget for DebugLogProjection<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let visible = usize::from(area.height.saturating_sub(2));
        let skip = self.entries.len().saturating_sub(visible);
        let lines: Vec<Line<'_>> = self
            .entries
            .iter()
            .skip(skip)
            .map(|entry| Line::from(Span::styled(entry.as_str(), Style::default().fg(Color::Gray))))
            .collect();

        let block = Block::bordered()
            .title(" Debug Log ")
            .border_style(Style::default().fg(Color::Cyan));
        Paragraph::new(lines).block(block).render(area, buf);
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_log_records_and_renders_events() {
        let rev_before = debug_log_revision();
        log_debug_event("title received (OSC 0/2): \"hello\"");
        assert!(debug_log_revision() > rev_before);

        let events = recent_debug_events();
        assert!(
            events
                .iter()
                .any(|e| e.contains("title received (OSC 0/2): \"hello\""))
        );

        let area = Rect::new(0, 0, 50, 6);
        let mut buf = Buffer::empty(area);
        DebugLogProjection::new(&events).render(area, &mut buf);
        let top_row: String = (0..50).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(top_row.contains(" Debug Log "));
    }
}
