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
use wanderlan_core::{policy, topology};

use super::{health_style, list, port_state, titled};
use crate::tui::app::{App, HISTORY_LEN};
use crate::tui::{format, theme};

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
pub fn peer(frame: &mut Frame, app: &App, name: &str, area: Rect) {
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
        " {} ({}) via {}   Esc: close ",
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
    frame.render_widget(chart, graph);

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rtts: Vec<u64> = samples
        .iter()
        .map(|s| s.rtt_ms.map_or(0, |r| (r * 100.0) as u64))
        .collect();
    let [rtt_label, rtt_spark] =
        Layout::horizontal([Constraint::Length(14), Constraint::Min(10)]).areas(rtt);
    frame.render_widget(
        Paragraph::new(vec![Line::raw(" Round trip"), Line::raw(" history")]).style(theme::dim()),
        rtt_label,
    );
    frame.render_widget(
        Sparkline::default().data(&rtts).style(theme::title()),
        rtt_spark,
    );
}

/// Everything about one site of the draft.
pub fn site(frame: &mut Frame, app: &App, name: &str, area: Rect) {
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
        .map(|m| format!("{} → {}", m.local, m.shared))
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
    let title = format!(" {name}   Up/Down: next site   e: edit   Esc: close ");
    let block = titled(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let total = lines.len();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    list::scrollbar(frame, area, 1, 1, total, 0, usize::from(inner.height));
}

/// The open invitations with their rules.
pub fn invites(frame: &mut Frame, app: &App, selected: usize, area: Rect) {
    let rows = app.invitations.len().max(1);
    let height = u16::try_from(rows + 6).unwrap_or(u16::MAX);
    let popup = area.centered(Constraint::Max(80), Constraint::Max(height));
    frame.render_widget(Clear, popup);
    let block = titled(" Open invitations ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [rows_area, command] =
        Layout::vertical([Constraint::Min(2), Constraint::Length(2)]).areas(inner);
    let table_rows: Vec<Row> = if app.invitations.is_empty() {
        vec![Row::new(vec![Cell::from("no open invitations")]).style(theme::dim())]
    } else {
        app.invitations
            .iter()
            .map(|inv| {
                let state = if inv.in_use {
                    " (a site is joining)"
                } else {
                    ""
                };
                Row::new(vec![
                    Cell::from(inv.code.clone()),
                    Cell::from(format!("{}{state}", inv.rules.summary())),
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
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(" On the new site run:", theme::dim()),
            Line::styled(format!(" {cmd}"), theme::bold()),
        ]),
        command,
    );
}
