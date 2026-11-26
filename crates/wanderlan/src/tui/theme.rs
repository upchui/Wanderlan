//! Styles. Only the 16 basic terminal colours are used so that the UI looks
//! the same over SSH and in any terminal.

use ratatui::style::{Color, Modifier, Style};
use wanderlan_core::config::Severity;

/// Healthy / added.
pub fn ok() -> Style {
    Style::new().fg(Color::Green)
}

/// Degraded / warning.
pub fn warn() -> Style {
    Style::new().fg(Color::Yellow)
}

/// Failed / error / removed.
pub fn bad() -> Style {
    Style::new().fg(Color::Red)
}

/// Received traffic (in) in graphs and rate columns.
pub fn rx() -> Style {
    Style::new().fg(Color::Green)
}

/// Sent traffic (out): its own colour, not a warning.
pub fn tx() -> Style {
    Style::new().fg(Color::Magenta)
}

/// Secondary text: a fainter form of the terminal's own colour (dark
/// grey is invisible on many dark themes).
pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// Emphasis.
pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Table headers.
pub fn header() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
}

/// Block titles.
pub fn title() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
}

/// The selected row of a table.
pub fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// A key in the help bar.
pub fn key() -> Style {
    Style::new().fg(Color::Black).bg(Color::Cyan)
}

/// The active tab.
pub fn active_tab() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// The focused input field.
pub fn focused() -> Style {
    Style::new().fg(Color::Black).bg(Color::Gray)
}

/// An input field without focus.
pub fn field() -> Style {
    Style::new().add_modifier(Modifier::UNDERLINED)
}

/// The countdown banner; red when the rollback is imminent.
pub fn banner(urgent: bool) -> Style {
    let style = Style::new().add_modifier(Modifier::BOLD);
    if urgent {
        style.fg(Color::White).bg(Color::Red)
    } else {
        style.fg(Color::Black).bg(Color::Yellow)
    }
}

/// Colour of a diagnostic.
pub fn severity(severity: Severity) -> Style {
    match severity {
        Severity::Error => bad(),
        Severity::Warning => warn(),
    }
}
