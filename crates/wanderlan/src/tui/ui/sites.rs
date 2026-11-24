//! View 2: the sites of the draft, the network settings and the local settings.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use wanderlan_core::config::{FieldPath, Severity};
use wanderlan_core::{policy, topology};

use super::{diagnostic_line, health_style, not_configured, port_state, severity_style, titled};
use crate::tui::app::App;
use crate::tui::draft::{Draft, is_within};
use crate::tui::{format, theme};

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let Some(draft) = &app.draft else {
        not_configured(frame, area);
        return;
    };
    let [table, detail, settings] = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(3),
        Constraint::Length(7),
    ])
    .areas(area);
    draw_table(frame, app, draft, table);
    frame.render_widget(Paragraph::new(detail_lines(app, draft)), detail);
    let [network, local] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(settings);
    draw_network(frame, draft, network);
    draw_local(frame, draft, local);
}

fn draw_table(frame: &mut Frame, app: &App, draft: &Draft, area: Rect) {
    let running = app.running.as_ref().and_then(|r| r.network.as_ref());
    let own = draft.local.self_name.as_deref();
    let header = Row::new(vec![
        "",
        "NAME",
        "ADDRESS",
        "VLANS",
        "ROLE",
        "MANAGED BY",
        "TUNNEL",
    ])
    .style(theme::header());
    let order = topology::tree_order(&draft.network);
    let rows: Vec<Row> = order
        .iter()
        .filter_map(|(i, depth)| draft.network.sites.get(*i).map(|s| (*i, *depth, s)))
        .map(|(i, depth, site)| {
            let marker = match running.and_then(|m| m.site(&site.name)) {
                None => Span::styled("+", theme::ok()),
                Some(old) if old != site => Span::styled("*", theme::warn()),
                Some(_) => Span::raw(" "),
            };
            let (tunnel, tunnel_style) = if !site.online {
                ("offline".to_string(), theme::warn())
            } else if Some(site.name.as_str()) == own {
                ("this site".to_string(), theme::bold())
            } else {
                match app.peer(&site.name) {
                    Some(peer) => (port_state(peer.port_state).to_string(), health_style(peer)),
                    None => {
                        match own.and_then(|me| topology::via(&draft.network, me, &site.name)) {
                            Some(hop) => (format!("via {hop}"), theme::dim()),
                            None => ("-".to_string(), theme::dim()),
                        }
                    }
                }
            };
            let style = severity_style(draft.worst_within(&FieldPath::site(i)));
            Row::new(vec![
                Cell::from(marker),
                Cell::from(topology::tree_name(&site.name, depth)),
                Cell::from(site.address.to_string()),
                {
                    let own = site
                        .own_vlans
                        .as_ref()
                        .map_or_else(String::new, |o| format!(" +{o}"));
                    match &site.vlans {
                        Some(v) => Cell::from(format!("{v}{own}")),
                        None => Cell::from(Span::styled(
                            format!("{}{own}", draft.network.vlans),
                            theme::dim(),
                        )),
                    }
                },
                Cell::from(format::role(site.uplink.as_deref())),
                Cell::from(policy::managed_label(&draft.network, site)),
                Cell::from(tunnel).style(tunnel_style),
            ])
            .style(style)
        })
        .collect();
    let removed = running.map_or(0, |m| {
        m.sites
            .iter()
            .filter(|s| draft.network.site(&s.name).is_none())
            .count()
    });
    let mut title = format!(" Sites (draft): {} ", draft.network.sites.len());
    if removed > 0 {
        title.push_str(&format!("- {removed} removed "));
    }
    let widths = [
        Constraint::Length(1),
        Constraint::Length(12),
        Constraint::Length(15),
        Constraint::Length(9),
        Constraint::Length(15),
        Constraint::Length(10),
        Constraint::Min(9),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(titled(title))
        .row_highlight_style(theme::selected());
    let row = order
        .iter()
        .position(|(i, _)| *i == app.sites_selected)
        .unwrap_or_default();
    let mut state = TableState::default().with_selected(Some(row));
    frame.render_stateful_widget(table, area, &mut state);
}

/// Problems of the selected site, then other problems of the draft.
fn detail_lines(app: &App, draft: &Draft) -> Vec<Line<'static>> {
    let selected = FieldPath::site(app.sites_selected);
    let mut lines: Vec<Line> = draft.within(&selected).map(diagnostic_line).collect();
    let others: Vec<_> = draft
        .diagnostics
        .iter()
        .filter(|d| d.is_error() && !is_within(&d.path, &selected))
        .collect();
    if let Some(first) = others.first() {
        let more = if others.len() > 1 {
            format!(" (+{} more)", others.len() - 1)
        } else {
            String::new()
        };
        let mut line = diagnostic_line(first);
        line.push_span(Span::styled(more, theme::dim()));
        lines.push(line);
    }
    if let Some(site) = draft.network.sites.get(app.sites_selected) {
        let text = format!(
            " {}: {}; VLANs and online state changed by {} (Enter: edit).",
            site.name,
            format::role(site.uplink.as_deref()),
            policy::managed_label(&draft.network, site)
        );
        lines.push(Line::styled(text, theme::dim()));
        if let Some(trunk) = draft.local.tunnel_trunks.get(&site.name) {
            lines.push(Line::styled(
                format!(
                    " The tunnel to {} has its own trunk {trunk} here (Settings, 7).",
                    site.name
                ),
                theme::dim(),
            ));
        }
    }
    if lines.is_empty() {
        let marks = "+ new, * changed (not applied yet). a: review and apply the draft.";
        lines.push(Line::styled(format!(" {marks}"), theme::dim()));
    }
    lines.truncate(3);
    lines
}

