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
    let notes = form.notes.len().min(MAX_NOTES);
    let height = 2 + form.fields.len() * 2 + notes + 2;
    let popup = area.centered(
        Constraint::Max(76),
        Constraint::Max(u16::try_from(height).unwrap_or(u16::MAX)),
    );
    frame.render_widget(Clear, popup);
    let block = titled(format!(" {} ", form.title));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let value_width = inner.width.saturating_sub(LABEL_WIDTH + 1);
    let mut lines = Vec::new();
    let mut cursor = None;
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let row = inner.y + u16::try_from(lines.len()).unwrap_or_default();
        let (value, cursor_col) = field_value(field, focused, usize::from(value_width));
        if let Some(col) = cursor_col {
            cursor = Some(Position::new(
                inner.x + LABEL_WIDTH + 1 + u16::try_from(col).unwrap_or_default(),
                row,
            ));
        }
        let label_style = if focused { theme::bold() } else { theme::dim() };
        let mut spans = vec![Span::styled(
            format!(" {:<width$}", field.label, width = usize::from(LABEL_WIDTH)),
            label_style,
        )];
        spans.extend(value);
        lines.push(Line::from(spans));
        let pad = " ".repeat(usize::from(LABEL_WIDTH + 1));
        let below = match (field.visible_error(form.submitted), &field.warning) {
            _ if !field.enabled => Line::styled(format!("{pad}not used here"), theme::dim()),
            (Some(e), _) => Line::styled(format!("{pad}{e}"), theme::bad()),
            (None, Some(w)) => Line::styled(format!("{pad}{w}"), theme::warn()),
            (None, None) => Line::styled(format!("{pad}{}", field.hint), theme::dim()),
        };
        lines.push(below);
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
    frame.render_widget(Paragraph::new(lines), inner);
    if let Some(pos) = cursor {
        frame.set_cursor_position(pos);
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
    text.push(Line::from(vec![
        Span::styled(" y ", theme::key()),
        Span::raw(" yes   "),
        Span::styled(" n ", theme::key()),
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

pub fn help(frame: &mut Frame, scroll: u16, area: Rect) {
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
    frame.render_widget(
        Paragraph::new(lines)
            .block(titled(" Help (Esc closes, Up/Down scrolls) "))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}
