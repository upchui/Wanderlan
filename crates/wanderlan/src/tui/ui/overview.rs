//! View 1: this site, live status of the direct peers, all sites.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, Wrap};
use wanderlan_core::api::{PeerStatus, PortState, SiteState, SiteSummary, Status};
use wanderlan_core::topology;

use super::{health_style, list, port_state, titled};
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
        || peer.refused.is_some()
        || peer.last_reply_secs.is_some_and(|s| s > LATE_REPLY_SECS)
    {
        Health::Degraded
    } else {
        Health::Good
    }
}

pub fn draw(frame: &mut Frame, app: &App, status: &Status, area: Rect) {
    let header = header_lines(app, status);
    // Header lines wrap instead of being cut.
    let width = usize::from(area.width.max(1));
    let head_height: usize = header
        .iter()
        .map(|l| l.width().div_ceil(width).max(1))
        .sum();
    let head_height = u16::try_from(head_height).unwrap_or_default();
    // The traffic of all tunnels, where there is room for it besides the
    // lists.
    let graph_height = if area.height.saturating_sub(head_height) >= GRAPH_FROM {
        GRAPH_HEIGHT
    } else {
        0
    };
    let rest = area.height.saturating_sub(head_height + graph_height);
    // Both lists get what they need if it fits, otherwise a share of it.
    let want_peers = u16::try_from(status.peers.len().max(1) + 3).unwrap_or(u16::MAX);
    let want_sites = u16::try_from(status.sites.len().max(1) + 3).unwrap_or(u16::MAX);
    let peers_height = if want_peers.saturating_add(want_sites) <= rest {
        want_peers
    } else {
        let share = u32::from(rest) * u32::from(want_peers)
            / (u32::from(want_peers) + u32::from(want_sites)).max(1);
        // The peers (the live state) get at least half.
        u16::try_from(share)
            .unwrap_or(want_peers)
            .max(rest / 2)
            .clamp(4.min(rest), want_peers)
    };
    let [head, graph, peers, sites] = Layout::vertical([
        Constraint::Length(head_height),
        Constraint::Length(graph_height),
        Constraint::Length(peers_height),
        Constraint::Min(4),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(header).wrap(Wrap { trim: false }), head);
    if graph_height > 0 {
        super::graph::traffic(
            frame,
            graph,
            "All tunnels",
            &app.recorded.total.iter().copied().collect::<Vec<_>>(),
            app.recorded.step_ms(),
            app.recorded.now_ms,
            app.graph_range,
        );
    }
    draw_peers(frame, app, status, peers);
    draw_sites(frame, app, status, sites);
}

fn header_lines(app: &App, status: &Status) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match &status.site {
        Some(site) => {
            let value = |v: &Option<String>| v.clone().unwrap_or_else(|| "?".into());
            let role = status
                .sites
                .iter()
                .find(|s| s.is_self)
                .map_or_else(String::new, |s| {
                    format::role(s.uplink.as_deref(), format::serves(&status.sites, &s.name))
                });
            lines.push(Line::from(vec![
                Span::styled(format!(" {site}"), theme::bold()),
                Span::styled(format!("  {role}"), theme::title()),
                Span::styled("  address ", theme::dim()),
                Span::raw(value(&status.address)),
                Span::styled("  trunk ", theme::dim()),
                Span::raw(if status.trunk_ports.len() > 1 {
                    status.trunk_ports.join(", ")
                } else {
                    value(&status.trunk)
                }),
                Span::styled("  VLANs ", theme::dim()),
                Span::raw(value(&status.vlans)),
            ]));
        }
        None => lines.push(Line::styled(
            " This site is not configured yet. Press 6 to start a new network or join one.",
            theme::warn(),
        )),
    }
    if status.site.is_some() && !status.online {
        lines.push(Line::styled(
            " This site is OFFLINE: its tunnels stay down (Sites (2): o, then a).",
            theme::warn(),
        ));
    }
    // Other sites that are offline (a site that just joined starts so).
    let offline: Vec<&str> = status
        .sites
        .iter()
        .filter(|s| !s.online && !s.is_self)
        .map(|s| s.name.as_str())
        .collect();
    if !offline.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(format!(" Offline: {}", offline.join(", ")), theme::warn()),
            Span::styled("  (o on the site below takes it online)", theme::dim()),
        ]));
    }
    if let Some(manager) = app.restricted_by() {
        lines.push(Line::styled(
            format!(" Only the server {manager} changes the tunnel of this site: VLANs, mappings and online state change there."),
            theme::warn(),
        ));
    }
    let (state, style) = match status.state {
        SiteState::Running => ("running", theme::ok()),
        SiteState::Error => ("error", theme::bad()),
        SiteState::Unconfigured => ("not configured", theme::warn()),
    };
    lines.push(Line::from(vec![
        Span::styled(" state ", theme::dim()),
        Span::styled(state, style),
        Span::styled("  network revision ", theme::dim()),
        Span::raw(format!(
            "{} ({})",
            status.revision,
            format::short_fingerprint(&status.fingerprint)
        )),
        // The identity of this box (what the other sites know it by).
        Span::styled("  identity ", theme::dim()),
        Span::raw(
            status
                .site_id
                .as_deref()
                .and_then(|t| t.parse::<wanderlan_core::identity::SiteId>().ok())
                .map_or_else(|| "-".to_string(), |id| id.fingerprint()),
        ),
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
        // Further trunks (tunnels with a trunk of their own).
        for t in status.trunks.iter().skip(1) {
            let peers: Vec<&str> = status
                .peers
                .iter()
                .filter(|p| p.trunk.split('+').any(|port| port == t.name))
                .map(|p| p.name.as_str())
                .collect();
            let of = if status.trunk_ports.contains(&t.name) {
                "  a port of the default trunk".to_string()
            } else {
                format!("  segment of {}", peers.join(", "))
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" trunk {} ", t.name), theme::dim()),
                Span::raw(format!(
                    "in {}bit/s out {}bit/s",
                    format::rate(t.rx_bps),
                    format::rate(t.tx_bps)
                )),
                Span::styled(of, theme::dim()),
            ]));
        }
    }
    if !status.missing_ports.is_empty() {
        lines.push(Line::styled(
            format!(
                " Trunk port {} missing (unplugged?); the other ports keep working",
                status.missing_ports.join(", ")
            ),
            theme::warn(),
        ));
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
    // Tunnel ports are unblocked at their peer row, trunk ports with U.
    let (tunnels, trunks): (Vec<&String>, Vec<&String>) = status
        .blocked
        .iter()
        .partition(|b| status.peers.iter().any(|p| p.port == **b));
    for (ports, how) in [
        (tunnels, "  (select the peer, press u)"),
        (trunks, "  (trunk port: U unblocks)"),
    ] {
        if !ports.is_empty() {
            let names: Vec<&str> = ports.iter().map(|p| p.as_str()).collect();
            lines.push(Line::from(vec![
                Span::styled(" Blocked by the loop guard: ", theme::bad()),
                Span::styled(names.join(", "), theme::bad().patch(theme::bold())),
                Span::styled(how, theme::dim()),
            ]));
        }
    }
    if let Some(e) = &status.last_error {
        lines.push(Line::styled(format!(" Last error: {e}"), theme::bad()));
    }
    if let Some(problem) = &status.sync_problem {
        lines.push(Line::styled(format!(" {problem}"), theme::warn()));
    }
    lines
}

