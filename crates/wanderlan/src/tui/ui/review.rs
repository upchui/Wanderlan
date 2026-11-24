//! The review screen: diff against the running configuration, apply, and
//! the result of the apply.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Clear, Paragraph};

use super::{diagnostic_line, diagnostic_text, titled};
use crate::tui::app::{App, Origin, Review, ReviewState};
use crate::tui::diff::DiffKind;
use crate::tui::{format, theme};

/// Rows of the footer at most.
const FOOTER_MAX: usize = 8;

pub fn draw(frame: &mut Frame, app: &App, review: &Review, area: Rect) {
    frame.render_widget(Clear, area);
    let title = match (review.origin, &review.state) {
        (_, ReviewState::Applied(_)) => " Applied - waiting for confirmation ",
        (Origin::Draft, _) => " Review: draft against the running configuration ",
        (Origin::Wizard, _) => " Review: setup against the running configuration ",
    };
    let block = titled(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = usize::from(inner.width);
    let mut footer: Vec<Line> = Vec::new();
    for (text, style) in footer_texts(app, review) {
        footer.extend(
            format::wrap(&text, width)
                .into_iter()
                .map(|l| Line::styled(l, style)),
        );
    }
    footer.truncate(FOOTER_MAX);
    let footer_height = u16::try_from(footer.len()).unwrap_or_default();
    let [main, rule, foot] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(footer_height),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new("─".repeat(width)).style(theme::dim()), rule);
    frame.render_widget(Paragraph::new(footer), foot);

    let lines: Vec<Line> = match &review.state {
        ReviewState::Applied(applied) => {
            let mut lines = vec![Line::styled("Changes made to the system:", theme::bold())];
            if applied.changes.is_empty() {
                lines.push(Line::styled("  none", theme::dim()));
            }
            lines.extend(
                applied
                    .changes
                    .iter()
                    .map(|c| Line::raw(format!("  - {c}"))),
            );
            if !applied.warnings.is_empty() {
                lines.push(Line::raw(""));
                lines.extend(applied.warnings.iter().map(diagnostic_line));
            }
            lines
        }
        _ => review
            .lines
            .iter()
            .map(|l| {
                let style = match l.kind {
                    DiffKind::Header => theme::bold(),
                    DiffKind::Hunk => Style::new().fg(Color::Cyan),
                    DiffKind::Added => theme::ok(),
                    DiffKind::Removed => theme::bad(),
                    DiffKind::Context => Style::new(),
                    DiffKind::Note => theme::dim(),
                };
                Line::styled(l.text.clone(), style)
            })
            .collect(),
    };
    frame.render_widget(Paragraph::new(lines).scroll((review.scroll, 0)), main);
}

fn footer_texts(app: &App, review: &Review) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    let strong_bad = theme::bad().patch(theme::bold());
    match &review.state {
        ReviewState::Ready if review.is_valid() => {
            out.push((
                format!(
                    "Enter applies the change. Then confirm it with c within {}; otherwise it is rolled back automatically.",
                    format::duration(review.commit_timeout)
                ),
                Style::new(),
            ));
            out.extend(
                review
                    .problems
                    .iter()
                    .filter(|d| !d.is_error())
                    .map(diagnostic_text),
            );
        }
        ReviewState::Ready => {
            let errors = review.problems.iter().filter(|d| d.is_error()).count();
            out.push((
                format!("Cannot apply: {errors} error(s). Esc goes back to fix them."),
                strong_bad,
            ));
            out.extend(
                review
                    .problems
                    .iter()
                    .filter(|d| d.is_error())
                    .map(diagnostic_text),
            );
        }
        ReviewState::Applying => out.push(("Applying ...".into(), theme::warn())),
        ReviewState::Failed(e) => {
            out.push((
                format!("The daemon refused the change: {}", e.message),
                strong_bad,
            ));
            out.extend(e.diagnostics.iter().map(diagnostic_text));
            out.push((
                "Enter tries again, Esc goes back to the draft.".into(),
                Style::new(),
            ));
        }
        ReviewState::Applied(applied) => {
            let secs = app
                .pending()
                .map_or(applied.confirm_within_secs, |p| p.remaining_secs);
            out.push((
                format!(
                    "Check that everything still works (Esc shows the overview), then press c to keep the change. Without confirmation it is rolled back in {secs} s; r rolls back now."
                ),
                theme::warn(),
            ));
        }
    }
    out
}
