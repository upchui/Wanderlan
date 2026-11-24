//! Popups: forms, yes/no questions and the help screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};

use super::{diagnostic_line, titled};
use crate::tui::app::{App, Ask, MessageKind};
use crate::tui::form::{Field, Form, Input};
use crate::tui::help::{self, HelpLine};
use crate::tui::{format, theme};

/// Width of the labels in forms.
const LABEL_WIDTH: u16 = 17;
/// Notes shown at most below a form.
const MAX_NOTES: usize = 3;

pub fn form(frame: &mut Frame, form: &Form, area: Rect) {
    let popup_width = area.width.min(76);
    let inner_width = popup_width.saturating_sub(2);
    let value_width = inner_width.saturating_sub(LABEL_WIDTH + 1);
    let pad = " ".repeat(usize::from(LABEL_WIDTH + 1));
    let mut lines = Vec::new();
    // Line of the cursor, and the lines of the focused field.
    let mut cursor = None;
    let mut focus_lines = (0, 0);
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let first = lines.len();
        let (value, cursor_col) = field_value(field, focused, usize::from(value_width));
        if let Some(col) = cursor_col {
            cursor = Some((first, col));
        }
        let label_style = if focused { theme::bold() } else { theme::dim() };
        let mut spans = vec![Span::styled(
            format!(" {:<width$}", field.label, width = usize::from(LABEL_WIDTH)),
            label_style,
        )];
        spans.extend(value);
        lines.push(Line::from(spans));
        // Hints, errors and warnings wrap below the value.
        let (below, style) = match (field.visible_error(form.submitted), &field.warning) {
            _ if !field.enabled => ("not used here".to_string(), theme::dim()),
            (Some(e), _) => (e.to_string(), theme::bad()),
            (None, Some(w)) => (w.clone(), theme::warn()),
            (None, None) => (field.hint.clone(), theme::dim()),
        };
        for part in format::wrap(&below, usize::from(value_width.saturating_sub(1)).max(10)) {
            lines.push(Line::styled(format!("{pad}{part}"), style));
        }
        if focused {
            focus_lines = (first, lines.len());
        }
    }
    lines.push(Line::raw(""));
    for note in form.notes.iter().take(MAX_NOTES) {
        let mut line = diagnostic_line(note);
        line.spans.insert(0, Span::raw(" "));
        lines.push(line);
    }
    if form.submitted && !form.is_valid() {
        lines.push(Line::styled(
            " Fix the marked fields to save.",
            theme::bad(),
        ));
    }
    let total = lines.len();
    let popup = area.centered(
        Constraint::Length(popup_width),
        Constraint::Max(u16::try_from(total + 2).unwrap_or(u16::MAX)),
    );
    frame.render_widget(Clear, popup);
    let block = titled(format!(" {} ", form.title));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    // A form taller than the screen scrolls with the focus.
    let visible = usize::from(inner.height);
    let (first, end) = focus_lines;
    let offset = end.saturating_sub(visible).min(first);
    frame.render_widget(
        Paragraph::new(lines).scroll((u16::try_from(offset).unwrap_or_default(), 0)),
        inner,
    );
    super::list::scrollbar(frame, popup, 1, 1, total, offset, visible);
    if let Some((line, col)) = cursor
        && line >= offset
        && line < offset + visible
    {
        frame.set_cursor_position(Position::new(
            inner.x + LABEL_WIDTH + 1 + u16::try_from(col).unwrap_or_default(),
            inner.y + u16::try_from(line - offset).unwrap_or_default(),
        ));
    }
}