fn setting(label: &str, value: String, severity: Option<Severity>) -> Line<'static> {
    let style = match severity {
        Some(s) => theme::severity(s),
        None => Style::new(),
    };
    Line::from(vec![
        Span::styled(format!(" {label:<16}"), theme::dim()),
        Span::styled(value, style),
    ])
}

fn draw_network(frame: &mut Frame, draft: &Draft, area: Rect) {
    let m = &draft.network;
    let at = |key: &str| draft.worst_at(&FieldPath::network(key));
    let lines = vec![
        setting(
            "Default VLANs",
            format!("{} ({} VLANs)", m.vlans, m.vlans.len()),
            at("vlans"),
        ),
        setting(
            "Control port",
            m.control_port.to_string(),
            at("control_port"),
        ),
        setting(
            "MTU probe every",
            format::duration(m.mtu_probe_interval),
            at("mtu_probe_interval"),
        ),
        setting(
            "Commit timeout",
            format::duration(m.commit_timeout),
            at("commit_timeout"),
        ),
        setting(
            "MSS",
            m.mss
                .map_or_else(|| "from path MTU".to_string(), |v| format!("fixed {v}")),
            at("mss"),
        ),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(titled(" Network settings (m) ")),
        area,
    );
}

fn draw_local(frame: &mut Frame, draft: &Draft, area: Rect) {
    let local = &draft.local;
    let at = |key: &str| draft.worst_at(&FieldPath::local(key));
    let mut lines = vec![
        setting(
            "This site",
            local.self_name.clone().unwrap_or_else(|| "-".into()),
            at("self"),
        ),
        setting(
            "Trunk",
            local.trunk.clone().unwrap_or_else(|| "-".into()),
            at("trunk"),
        ),
    ];
    for key in ["self", "trunk"] {
        lines.extend(draft.at(&FieldPath::local(key)).map(diagnostic_line));
    }
    if lines.len() == 2 {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            " Only on this host (local.toml).",
            theme::dim(),
        ));
    }
    frame.render_widget(Paragraph::new(lines).block(titled(" This host (l) ")), area);
}
