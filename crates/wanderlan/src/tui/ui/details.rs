//! Popups with details: the traffic and round trips of one peer, and the
//! open invitations of this site.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Cell, Chart, Clear, Dataset, GraphType, Paragraph, Row, Sparkline, Table, Wrap,
};
use wanderlan_core::api::{PeerStatus, Traffic};
use wanderlan_core::config::LocalVid;
use wanderlan_core::{policy, topology};

use super::{health_style, list, port_state, titled};
use crate::tui::app::{App, HISTORY_LEN};
use crate::tui::{format, theme, traffic};

fn label(text: &str) -> Span<'static> {
    Span::styled(format!(" {text:<13}"), theme::dim())
}

fn counters(t: &Traffic, rx: bool) -> String {
    let (bytes, packets, errors, dropped) = if rx {
        (t.rx_bytes, t.rx_packets, t.rx_errors, t.rx_dropped)
    } else {
        (t.tx_bytes, t.tx_packets, t.tx_errors, t.tx_dropped)
    };
    format!(
        "{} in {packets} pkts, {errors} err, {dropped} drop",
        format::bytes(bytes)
    )
}

fn rtt_line(peer: &PeerStatus) -> Line<'static> {
    let Some(s) = peer.rtt_stats else {
        return Line::from(vec![label("Round trip"), Span::raw("no answers yet")]);
    };
    let now = peer.rtt_ms.map_or_else(|| "-".into(), format::rtt);
    Line::from(vec![
        label("Round trip"),
        Span::styled(now, theme::bold()),
        Span::raw(format!(
            "  min {} avg {} max {}  jitter {}",
            format::rtt(s.min_ms),
            format::rtt(s.avg_ms),
            format::rtt(s.max_ms),
            format::rtt(s.jitter_ms)
        )),
    ])
}

fn quality_line(peer: &PeerStatus) -> Line<'static> {
    let (loss, samples) = peer
        .rtt_stats
        .map_or((0.0, 0), |s| (s.loss_percent, s.samples));
    let loss_style = if loss > 0.0 {
        theme::warn()
    } else {
        Style::new()
    };
    let latency = peer
        .rtt_stats
        .map_or_else(|| "-".into(), |s| format::rtt(s.avg_ms / 2.0));
    Line::from(vec![
        label("Probe loss"),
        Span::styled(format!("{loss:.0} %"), loss_style),
        Span::raw(format!(" of {samples} probes   ")),
        Span::styled("latency ", theme::dim()),
        Span::raw(format!("~{latency} one way (round trip / 2)")),
    ])
}

/// Traffic graph and everything known about one peer.
/// The VLANs on the tunnel to `peer` with their live traffic; `vlan` is
/// the chosen one (`None`: the first).
fn peer_vlans(frame: &mut Frame, app: &App, peer: &str, vlan: Option<u16>, area: Rect) {
    let block = titled(" VLANs  Enter: open ");
    let message = |frame: &mut Frame, text: String, style| {
        let inner = block.inner(area);
        frame.render_widget(block.clone(), area);
        let lines: Vec<Line> = format::wrap(&text, usize::from(inner.width.saturating_sub(1)))
            .into_iter()
            .map(|l| Line::styled(format!(" {l}"), style))
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    };
    let traffic = match (&app.vlan_traffic, &app.vlan_traffic_error) {
        (_, Some(e)) => return message(frame, format!("Cannot read: {e}"), theme::bad()),
        (None, None) => return message(frame, "Loading ...".into(), theme::dim()),
        (Some(t), _) if !t.available => {
            let why = t.note.clone().unwrap_or_default();
            return message(frame, format!("No traffic per VLAN: {why}."), theme::dim());
        }
        (Some(t), None) => t,
    };
    let list = traffic::peer_vlans(traffic, peer);
    if list.is_empty() {
        return message(frame, "No VLAN on this tunnel.".into(), theme::dim());
    }
    let selected = vlan
        .and_then(|v| list.iter().position(|p| p.vid == v))
        .unwrap_or_default();
    let rows: Vec<Row> = list
        .iter()
        .map(|p| {
            let idle = p.rx_bps == 0 && p.tx_bps == 0;
            Row::new(vec![
                Cell::from(p.vid.to_string()),
                Cell::from(format::rate(p.rx_bps)),
                Cell::from(format::rate(p.tx_bps)),
            ])
            .style(if idle { theme::dim() } else { Style::new() })
        })
        .collect();
    let total = rows.len();
    list::show(
        frame,
        Table::new(
            rows,
            [
                Constraint::Length(5),
                Constraint::Length(8),
                Constraint::Length(8),
            ],
        )
        .header(Row::new(vec!["VLAN", "IN b/s", "OUT b/s"]).style(theme::header()))
        .block(block)
        .row_highlight_style(theme::selected()),
        area,
        &app.scrolls.peer_vlans,
        Some(selected),
        total,
        list::BOXED,
    );
}

