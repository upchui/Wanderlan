//! Rendering. Everything here only reads the [`App`]; it is drawn anew on
//! every frame.
//!
//! Screen layout (80×24 and up): tab bar, countdown banner (only while a
//! change waits for confirmation), the view, a status line and the help bar.

mod details;
mod log;
mod macs;
mod overview;
mod popup;
mod review;
pub mod settings;
mod sites;
mod vlans;
mod wizard;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use wanderlan_core::api::{PeerStatus, PortState};
use wanderlan_core::config::{Diagnostic, Severity};

use super::app::{
    App, BRAND, Busy, Conn, MessageKind, Modal, URGENT_SECS, View, banner_text, tab_layout,
};
use super::{help, theme};
use overview::{Health, health};

/// Draws the whole screen.
pub fn draw(frame: &mut Frame, app: &App) {
    let pending = app.pending();
    let [tabs, banner, body, status, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(pending.is_some())),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_tabs(frame, app, tabs);
    if let Some(p) = pending {
        let urgent = p.remaining_secs <= URGENT_SECS;
        frame.render_widget(
            Paragraph::new(banner_text(p)).style(theme::banner(urgent)),
            banner,
        );
    }
    draw_body(frame, app, body);
    draw_status(frame, app, status);
    draw_help_bar(frame, app, keys);
    if let Some(modal) = &app.modal {
        match modal {
            Modal::Review(r) => review::draw(frame, app, r, body),
            Modal::Form(f) => popup::form(frame, f, body),
            Modal::Ask(a) => popup::ask(frame, a, body),
            Modal::Help { scroll } => popup::help(frame, *scroll, body),
            Modal::Invites { selected } => details::invites(frame, app, *selected, body),
            Modal::Peer { name } => details::peer(frame, app, name, body),
        }
    }
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(BRAND, theme::bold())];
    for (view, label, _, _) in tab_layout() {
        let style = if view == app.view {
            theme::active_tab()
        } else {
            Style::new()
        };
        spans.push(Span::styled(label, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(frame: &mut Frame, app: &App, area: Rect) {
    match &app.conn {
        Conn::Down { error, retry_in } => {
            return offline(frame, app, error, Some(*retry_in), area);
        }
        Conn::Connecting if app.attempts > 0 => {
            return offline(frame, app, "connecting...", None, area);
        }
        Conn::Connecting | Conn::Connected => {}
    }
    let Some(status) = &app.status else {
        let text = format!("Connecting to the daemon at {} ...", app.socket);
        frame.render_widget(Paragraph::new(text).style(theme::dim()), area);
        return;
    };
    match app.view {
        View::Overview => overview::draw(frame, app, status, area),
        View::Sites => sites::draw(frame, app, area),
        View::Vlans => vlans::draw(frame, app, area),
        View::Macs => macs::draw(frame, &app.macs, area),
        View::Log => log::draw(frame, &app.log, area),
        View::Setup => wizard::draw(frame, app, area),
        View::Settings => settings::draw(frame, app, area),
    }
}

/// The "daemon not reachable" screen. The TUI stays usable and retries.
fn offline(frame: &mut Frame, app: &App, error: &str, retry_in: Option<u32>, area: Rect) {
    let journal = app.diagnosis.as_ref().map_or(&[][..], |d| &d.journal[..]);
    let extra = app.diagnosis.as_ref().map_or(0, |d| {
        d.summary().map_or(0, |_| 3)
            + if journal.is_empty() {
                0
            } else {
                journal.len() + 2
            }
    });
    let height = u16::try_from(11 + extra).unwrap_or(u16::MAX);
    let popup = area.centered(Constraint::Max(74), Constraint::Max(height));
    let mut lines = vec![Line::styled(error.to_string(), theme::bad()), Line::raw("")];
    if let Some(summary) = app.diagnosis.as_ref().and_then(|d| d.summary()) {
        lines.push(Line::styled(summary, theme::bold()));
        lines.push(Line::raw(""));
    }
    if !journal.is_empty() {
        lines.push(Line::styled("Last log lines of the daemon:", theme::dim()));
        lines.extend(journal.iter().map(|l| Line::raw(format!("  {l}"))));
        lines.push(Line::raw(""));
    }
    let retry = match retry_in {
        Some(secs) => format!(
            "Retrying in {secs} s (attempt {}). The TUI keeps running; the draft is kept.",
            app.attempts
        ),
        None => "Connecting ...".to_string(),
    };
    lines.push(Line::raw(retry));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!(
            "Socket: {} (change with --socket or WANDERLAN_SOCKET)",
            app.socket
        ),
        theme::dim(),
    ));
    let block = titled(" Daemon not reachable ").border_style(theme::bad());
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let right = status_badge(app);
    let width = u16::try_from(right.width()).unwrap_or_default();
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(area);
    let left = match (&app.busy, &app.message) {
        (Some(busy), _) => Line::styled(
            match busy {
                Busy::Applying => " Applying ...",
                Busy::Confirming => " Confirming ...",
                Busy::RollingBack => " Rolling back ...",
            },
            theme::warn(),
        ),
        (None, Some(m)) => Line::styled(
            format!(" {}", m.text),
            match m.kind {
                MessageKind::Info => Style::new(),
                MessageKind::Success => theme::ok(),
                MessageKind::Error => theme::bad(),
            },
        ),
        (None, None) if app.draft_stale() && app.draft_dirty() => Line::styled(
            " The running configuration changed since the draft was made; x discards the draft.",
            theme::warn(),
        ),
        (None, None) => Line::raw(""),
    };
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(Paragraph::new(right), right_area);
}

/// Draft and connection state for the right end of the status line.
fn status_badge(app: &App) -> Line<'static> {
    let mut spans = Vec::new();
    if let Some(draft) = &app.draft {
        let (text, style) = if !app.draft_dirty() {
            ("draft: no changes".to_string(), theme::dim())
        } else if draft.errors() > 0 {
            (format!("draft: {} error(s)", draft.errors()), theme::bad())
        } else {
            ("draft: modified".to_string(), theme::warn())
        };
        spans.push(Span::styled(text, style));
        spans.push(Span::raw("  "));
    }
    match &app.conn {
        Conn::Connected => {
            if let Some(s) = &app.status {
                let name = s.site.as_deref().unwrap_or("unconfigured");
                spans.push(Span::styled(format!("{name} r{}", s.revision), theme::ok()));
            }
        }
        Conn::Connecting | Conn::Down { .. } => spans.push(Span::styled("offline", theme::bad())),
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn draw_help_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (key, what) in help::bar(app) {
        let key = format!(" {key} ");
        let what = format!(" {what}  ");
        let width = key.chars().count() + what.chars().count();
        if used + width > usize::from(area.width) {
            break;
        }
        used += width;
        spans.push(Span::styled(key, theme::key()));
        spans.push(Span::raw(what));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A bordered block with a styled title.
fn titled(title: impl Into<String>) -> Block<'static> {
    Block::bordered().title(Span::styled(title.into(), theme::title()))
}

/// Name of a port state.
fn port_state(state: PortState) -> &'static str {
    match state {
        PortState::Up => "up",
        PortState::Down => "down",
        PortState::Blocked => "BLOCKED",
        PortState::Missing => "missing",
        PortState::Offline => "offline",
    }
}

/// Style for a peer row by its health.
fn health_style(peer: &PeerStatus) -> Style {
    match health(peer) {
        Health::Good => theme::ok(),
        Health::Degraded => theme::warn(),
        Health::Bad => theme::bad(),
    }
}

/// Style for a row with diagnostics of the given worst severity.
fn severity_style(severity: Option<Severity>) -> Style {
    severity.map_or_else(Style::new, theme::severity)
}

/// A diagnostic as text (`error: ...` / `warning: ...`) and its style.
fn diagnostic_text(d: &Diagnostic) -> (String, Style) {
    let label = match d.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    (
        format!("{label}: {}", d.message),
        theme::severity(d.severity),
    )
}

/// A diagnostic as one line.
fn diagnostic_line(d: &Diagnostic) -> Line<'static> {
    let (text, style) = diagnostic_text(d);
    Line::styled(text, style)
}

/// Shown in the editing views of a site without configuration.
fn not_configured(frame: &mut Frame, area: Rect) {
    let text = vec![
        Line::styled("This site is not configured yet.", theme::warn()),
        Line::raw(""),
        Line::raw("Press 6 to run the setup wizard. It creates a minimal configuration"),
        Line::raw("(this site and one peer); the rest of the mesh then arrives by sync."),
    ];
    frame.render_widget(Paragraph::new(text).block(titled(" Draft ")), area);
}

#[cfg(test)]
mod tests;