fn config_cell(peer: &PeerStatus) -> String {
    use wanderlan_core::api::IdentityState;
    if peer.refused.is_some() {
        return "refused".into();
    }
    match peer.identity_state {
        Some(IdentityState::WrongKey) => return "wrong key".into(),
        Some(IdentityState::Waiting) => return "no key yet".into(),
        _ => {}
    }
    match (peer.revision, peer.in_sync) {
        (Some(r), Some(true)) => format!("r{r} ok"),
        (Some(r), Some(false)) => format!("r{r} diff"),
        (Some(r), None) => format!("r{r}"),
        (None, _) => "-".into(),
    }
}

fn peer_row(peer: &PeerStatus, wide: bool, load: Option<String>) -> Row<'static> {
    let opt = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    let reply = if peer.reachable {
        opt(peer.last_reply_secs.map(format::age))
    } else {
        "lost".into()
    };
    let mut cells = vec![
        Cell::from(peer.name.clone()),
        Cell::from(port_state(peer.port_state)),
        Cell::from(reply),
        Cell::from(opt(peer.rtt_ms.map(format::rtt))),
        Cell::from(opt(peer.pmtu.map(|v| v.to_string()))),
        Cell::from(opt(peer.mss.map(|v| v.to_string()))),
        Cell::from(Line::raw(peer.macs.to_string()).right_aligned()),
        Cell::from(Line::raw(format::rate(peer.rx_bps)).right_aligned()),
        Cell::from(Line::raw(format::rate(peer.tx_bps)).right_aligned()),
    ];
    if let Some(load) = load {
        cells.push(Cell::from(Span::styled(load, theme::title())));
    }
    cells.push(Cell::from(config_cell(peer)));
    if wide {
        cells.push(Cell::from(peer.vlans.clone()));
        cells.push(Cell::from(peer.trunk.clone()));
    }
    Row::new(cells).style(health_style(peer))
}

