//! Popups with details: the traffic and round trips of one peer, and the
//! open invitations of this site.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Cell, Chart, Clear, Dataset, GraphType, Paragraph, Row, Sparkline, Table, TableState,
};
use wanderlan_core::api::{PeerStatus, Traffic};

use super::{health_style, port_state, titled};
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
    let [facts, graph, rtt] = Layout::vertical([
        Constraint::Length(8),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .areas(inner);

    let config = match (peer.revision, peer.in_sync) {
        (Some(r), Some(true)) => format!("revision {r}, same as here"),
        (Some(r), Some(false)) => format!("revision {r}, DIFFERS"),
        (Some(r), None) => format!("revision {r}"),
        (None, _) => "-".into(),
    };
    let lines = vec![
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

/// The open invitations with their rules.
pub fn invites(frame: &mut Frame, app: &App, selected: usize, area: Rect) {
    let rows = app.invitations.len().max(1);
    let height = u16::try_from(rows + 7).unwrap_or(u16::MAX);
    let popup = area.centered(Constraint::Max(80), Constraint::Max(height));
    frame.render_widget(Clear, popup);
    let block = titled(" Open invitations ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [list, command, keys] = Layout::vertical([
        Constraint::Min(2),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(inner);
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
    let mut state = TableState::default();
    if !app.invitations.is_empty() {
        state.select(Some(selected));
    }
    frame.render_stateful_widget(
        Table::new(table_rows, [Constraint::Length(12), Constraint::Min(20)])
            .header(Row::new(vec!["CODE", "RULES FOR THE NEW SITE"]).style(theme::header()))
            .row_highlight_style(theme::selected()),
        list,
        &mut state,
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
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" n ", theme::key()),
            Span::raw(" new  "),
            Span::styled(" d ", theme::key()),
            Span::raw(" delete  "),
            Span::styled(" Esc ", theme::key()),
            Span::raw(" close"),
        ])),
        keys,
    );
}