/// The peer's identity.
fn identity_line(peer: &PeerStatus) -> Line<'static> {
    use wanderlan_core::api::IdentityState;
    let id = peer.identity.clone().unwrap_or_else(|| "none".into());
    let (state, style) = match peer.identity_state {
        Some(IdentityState::Confirmed) => ("confirmed", theme::ok()),
        Some(IdentityState::Waiting) => ("not confirmed yet", theme::warn()),
        Some(IdentityState::WrongKey) => ("WRONG KEY at its address", theme::bad()),
        None => ("no key (older version)", theme::dim()),
    };
    Line::from(vec![
        label("Identity"),
        Span::raw(format!("{id}   ")),
        Span::styled(state, style),
    ])
}

/// How the tunnel runs (found out by the daemons) and why, its VNI and
/// where a client's packets come from.
fn vxlan_line(peer: &PeerStatus) -> Line<'static> {
    let reason = peer
        .tunnel_reason
        .split_once(": ")
        .map_or("", |(_, why)| why)
        .to_string();
    let mut spans = vec![
        label("VXLAN"),
        Span::styled(peer.tunnel.to_string(), theme::ok()),
    ];
    if !reason.is_empty() {
        spans.push(Span::raw(format!(" ({reason})")));
    }
    let mut extra = Vec::new();
    if let Some(vni) = peer.vni {
        extra.push(format!("VNI {vni}"));
    }
    if let Some(at) = &peer.endpoint {
        extra.push(format!("from {at}"));
    }
    if !extra.is_empty() {
        spans.push(Span::styled(
            format!("   {}", extra.join("   ")),
            theme::dim(),
        ));
    }
    Line::from(spans)
}