/// From this width on, the peer table also shows VLANs and trunk.
const WIDE: u16 = 110;
/// From this width on, it shows the load of each tunnel over time.
const LOAD_FROM: u16 = 88;
/// Rows of the graph of all tunnels.
const GRAPH_HEIGHT: u16 = 9;
/// From this height (below the head) on, the overview shows it.
const GRAPH_FROM: u16 = 26;

/// The traffic (in + out) of each peer's tunnel over the time range, in
/// `width` characters, all on one scale: who has load stands out.
fn loads(app: &App, status: &Status, width: usize) -> Vec<String> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let parts: Vec<Vec<Option<u64>>> = status
        .peers
        .iter()
        .map(|p| {
            let points: Vec<_> = app
                .recorded
                .peers
                .get(&p.name)
                .map(|r| r.traffic.iter().copied().collect())
                .unwrap_or_default();
            super::graph::grid(
                &points,
                app.recorded.step_ms(),
                app.graph_range.ms(),
                width,
                app.recorded.now_ms,
                |t: &wanderlan_core::api::TrafficPoint| t.0,
                |t| Some(((t.1 + t.2) as f64, 0.0)),
            )
            .into_iter()
            .map(|b| b.map(|(v, _)| v as u64))
            .collect()
        })
        .collect();
    // A round scale: it changes only when a peak comes or goes.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let max = super::graph::nice(parts.iter().flatten().flatten().copied().max().unwrap_or(0) as f64)
        as u64;
    parts
        .iter()
        .map(|values| super::graph::spark(values, max))
        .collect()
}

fn draw_peers(frame: &mut Frame, app: &App, status: &Status, area: Rect) {
    let wide = area.width >= WIDE;
    let load_width: u16 = if wide {
        16
    } else if area.width >= LOAD_FROM {
        10
    } else {
        0
    };
    let mut names = vec![
        "PEER", "PORT", "REPLY", "RTT", "PMTU", "MSS", "MACS", "RX b/s", "TX b/s",
    ];
    if load_width > 0 {
        names.push("LOAD");
    }
    names.push("CONFIG");
    if wide {
        names.extend(["VLANS", "TRUNK"]);
    }
    let header = Row::new(names).style(theme::header());
    let mut loads = if load_width > 0 {
        loads(app, status, usize::from(load_width))
    } else {
        Vec::new()
    }
    .into_iter();
    let rows: Vec<Row> = if status.peers.is_empty() {
        vec![Row::new(vec![Cell::from("no direct peers")]).style(theme::dim())]
    } else {
        status
            .peers
            .iter()
            .map(|p| peer_row(p, wide, loads.next()))
            .collect()
    };
    let mut widths = vec![
        Constraint::Length(12),
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Length(6),
    ];
    if load_width > 0 {
        widths.push(Constraint::Length(load_width));
    }
    widths.push(if wide {
        Constraint::Length(12)
    } else {
        Constraint::Min(7)
    });
    if wide {
        widths.extend([Constraint::Length(14), Constraint::Min(8)]);
    }
    let table = Table::new(rows, widths)
        .header(header)
        .block(titled(" Direct peers (live) "))
        .row_highlight_style(theme::selected());
    let selected = (app.overview_selected < status.peers.len()).then_some(app.overview_selected);
    list::show(
        frame,
        table,
        area,
        &app.scrolls.peers,
        selected,
        status.peers.len(),
        list::BOXED,
    );
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
    match &site.via {
        Some(hop) => (format!("via {hop}"), theme::dim()),
        None => ("-".into(), theme::dim()),
    }
}

