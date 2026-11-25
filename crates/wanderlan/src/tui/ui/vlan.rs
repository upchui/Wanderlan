//! The window of one VLAN: its live traffic on every port of this site, the
//! MAC addresses behind them, and what the configuration says about it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Cell, Chart, Clear, Dataset, GraphType, Paragraph, Row, Table};
use wanderlan_core::config::{LocalVid, NATIVE_VID, Network};
use wanderlan_core::{forwarding, topology};

use super::{list, titled};
use crate::tui::app::{App, HISTORY_LEN};
use crate::tui::{format, theme, traffic};

fn label(text: &str) -> Span<'static> {
    Span::styled(format!(" {text:<12}"), theme::dim())
}

/// What the configuration says about `vid` at site `me`: on the trunk (and
/// how), the tunnels that carry it, where it is passed on.
fn facts(network: &Network, me: &str, vid: u16) -> Vec<Line<'static>> {
    let Some(site) = network.site(me) else {
        return Vec::new();
    };
    let on_trunk =
        network.effective_vlans(site).contains(vid) || network.own_vlans(site).contains(vid);
    let mapped: Vec<String> = network
        .maps
        .iter()
        .filter(|m| m.site == me && m.shared == vid)
        .map(|m| match m.local {
            LocalVid::Native => "untagged".to_string(),
            LocalVid::Vid(v) => format!("as local VLAN {v}"),
        })
        .collect();
    let here = match (on_trunk, mapped.as_slice()) {
        (false, _) => "not on the trunk of this site".to_string(),
        (true, []) if vid == NATIVE_VID => "on the trunk untagged (native)".to_string(),
        (true, []) => "on the trunk, tagged".to_string(),
        (true, ways) => format!("on the trunk {} (mapped)", ways.join(", ")),
    };
    let carried: Vec<String> = topology::tunnels_for(network, me)
        .iter()
        .filter(|t| {
            topology::tunnel_vlans(network, me, &t.peer.name).is_some_and(|v| v.contains(vid))
        })
        .map(|t| t.peer.name.clone())
        .collect();
    let mut lines = vec![
        Line::from(vec![label("Here"), Span::raw(here)]),
        Line::from(vec![
            label("Tunnels"),
            Span::raw(if carried.is_empty() {
                "none carries it".to_string()
            } else {
                carried.join(", ")
            }),
        ]),
    ];
    let groups: Vec<String> = forwarding::forwarding(network, me)
        .passes
        .iter()
        .filter(|(vlans, _)| vlans.contains(vid))
        .flat_map(|(_, groups)| groups.iter())
        .map(|g| g.iter().cloned().collect::<Vec<_>>().join(", "))
        .collect();
    if !groups.is_empty() {
        lines.push(Line::from(vec![
            label("Passed on"),
            Span::raw(format!("here between {}", groups.join(" | "))),
        ]));
    }
    lines
}

/// Draws the window over `area`.
pub fn draw(frame: &mut Frame, app: &App, vid: u16, back: Option<&str>, area: Rect) {
    frame.render_widget(Clear, area);
    let esc = if back.is_some() { "back" } else { "close" };
    let block = titled(format!(
        " VLAN {vid} live   Up/Down: other VLAN  m: MACs  Esc: {esc} "
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = app
        .running
        .as_ref()
        .and_then(|r| r.network.as_ref().zip(r.local.self_name.as_deref()))
        .map(|(network, me)| facts(network, me, vid))
        .unwrap_or_default();
    let ports = app
        .vlan_traffic
        .as_ref()
        .map(|t| traffic::vlan_ports(t, vid))
        .unwrap_or_default();
    match (&app.vlan_traffic, &app.vlan_traffic_error) {
        (_, Some(e)) => lines.push(Line::from(vec![
            label("Traffic"),
            Span::styled(format!("cannot be read: {e}"), theme::bad()),
        ])),
        (None, None) => lines.push(Line::from(vec![
            label("Traffic"),
            Span::styled("loading ...", theme::dim()),
        ])),
        (Some(t), None) if !t.available => lines.push(Line::from(vec![
            label("Traffic"),
            Span::styled(
                format!(
                    "not counted per VLAN: {}",
                    t.note.as_deref().unwrap_or_default()
                ),
                theme::warn(),
            ),
        ])),
        (Some(_), None) if ports.is_empty() => lines.push(Line::from(vec![
            label("Traffic"),
            Span::styled("no port of this site has this VLAN", theme::dim()),
        ])),
        (Some(_), None) => {}
    }
    let fact_height = u16::try_from(lines.len()).unwrap_or(4);
    let table_height = u16::try_from(ports.len().max(1) + 3).unwrap_or(u16::MAX);
    let [facts_area, table_area, graph_area] = Layout::vertical([
        Constraint::Length(fact_height + 1),
        Constraint::Max(table_height),
        Constraint::Min(5),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(lines), facts_area);

    let macs = |port: &str| {
        app.macs
            .entries
            .iter()
            .filter(|m| m.vlan == Some(vid) && m.port == port)
            .count()
    };
    let rows: Vec<Row> = ports
        .iter()
        .map(|p| {
            let name = match &p.peer {
                Some(_) => p.port.clone(),
                None => format!("{} (trunk)", p.port),
            };
            let idle = p.rx_bps == 0 && p.tx_bps == 0;
            Row::new(vec![
                Cell::from(name),
                Cell::from(format::rate(p.rx_bps)),
                Cell::from(format::rate(p.tx_bps)),
                Cell::from(format::bytes(p.rx_bytes)),
                Cell::from(format::bytes(p.tx_bytes)),
                Cell::from(format!("{} / {}", p.rx_packets, p.tx_packets)),
                Cell::from(macs(&p.port).to_string()),
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
                Constraint::Length(16),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Length(9),
                Constraint::Min(13),
                Constraint::Length(4),
            ],
        )
        .header(
            Row::new(vec![
                "PORT",
                "IN b/s",
                "OUT b/s",
                "RECEIVED",
                "SENT",
                "PACKETS IN/OUT",
                "MACS",
            ])
            .style(theme::header()),
        )
        .block(titled(" Ports (in: from the port into the bridge) ")),
        table_area,
        &app.scrolls.vlan_ports,
        None,
        total,
        list::BOXED,
    );

    let samples: Vec<_> = app
        .vlan_history
        .get(&vid)
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
            .name("in")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(theme::ok())
            .data(&rx),
        Dataset::default()
            .name("out")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(theme::warn())
            .data(&tx),
    ])
    .block(titled(" All ports (bit/s, last samples) "))
    .x_axis(Axis::default().bounds([0.0, width]))
    .y_axis(
        Axis::default()
            .bounds([0.0, top])
            .labels(vec![Span::raw("0"), Span::raw(top_label)]),
    );
    frame.render_widget(chart, graph_area);
}