pub fn peer(frame: &mut Frame, app: &App, name: &str, vlan: Option<u16>, area: Rect) {
    frame.render_widget(Clear, area);
    let Some(peer) = app
        .status
        .as_ref()
        .and_then(|s| s.peers.iter().find(|p| p.name == name))
    else {
        frame.render_widget(
            Paragraph::new(format!(" {name} is no longer a peer. Esc closes."))
                .block(titled(" Peer ")),
            area,
        );
        return;
    };
    let block = titled(format!(
        " {} ({}) via {}   Left/Right: other peer  Esc: close ",
        peer.name, peer.address, peer.port
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let config = match (&peer.refused, peer.revision, peer.in_sync) {
        (Some(_), _, _) => "change REFUSED".to_string(),
        (None, Some(r), Some(true)) => format!("revision {r}, same as here"),
        (None, Some(r), Some(false)) => format!("revision {r}, DIFFERS"),
        (None, Some(r), None) => format!("revision {r}"),
        (None, None, _) => "-".into(),
    };
    let limited = app.running.as_ref().is_some_and(|r| {
        r.network
            .as_ref()
            .zip(r.local.self_name.as_deref())
            .is_some_and(|(network, me)| network.tunnel_limit(me, &peer.name).is_some())
    });
    let mut lines = vec![
        Line::from(vec![
            label("Tunnel"),
            Span::styled(port_state(peer.port_state).to_string(), health_style(peer)),
            Span::raw(format!(
                "   {}   config {config}",
                if peer.reachable {
                    "reachable"
                } else {
                    "NOT reachable"
                }
            )),
        ]),
        rtt_line(peer),
        quality_line(peer),
        identity_line(peer),
        vxlan_line(peer),
        Line::from(vec![
            label("Path MTU"),
            Span::raw(format!(
                "{}   TCP MSS {}   MACs behind it {}",
                peer.pmtu.map_or_else(|| "-".into(), |v| v.to_string()),
                peer.mss.map_or_else(|| "-".into(), |v| v.to_string()),
                peer.macs
            )),
        ]),
        Line::from(vec![
            label("VLANs"),
            Span::raw(peer.vlans.clone()),
            Span::styled(
                if limited {
                    " (limited on this tunnel)"
                } else {
                    ""
                },
                theme::dim(),
            ),
            Span::styled("   trunk ", theme::dim()),
            Span::raw(if peer.trunk.is_empty() {
                "-".to_string()
            } else {
                peer.trunk.clone()
            }),
        ]),
        Line::from(vec![
            label("Received"),
            Span::styled(format!("{}bit/s", format::rate(peer.rx_bps)), theme::ok()),
            Span::raw(format!(" now, {}", counters(&peer.traffic, true))),
        ]),
        Line::from(vec![
            label("Sent"),
            Span::styled(format!("{}bit/s", format::rate(peer.tx_bps)), theme::warn()),
            Span::raw(format!(" now, {}", counters(&peer.traffic, false))),
        ]),
    ];
    if let Some(reason) = &peer.down_reason {
        lines.insert(
            1,
            Line::from(vec![
                label("Down"),
                Span::styled(reason.clone(), theme::warn()),
            ]),
        );
    }
    if let Some(reason) = &peer.refused {
        lines.push(Line::from(vec![
            label("Refused"),
            Span::styled(reason.clone(), theme::bad()),
        ]));
    }
    // As many fact lines as there are, the graph gets the rest.
    let [facts, graph, rtt] = Layout::vertical([
        Constraint::Length(u16::try_from(lines.len() + 1).unwrap_or(9)),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(lines), facts);

    let samples: Vec<_> = app
        .history
        .get(name)
        .map(|h| h.iter().copied().collect())
        .unwrap_or_default();
    #[allow(clippy::cast_precision_loss)]
    let points = |pick: fn(&crate::tui::app::Sample) -> u64| -> Vec<(f64, f64)> {
        let offset = HISTORY_LEN.saturating_sub(samples.len());
        samples
            .iter()
            .enumerate()
            .map(|(i, s)| ((offset + i) as f64, pick(s) as f64))
            .collect()
    };
    let rx = points(|s| s.rx_bps);
    let tx = points(|s| s.tx_bps);
    let top = rx
        .iter()
        .chain(tx.iter())
        .map(|p| p.1)
        .fold(1000.0, f64::max)
        * 1.1;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let top_label = format::rate(top as u64);
    #[allow(clippy::cast_precision_loss)]
    let width = HISTORY_LEN as f64;
    let chart = Chart::new(vec![
        Dataset::default()
            .name("received")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(theme::ok())
            .data(&rx),
        Dataset::default()
            .name("sent")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(theme::warn())
            .data(&tx),
    ])
    .block(titled(" Traffic (bit/s, last samples) "))
    .x_axis(Axis::default().bounds([0.0, width]))
    .y_axis(
        Axis::default()
            .bounds([0.0, top])
            .labels(vec![Span::raw("0"), Span::raw(top_label)]),
    );
    let [graph, vlans] =
        Layout::horizontal([Constraint::Min(30), Constraint::Length(28)]).areas(graph);
    frame.render_widget(chart, graph);
    peer_vlans(frame, app, name, vlan, vlans);

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rtts: Vec<u64> = samples
        .iter()
        .map(|s| s.rtt_ms.map_or(0, |r| (r * 100.0) as u64))
        .collect();
    let [rtt_label, rtt_spark] =
        Layout::horizontal([Constraint::Length(14), Constraint::Min(10)]).areas(rtt);
    // The newest samples, as many as fit (the oldest drop off the left).
    let rtts = &rtts[rtts.len().saturating_sub(usize::from(rtt_spark.width))..];
    frame.render_widget(
        Paragraph::new(vec![Line::raw(" Round trip"), Line::raw(" history")]).style(theme::dim()),
        rtt_label,
    );
    frame.render_widget(
        Sparkline::default().data(rtts).style(theme::title()),
        rtt_spark,
    );
}

/// Everything about one site of the draft.
pub fn site(frame: &mut Frame, app: &App, name: &str, scroll: u16, area: Rect) {
    frame.render_widget(Clear, area);
    let Some((network, site)) = app
        .draft
        .as_ref()
        .and_then(|d| d.network.site(name).map(|s| (&d.network, s)))
    else {
        frame.render_widget(
            Paragraph::new(format!(" {name} is not in the draft. Esc closes."))
                .block(titled(" Site ")),
            area,
        );
        return;
    };
    let me = app
        .draft
        .as_ref()
        .and_then(|d| d.local.self_name.clone())
        .unwrap_or_default();
    let dim = |t: &str| Span::styled(t.to_string(), theme::dim());
    let vlans_text = |v: &wanderlan_core::config::VlanSet| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.to_string()
        }
    };
    let mut lines = vec![
        Line::from(vec![
            label("Role"),
            Span::raw(format::role(site.uplink.as_deref())),
            if site.name == me {
                dim("   this site")
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            label("Address"),
            Span::raw(site.address.to_string()),
            if site.online {
                dim("   online")
            } else {
                Span::styled("   OFFLINE", theme::warn())
            },
        ]),
        Line::from(vec![
            label("Identity"),
            Span::raw(
                site.id
                    .map_or_else(|| "none yet (older version)".into(), |id| id.to_string()),
            ),
        ]),
        Line::from(vec![
            label("VLANs"),
            Span::raw(network.vlans_of(site).to_string()),
            dim("   own VLANs "),
            Span::raw(
                site.own_vlans
                    .as_ref()
                    .map_or_else(|| "none".into(), ToString::to_string),
            ),
        ]),
        Line::from(vec![
            label("Changes"),
            Span::raw(policy::managers(network, site)),
        ]),
        Line::from(vec![
            label("Renamed by"),
            Span::raw(policy::renamers(network, site)),
        ]),
    ];
    let clients: Vec<&str> = network
        .sites
        .iter()
        .filter(|s| s.uplink.as_deref() == Some(name))
        .map(|s| s.name.as_str())
        .collect();
    if !clients.is_empty() {
        lines.push(Line::from(vec![
            label("Clients"),
            Span::raw(clients.join(", ")),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(" Tunnels", theme::header()));
    let tunnels = topology::tunnels_for(network, name);
    if tunnels.is_empty() {
        lines.push(Line::styled("   none", theme::dim()));
    }
    for t in &tunnels {
        let kind = match t.kind {
            topology::TunnelKind::Server => "server",
            topology::TunnelKind::Uplink => "its server",
            topology::TunnelKind::Client => "its client",
        };
        let carries = topology::tunnel_vlans(network, name, &t.peer.name).unwrap_or_default();
        let mut spans = vec![
            Span::raw(format!("   to {:<13}", t.peer.name)),
            Span::styled(format!("{kind:<12}"), theme::dim()),
            Span::raw(format!("carries {}", vlans_text(&carries))),
        ];
        if let Some(limit) = network.tunnel_limit(name, &t.peer.name) {
            spans.push(dim(&format!("  (limited to {})", vlans_text(limit))));
        }
        lines.push(Line::from(spans));
    }
    let maps: Vec<String> = network
        .maps_of(name)
        .map(|m| match m.local {
            LocalVid::Native => format!("source VLAN {} = untagged", m.shared),
            LocalVid::Vid(v) => format!("source VLAN {} = VLAN {v}", m.shared),
        })
        .collect();
    if !maps.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            label("Mappings"),
            Span::raw(maps.join(", ")),
        ]));
    }
    // Which ports of this host carry the tunnel to the site.
    if let Some(ports) = app
        .draft
        .as_ref()
        .and_then(|d| d.local.tunnel_trunks.get(name))
    {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            label("Trunk here"),
            Span::raw(ports.join(", ")),
            dim("   a segment of its own on this host (Settings, 7)"),
        ]));
    }
    let title = format!(" {name}   Up/Down: next site   PgUp/PgDn: scroll   e: edit   Esc: close ");
    let block = titled(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Wrapped lines count: the scroll ends with the last line in view.
    let width = usize::from(inner.width.max(1));
    let total: usize = lines.iter().map(|l| l.width().max(1).div_ceil(width)).sum();
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max = total.saturating_sub(usize::from(inner.height));
    app.scrolls
        .site_max
        .set(u16::try_from(max).unwrap_or(u16::MAX));
    let scroll = scroll.min(u16::try_from(max).unwrap_or(u16::MAX));
    frame.render_widget(paragraph.scroll((scroll, 0)), inner);
    list::scrollbar(
        frame,
        area,
        1,
        1,
        total,
        usize::from(scroll),
        usize::from(inner.height),
    );
}