fn draw_sites(frame: &mut Frame, app: &App, status: &Status, area: Rect) {
    let header = Row::new(vec!["SITE", "ADDRESS", "VLANS", "ROLE", "LINK"]).style(theme::header());
    let pairs: Vec<(&str, Option<&str>)> = status
        .sites
        .iter()
        .map(|s| (s.name.as_str(), s.uplink.as_deref()))
        .collect();
    // Widths from the texts; the VLANs get the rest, "…" where cut.
    let entries: Vec<(Vec<String>, Style)> = topology::tree_order_by(&pairs)
        .into_iter()
        .filter_map(|(i, depth)| status.sites.get(i).map(|s| (s, depth)))
        .map(|(site, depth)| {
            let (link, style) = link(status, site);
            (
                vec![
                    topology::tree_name(&site.name, depth),
                    site.address.clone(),
                    site.vlans.clone(),
                    format::role(
                        site.uplink.as_deref(),
                        format::serves(&status.sites, &site.name),
                    ),
                    link,
                ],
                style,
            )
        })
        .collect();
    let headers = ["SITE", "ADDRESS", "VLANS", "ROLE", "LINK"];
    let texts: Vec<Vec<String>> = entries.iter().map(|e| e.0.clone()).collect();
    let widths = list::fit_widths(
        area.width.saturating_sub(3),
        &headers,
        &texts,
        &[16, 15, 40, 30, 16],
        2,
    );
    let rows: Vec<Row> = entries
        .into_iter()
        .map(|(texts, style)| {
            let cell = |c: usize| list::fit(&texts[c], widths[c]);
            Row::new(vec![
                Cell::from(cell(0)),
                Cell::from(cell(1)),
                Cell::from(cell(2)),
                Cell::from(cell(3)),
                Cell::from(cell(4)).style(style),
            ])
        })
        .collect();
    let widths: Vec<Constraint> = widths.into_iter().map(Constraint::Length).collect();
    let title = format!(" Network: {} sites (Enter: show) ", status.sites.len());
    let total = rows.len();
    let selected = app
        .overview_selected
        .checked_sub(status.peers.len())
        .filter(|i| *i < total);
    let table = Table::new(rows, widths)
        .header(header)
        .block(titled(title))
        .row_highlight_style(theme::selected());
    list::show(
        frame,
        table,
        area,
        &app.scrolls.network,
        selected,
        total,
        list::BOXED,
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

    #[test]
    fn sites_without_a_tunnel_show_the_next_hop() {
        let mut status = fixtures::status();
        let mut far = status.sites[1].clone();
        far.name = "far".into();
        far.direct = false;
        far.uplink = Some("buero".into());
        far.via = Some("buero".into());
        status.sites.push(far.clone());
        assert_eq!(link(&status, &far).0, "via buero");
        assert_eq!(
            format::role(far.uplink.as_deref(), false),
            "client of buero"
        );
        // A client with clients of its own is a server for them.
        let mut below = far.clone();
        below.name = "below".into();
        below.uplink = Some("far".into());
        status.sites.push(below);
        assert!(format::serves(&status.sites, "far"));
        assert_eq!(
            format::role(far.uplink.as_deref(), true),
            "client of buero + server"
        );
    }
}
