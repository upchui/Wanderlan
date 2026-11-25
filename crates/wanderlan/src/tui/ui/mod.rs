//! Rendering. Everything here only reads the [`App`]; it is drawn anew on
//! every frame.
//!
//! Screen layout (80×24 and up): tab bar, countdown banner (only while a
//! change waits for confirmation), the view, a status line and the help bar.

mod details;
pub mod list;
mod log;
mod macs;
mod overview;
mod popup;
mod ports;
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
use wanderlan_core::api::{PeerStatus, PortState, SiteState};
use wanderlan_core::config::{Diagnostic, Severity};

use super::app::{
    App, BRAND, Busy, Conn, MessageKind, Modal, URGENT_SECS, View, banner_text, tab_layout,
};
use super::{format, help, theme};
use overview::{Health, health};

/// Smallest terminal the views are made for.
pub const MIN_SIZE: (u16, u16) = (80, 24);

/// Draws the whole screen.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width < MIN_SIZE.0 || area.height < MIN_SIZE.1 {
        draw_too_small(frame, area);
        return;
    }
    let pending = app.pending();
    let status_height = status_height(app, area.width);
    let [tabs, banner, body, status, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(pending.is_some())),
        Constraint::Min(0),
        Constraint::Length(status_height),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_tabs(frame, app, tabs);
    if let Some(p) = pending {
        let urgent = p.remaining_secs <= URGENT_SECS;
        frame.render_widget(
            Paragraph::new(banner_text(p, matches!(app.modal, Some(Modal::Form(_)))))
                .style(theme::banner(urgent)),
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
            Modal::Help { scroll } => popup::help(frame, app, *scroll, body),
            Modal::Messages { scroll } => popup::messages(frame, app, *scroll, body),
            Modal::Invites { selected } => details::invites(frame, app, *selected, body),
            Modal::Peer { name } => details::peer(frame, app, name, body),
            Modal::Site { name } => details::site(frame, app, name, body),
            Modal::Ports(p) => ports::draw(frame, app, p, body),
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
        View::Macs => macs::draw(frame, &app.macs, &app.scrolls.macs, area),
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

fn draw_too_small(frame: &mut Frame, area: Rect) {
    let (w, h) = MIN_SIZE;
    let text = vec![
        Line::styled(
            format!(" Terminal too small: {}×{}", area.width, area.height),
            theme::warn(),
        ),
        Line::raw(format!(" wanderlan needs at least {w}×{h}.")),
        Line::styled(" q quits, Ctrl-C too.", theme::dim()),
    ];
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
}

/// Two lines for a message that does not fit into one.
fn status_height(app: &App, width: u16) -> u16 {
    let badge = status_badge(app).width();
    let text = status_text(app).map_or(0, |l| l.width());
    if text + badge > usize::from(width) {
        2
    } else {
        1
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let right = status_badge(app);
    let width = u16::try_from(right.width()).unwrap_or_default();
    let [first, second] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(first);
    frame.render_widget(Paragraph::new(right), right_area);
    let Some(left) = status_text(app) else {
        return;
    };
    let style = left.style;
    let text: String = left.spans.iter().map(|s| s.content.as_ref()).collect();
    if second.height == 0 {
        frame.render_widget(Paragraph::new(Line::styled(text, style)), left_area);
        return;
    }
    // The first line next to the badge, the rest over the full width below.
    let lines = format::wrap(
        text.trim_start(),
        usize::from(left_area.width.saturating_sub(3)),
    );
    let head = lines.first().cloned().unwrap_or_default();
    let rest = text
        .trim_start()
        .get(head.len()..)
        .unwrap_or_default()
        .trim_start();
    let room = usize::from(second.width.saturating_sub(1));
    let tail = if rest.chars().count() > room {
        let cut: String = rest.chars().take(room.saturating_sub(14)).collect();
        format!(" {cut}… M shows all")
    } else {
        format!(" {rest}")
    };
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" {head}"), style)),
        left_area,
    );
    frame.render_widget(Paragraph::new(Line::styled(tail, style)), second);
}

/// The left part of the status line: what is running, the last message,
/// or a hint.
fn status_text(app: &App) -> Option<Line<'static>> {
    Some(match (&app.busy, &app.message) {
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
            " The running configuration changed; x discards the draft.",
            theme::warn(),
        ),
        (None, None) => return None,
    })
}

/// Problems of this site in a few words, for the status line of every view.
fn problems(app: &App) -> Option<(String, Style)> {
    // The overview shows them in full.
    if app.view == View::Overview && app.modal.is_none() {
        return None;
    }
    let s = app.status.as_ref()?;
    let mut bad = Vec::new();
    let mut warn = Vec::new();
    if s.state == SiteState::Error || s.last_error.is_some() {
        bad.push("error".to_string());
    }
    if !s.blocked.is_empty() {
        bad.push(format!("{} blocked", s.blocked.len()));
    }
    if s.sync_problem.is_some() {
        warn.push("not taken over".to_string());
    }
    if !s.missing_ports.is_empty() {
        warn.push(format!("{} port missing", s.missing_ports.len()));
    }
    let refused = s.peers.iter().filter(|p| p.refused.is_some()).count();
    if refused > 0 {
        warn.push(format!("{refused} refused"));
    }
    if bad.is_empty() && warn.is_empty() {
        return None;
    }
    let style = if bad.is_empty() {
        theme::warn()
    } else {
        theme::bad()
    };
    bad.extend(warn);
    Some((format!("{} (see 1)", bad.join(", ")), style))
}

/// Draft and connection state for the right end of the status line.
fn status_badge(app: &App) -> Line<'static> {
    let mut spans = Vec::new();
    if let Some((text, style)) = problems(app) {
        spans.push(Span::styled(text, style));
        spans.push(Span::raw("  "));
    }
    if let Some(draft) = &app.draft {
        let changes = draft.changes(app.running.as_ref());
        let (text, style) = if !app.draft_dirty() {
            ("draft: no changes".to_string(), theme::dim())
        } else if draft.errors() > 0 {
            (format!("draft: {} error(s)", draft.errors()), theme::bad())
        } else if changes == 1 {
            ("draft: 1 change".to_string(), theme::warn())
        } else if changes > 1 {
            (format!("draft: {changes} changes"), theme::warn())
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
    let keys = help::bar(app);
    let width = |(key, what): &(&str, &str)| key.chars().count() + what.chars().count() + 5;
    // The most important keys that fit, shown in their own order.
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by_key(|&i| (help::priority(i, keys[i].0), i));
    let mut shown = vec![false; keys.len()];
    let mut used = 0usize;
    for i in order {
        let w = width(&keys[i]);
        if used + w <= usize::from(area.width) {
            used += w;
            shown[i] = true;
        }
    }
    let mut spans = Vec::new();
    let mut hits = Vec::new();
    let mut x = area.x;
    for (i, (key, what)) in keys.iter().enumerate() {
        if shown[i] {
            let w = u16::try_from(width(&keys[i])).unwrap_or_default();
            hits.push((x, x + w, *key));
            x += w;
            spans.push(Span::styled(format!(" {key} "), theme::key()));
            spans.push(Span::raw(format!(" {what}  ")));
        }
    }
    *app.scrolls.bar.borrow_mut() = hits;
    app.scrolls.bar_row.set(area.y);
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
        Line::raw("(this site and one peer); the rest of the network then arrives by sync."),
    ];
    frame.render_widget(Paragraph::new(text).block(titled(" Draft ")), area);
}

#[cfg(test)]
mod tests;