/// The open invitations with their rules.
pub fn invites(frame: &mut Frame, app: &App, selected: usize, area: Rect) {
    let rows = app.invitations.len().max(1);
    // The selected invitation in full below the list (a row is cut at 80
    // columns): its rules, wrapped, and the command for the new site.
    let width = usize::from(area.width.min(100).saturating_sub(4)).max(20);
    let rules = app
        .invitations
        .get(selected)
        .map(|inv| format::wrap(&format!("Rules: {}", inv.rules.summary()), width))
        .unwrap_or_default();
    let below = rules.len() + 2;
    let height = u16::try_from(rows + 4 + below).unwrap_or(u16::MAX);
    // Wider than the other windows: the rules of an invitation can be long.
    let popup = area.centered(Constraint::Max(100), Constraint::Max(height));
    frame.render_widget(Clear, popup);
    let block = titled(" Open invitations ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [rows_area, command] = Layout::vertical([
        Constraint::Min(2),
        Constraint::Length(u16::try_from(below).unwrap_or(u16::MAX)),
    ])
    .areas(inner);
    let table_rows: Vec<Row> = if app.invitations.is_empty() {
        vec![Row::new(vec![Cell::from("no open invitations")]).style(theme::dim())]
    } else {
        app.invitations
            .iter()
            .map(|inv| {
                // The state first: the rules may be cut off.
                let state = if inv.in_use { "JOINING  " } else { "" };
                Row::new(vec![
                    Cell::from(inv.code.clone()),
                    Cell::from(format!("{state}{}", inv.rules.summary())),
                ])
            })
            .collect()
    };
    let total = table_rows.len();
    list::show(
        frame,
        Table::new(table_rows, [Constraint::Length(12), Constraint::Min(20)])
            .header(Row::new(vec!["CODE", "RULES FOR THE NEW SITE"]).style(theme::header()))
            .row_highlight_style(theme::selected()),
        rows_area,
        &app.scrolls.invites,
        (!app.invitations.is_empty()).then_some(selected),
        total,
        list::Chrome {
            bordered: false,
            header: true,
        },
    );
    let cmd = app
        .invitations
        .get(selected)
        .map(|inv| inv.command.clone())
        .unwrap_or_default();
    let mut lines: Vec<Line> = rules
        .into_iter()
        .map(|l| Line::raw(format!(" {l}")))
        .collect();
    lines.push(Line::styled(" On the new site run:", theme::dim()));
    lines.push(Line::styled(format!(" {cmd}"), theme::bold()));
    frame.render_widget(Paragraph::new(lines), command);
}
