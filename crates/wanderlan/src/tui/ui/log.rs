//! View 5: the event log, newest at the bottom.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use wanderlan_core::api::{Event, Level};

use super::titled;
use crate::tui::log::{LogState, level_name};
use crate::tui::{format, theme};

/// Width of the peer column.
const PEER_WIDTH: usize = 12;
/// Columns before the message: time, level, peer.
const PREFIX_WIDTH: usize = 9 + 6 + PEER_WIDTH + 1;

pub fn draw(frame: &mut Frame, log: &LogState, area: Rect) {
    let search = u16::from(log.text.is_shown());
    let [filters, text, list] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(search),
        Constraint::Min(3),
    ])
    .areas(area);
    let events = log.filtered();
    if search > 0 {
        log.text.draw(frame, text, events.len(), log.events.len());
    }
    let level = match log.min_level {
        None | Some(Level::Info) => "all",
        Some(Level::Warn) => "warn and error",
        Some(Level::Error) => "error",
    };
    let position = if log.scroll == 0 {
        Span::styled("following", theme::ok())
    } else {
        Span::styled(format!("{} newer below", log.scroll), theme::warn())
    };
    let line = Line::from(vec![
        Span::styled(" Level ", theme::dim()),
        Span::raw(level),
        Span::styled("  Peer ", theme::dim()),
        Span::raw(log.peer.clone().unwrap_or_else(|| "all".into())),
        Span::raw("  "),
        position,
        Span::styled(
            format!("  {} of {} events", events.len(), log.events.len()),
            theme::dim(),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), filters);

    let now = log.events.last().map_or(0, |e| e.time_ms);
    let zone = log.offset_at(now).1;
    let block = titled(if zone == "UTC" {
        " Events (times in UTC) ".to_string()
    } else {
        format!(" Events (local time, {zone}; t: UTC) ")
    });
    let inner = block.inner(list);
    let height = usize::from(inner.height);
    let width = usize::from(inner.width);
    let end = events.len().saturating_sub(log.scroll);
    let start = end.saturating_sub(height);
    let mut lines = Vec::new();
    let mut last_date = None;
    for event in events.get(start..end).unwrap_or_default() {
        let (date, time) = format::clock_at(event.time_ms, log.offset_at(event.time_ms).0);
        if last_date.as_ref() != Some(&date) {
            lines.push(Line::styled(format!("-- {date} --"), theme::dim()));
            last_date = Some(date);
        }
        lines.extend(event_lines(event, &time, width));
    }
    if events.is_empty() {
        let text = if log.synced {
            " No events match the filter."
        } else {
            " Waiting for the event history ..."
        };
        lines.push(Line::styled(text, theme::dim()));
    }
    let skip = lines.len().saturating_sub(height);
    let lines: Vec<Line> = lines.into_iter().skip(skip).collect();
    frame.render_widget(Paragraph::new(lines).block(block), list);
    // By events: how many lie above the window and how many it shows.
    let shown = end - start;
    super::list::scrollbar(frame, list, 1, 1, events.len(), start, shown);
}

fn event_lines(event: &Event, time: &str, width: usize) -> Vec<Line<'static>> {
    let style = match event.level {
        Level::Info => Style::new(),
        Level::Warn => theme::warn(),
        Level::Error => theme::bad(),
    };
    let peer: String = event
        .peer
        .as_deref()
        .unwrap_or("-")
        .chars()
        .take(PEER_WIDTH)
        .collect();
    let text_width = width.saturating_sub(PREFIX_WIDTH).max(10);
    let mut out = Vec::new();
    for (i, part) in format::wrap(&event.message, text_width)
        .into_iter()
        .enumerate()
    {
        if i == 0 {
            out.push(Line::from(vec![
                Span::styled(format!("{time} "), theme::dim()),
                Span::styled(format!("{:<5} ", level_name(event.level)), style),
                Span::raw(format!("{peer:<PEER_WIDTH$} ")),
                Span::styled(part, style),
            ]));
        } else {
            out.push(Line::styled(format!("{:PREFIX_WIDTH$}{part}", ""), style));
        }
    }
    out
}