/// The value part of a field line and the cursor column for text fields.
fn field_value(field: &Field, focused: bool, width: usize) -> (Vec<Span<'static>>, Option<usize>) {
    let style = if !field.enabled {
        theme::dim()
    } else if focused {
        theme::focused()
    } else {
        theme::field()
    };
    let invalid = field.visible_error(true).is_some() && field.enabled;
    let style = if invalid && !focused {
        style.patch(theme::bad())
    } else {
        style
    };
    match &field.input {
        _ if !field.enabled => (vec![Span::styled("-", style)], None),
        Input::Text(text) => {
            let box_width = width.clamp(1, 40);
            let (shown, col) = text.window(box_width.saturating_sub(1));
            let padded = format!("{shown:<box_width$}");
            (vec![Span::styled(padded, style)], focused.then_some(col))
        }
        Input::Choice { options, .. } => {
            let value = field.value();
            let text = if options.is_empty() {
                "(none available)".to_string()
            } else if focused {
                format!("< {value} >")
            } else {
                format!("  {value}  ")
            };
            let mut spans = vec![Span::styled(text, style)];
            if focused && options.len() > 1 {
                spans.push(Span::styled(
                    format!("  {} choices", options.len()),
                    theme::dim(),
                ));
            }
            (spans, None)
        }
    }
}

pub fn ask(frame: &mut Frame, ask: &Ask, area: Rect) {
    let lines = format::wrap(&ask.text, 56);
    let height = u16::try_from(lines.len() + 4).unwrap_or(u16::MAX);
    let popup = area.centered(Constraint::Max(60), Constraint::Max(height));
    frame.render_widget(Clear, popup);
    let mut text: Vec<Line> = lines
        .into_iter()
        .map(|l| Line::raw(format!(" {l}")))
        .collect();
    text.push(Line::raw(""));
    let (yes, no) = if ask.action.enter_means_yes() {
        (" y/Enter ", " n/Esc ")
    } else {
        (" y ", " n/Enter ")
    };
    text.push(Line::from(vec![
        Span::styled(yes, theme::key()),
        Span::raw(" yes   "),
        Span::styled(no, theme::key()),
        Span::raw(" no"),
    ]));
    frame.render_widget(
        Paragraph::new(text).block(titled(format!(" {} ", ask.title)).border_style(theme::warn())),
        popup,
    );
}

/// The last messages of the status line, newest first.
pub fn messages(frame: &mut Frame, app: &App, scroll: u16, area: Rect) {
    frame.render_widget(Clear, area);
    let lines: Vec<Line> = if app.messages.is_empty() {
        vec![Line::styled(" No messages yet.", theme::dim())]
    } else {
        app.messages
            .iter()
            .map(|m| {
                Line::styled(
                    format!(" {}", m.text),
                    match m.kind {
                        MessageKind::Info => Style::new(),
                        MessageKind::Success => theme::ok(),
                        MessageKind::Error => theme::bad(),
                    },
                )
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(titled(" Messages, newest first (Esc closes) "))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

pub fn help(frame: &mut Frame, app: &App, scroll: u16, area: Rect) {
    frame.render_widget(Clear, area);
    let lines: Vec<Line> = help::lines()
        .into_iter()
        .map(|l| match l {
            HelpLine::Title(t) => Line::styled(format!(" {t}"), theme::header()),
            HelpLine::Key(k, w) => Line::from(vec![
                Span::styled(format!("   {k:<20}"), theme::bold()),
                Span::raw(w),
            ]),
            HelpLine::Text(t) => Line::styled(format!("   {t}"), Style::new()),
            HelpLine::Blank => Line::raw(""),
        })
        .collect();
    // Wrapped lines decide how far it scrolls.
    let width = usize::from(area.width.saturating_sub(2).max(1));
    let total: usize = lines.iter().map(|l| l.width().div_ceil(width).max(1)).sum();
    let visible = usize::from(area.height.saturating_sub(2));
    let max = u16::try_from(total.saturating_sub(visible)).unwrap_or(u16::MAX);
    app.scrolls.help_max.set(max.max(1));
    let scroll = scroll.min(max);
    frame.render_widget(
        Paragraph::new(lines)
            .block(titled(" Help (Esc closes, Up/Down scrolls) "))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
    super::list::scrollbar(frame, area, 1, 1, total, usize::from(scroll), visible);
}
