//! View 1: this site, live status of the direct peers, all sites.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use wanderlan_core::api::{PeerStatus, PortState, SiteState, SiteSummary, Status};

use super::{health_style, port_state, titled};
use crate::tui::app::App;
use crate::tui::{format, theme};

/// Seconds without a reply after which a reachable peer counts as degraded.
const LATE_REPLY_SECS: u64 = 15;

/// Overall state of a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Reachable, port up, same configuration.
    Good,
    /// Reachable but something is off.
    Degraded,
    /// Unreachable or blocked.
    Bad,
}

/// Classifies a peer for colouring.
pub fn health(peer: &PeerStatus) -> Health {
    if !peer.reachable || peer.port_state == PortState::Blocked {
        Health::Bad
    } else if peer.port_state != PortState::Up
        || peer.in_sync == Some(false)
        || peer.last_reply_secs.is_some_and(|s| s > LATE_REPLY_SECS)
    {
        Health::Degraded
    } else {
        Health::Good
    }
}

pub fn draw(frame: &mut Frame, app: &App, status: &Status, area: Rect) {
    let header = header_lines(app, status);
    let peer_rows = u16::try_from(status.peers.len().max(1)).unwrap_or(u16::MAX);
    let [head, peers, sites] = Layout::vertical([
        Constraint::Length(u16::try_from(header.len()).unwrap_or_default()),
        Constraint::Length(peer_rows.saturating_add(3)),
        Constraint::Min(4),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(header), head);
    draw_peers(frame, app, status, peers);
    draw_sites(frame, status, sites);
}

fn header_lines(app: &App, status: &Status) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match &status.site {
        Some(site) => {
            let value = |v: &Option<String>| v.clone().unwrap_or_else(|| "?".into());
            lines.push(Line::from(vec![
                Span::styled(format!(" {site}"), theme::bold()),
                Span::styled("  address ", theme::dim()),
                Span::raw(value(&status.address)),
                Span::styled("  trunk ", theme::dim()),
                Span::raw(value(&status.trunk)),
                Span::styled("  VLANs ", theme::dim()),
                Span::raw(value(&status.vlans)),
            ]));
        }
        None => lines.push(Line::styled(
            " This site is not configured yet. Press 6 to start a new network or join one.",
            theme::warn(),
        )),
    }
    let (state, style) = match status.state {
        SiteState::Running => ("running", theme::ok()),
        SiteState::Error => ("error", theme::bad()),
        SiteState::Unconfigured => ("not configured", theme::warn()),
    };
    lines.push(Line::from(vec![
        Span::styled(" state ", theme::dim()),
        Span::styled(state, style),
        Span::styled("  mesh revision ", theme::dim()),
        Span::raw(format!(
            "{} ({})",
            status.revision,
            format::short_fingerprint(&status.fingerprint)
        )),
        Span::styled("  daemon ", theme::dim()),
        Span::raw(status.version.clone()),
    ]));
    if !status.peers.is_empty() {
        let rx: u64 = status.peers.iter().map(|p| p.rx_bps).sum();
        let tx: u64 = status.peers.iter().map(|p| p.tx_bps).sum();
        let mut spans = vec![
            Span::styled(" tunnels ", theme::dim()),
            Span::styled(format!("in {}bit/s", format::rate(rx)), theme::ok()),
            Span::styled(format!("  out {}bit/s", format::rate(tx)), theme::warn()),
        ];
        if let Some(t) = &status.trunk_traffic {
            spans.push(Span::styled(format!("  trunk {} ", t.name), theme::dim()));
            spans.push(Span::raw(format!(
                "in {}bit/s out {}bit/s",
                format::rate(t.rx_bps),
                format::rate(t.tx_bps)
            )));
        }
        spans.push(Span::styled("  (Enter: details)", theme::dim()));
        lines.push(Line::from(spans));
    }
    match (&app.invitation, status.invites) {
        (_, 0) => {}
        (Some(inv), _) => lines.push(Line::from(vec![
            Span::styled(" Invitation: ", theme::warn()),
            Span::styled(inv.command.clone(), theme::bold()),
        ])),
        (None, n) => lines.push(Line::from(vec![
            Span::styled(format!(" {n} open invitation(s)"), theme::warn()),
            Span::styled("  (Sites: I)", theme::dim()),
        ])),
    }
    if !status.blocked.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" Blocked by the loop guard: ", theme::bad()),
            Span::styled(status.blocked.join(", "), theme::bad().patch(theme::bold())),
            Span::styled("  (select the peer, press u)", theme::dim()),
        ]));
    }
    if let Some(e) = &status.last_error {
        lines.push(Line::styled(format!(" Last error: {e}"), theme::bad()));
    }
    lines
}

fn config_cell(peer: &PeerStatus) -> String {
    match (peer.revision, peer.in_sync) {
        (Some(r), Some(true)) => format!("r{r} ok"),
        (Some(r), Some(false)) => format!("r{r} diff"),
        (Some(r), None) => format!("r{r}"),
        (None, _) => "-".into(),
    }
}

fn peer_row(peer: &PeerStatus) -> Row<'static> {
    let opt = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    let reply = if peer.reachable {
        opt(peer.last_reply_secs.map(format::age))
    } else {
        "lost".into()
    };
    Row::new(vec![
        Cell::from(peer.name.clone()),
        Cell::from(port_state(peer.port_state)),
        Cell::from(reply),
        Cell::from(opt(peer.rtt_ms.map(format::rtt))),
        Cell::from(opt(peer.pmtu.map(|v| v.to_string()))),
        Cell::from(opt(peer.mss.map(|v| v.to_string()))),
        Cell::from(Line::raw(peer.macs.to_string()).right_aligned()),
        Cell::from(Line::raw(format::rate(peer.rx_bps)).right_aligned()),
        Cell::from(Line::raw(format::rate(peer.tx_bps)).right_aligned()),
        Cell::from(config_cell(peer)),
    ])
    .style(health_style(peer))
}

fn draw_peers(frame: &mut Frame, app: &App, status: &Status, area: Rect) {
    let header = Row::new(vec![
        "PEER", "PORT", "REPLY", "RTT", "PMTU", "MSS", "MACS", "RX b/s", "TX b/s", "CONFIG",
    ])
    .style(theme::header());
    let rows: Vec<Row> = if status.peers.is_empty() {
        vec![Row::new(vec![Cell::from("no direct peers")]).style(theme::dim())]
    } else {
        status.peers.iter().map(peer_row).collect()
    };
    let widths = [
        Constraint::Length(12),
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Min(7),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(titled(" Direct peers (live) "))
        .row_highlight_style(theme::selected());
    let mut state = TableState::default();
    if !status.peers.is_empty() {
        state.select(Some(app.overview_selected));
    }
    frame.render_stateful_widget(table, area, &mut state);
}

/// How traffic reaches a site from here.
fn link(status: &Status, site: &SiteSummary) -> (String, Style) {
    if site.is_self {
        return ("this site".into(), theme::bold());
    }
    if let Some(peer) = status.peers.iter().find(|p| p.name == site.name) {
        return (
            format!("tunnel {}", port_state(peer.port_state)),
            health_style(peer),
        );
    }
    if site.direct {
        return ("tunnel".into(), Style::new());
    }
    ("-".into(), theme::dim())
}

fn draw_sites(frame: &mut Frame, status: &Status, area: Rect) {
    let header = Row::new(vec!["SITE", "ADDRESS", "VLANS", "LINK"]).style(theme::header());
    let rows: Vec<Row> = status
        .sites
        .iter()
        .map(|site| {
            let (link, style) = link(status, site);
            Row::new(vec![
                Cell::from(site.name.clone()),
                Cell::from(site.address.clone()),
                Cell::from(site.vlans.clone()),
                Cell::from(link).style(style),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(12),
        Constraint::Length(15),
        Constraint::Length(20),
        Constraint::Min(10),
    ];
    let title = format!(" Network: {} sites ", status.sites.len());
    frame.render_widget(
        Table::new(rows, widths).header(header).block(titled(title)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::fixtures;

    #[test]
    fn health_classes() {
        let peers = fixtures::status().peers;
        let by_name = |n: &str| peers.iter().find(|p| p.name == n).unwrap();
        assert_eq!(health(by_name("buero")), Health::Good);
        assert_eq!(health(by_name("haus2")), Health::Bad);
        assert_eq!(health(by_name("haus5")), Health::Degraded);
        assert_eq!(health(by_name("haus6")), Health::Bad);
    }
}
