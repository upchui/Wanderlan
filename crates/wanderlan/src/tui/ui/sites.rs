//! View 2: the sites of the draft (network and host settings: Settings, 8).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, Wrap};
use wanderlan_core::config::FieldPath;
use wanderlan_core::identity::SiteId;
use wanderlan_core::{policy, topology};

use super::{
    diagnostic_line, health_style, list, not_configured, port_state, severity_style, titled,
};
use crate::tui::app::App;
use crate::tui::draft::{Draft, is_within};
use crate::tui::{format, theme};

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let Some(draft) = &app.draft else {
        not_configured(frame, area);
        return;
    };
    let filter_height = u16::from(app.sites_filter.is_shown());
    let [filter, table, detail] = Layout::vertical([
        Constraint::Length(filter_height),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .areas(area);
    if filter_height > 0 {
        let shown = app.visible_sites().len();
        app.sites_filter
            .draw(frame, filter, shown, draft.network.sites.len());
    }
    draw_table(frame, app, draft, table);
    frame.render_widget(
        Paragraph::new(detail_lines(app, draft)).wrap(Wrap { trim: false }),
        detail,
    );
}

fn draw_table(frame: &mut Frame, app: &App, draft: &Draft, area: Rect) {
    let running = app.running.as_ref().and_then(|r| r.network.as_ref());
    let own = draft.local.self_id;
    let header = Row::new(vec![
        "", "NAME", "ADDRESS", "VLANS", "ROLE", "CHANGES", "TUNNEL",
    ])
    .style(theme::header());
    // A site of an older version renamed (a provisional identity of its
    // new name) is changed, not new; its old entry is not removed.
    let rekeyed = running.map_or_else(Vec::new, |m| {
        wanderlan_core::rename::rekeys(m, &draft.network)
    });
    let before_id = |id: SiteId| -> SiteId {
        rekeyed
            .iter()
            .find(|(_, new)| *new == id)
            .map_or(id, |(old, _)| *old)
    };
    let order = app.visible_sites();
    // The texts first: the columns are as wide as they need, the VLANs get
    // the rest, and a cut text ends with "…" (the hint line has it all).
    let entries: Vec<(usize, Span, Vec<String>, Style, bool, Style)> = order
        .iter()
        .filter_map(|(i, depth)| draft.network.sites.get(*i).map(|s| (*i, *depth, s)))
        .map(|(i, depth, site)| {
            let before = before_id(site.id);
            let marker = match running.and_then(|m| m.site(before)) {
                None => Span::styled("+", theme::ok()),
                Some(old) if old != site => Span::styled("*", theme::warn()),
                Some(_) => Span::raw(" "),
            };
            let (tunnel, tunnel_style) = if !site.online {
                ("offline".to_string(), theme::warn())
            } else if Some(site.id) == own {
                ("this site".to_string(), theme::bold())
            } else {
                match app.peer(&app.running_name(before)) {
                    Some(peer) => (port_state(peer.port_state).to_string(), health_style(peer)),
                    None => match own.and_then(|me| topology::via(&draft.network, me, site.id)) {
                        Some(hop) => (format!("via {}", draft.network.label(hop)), theme::dim()),
                        None => ("-".to_string(), theme::dim()),
                    },
                }
            };
            let own_vlans = site
                .own_vlans
                .as_ref()
                .map_or_else(String::new, |o| format!(" +{o}"));
            let (vlans, default_vlans) = match &site.vlans {
                Some(v) => (format!("{v}{own_vlans}"), false),
                None => (format!("{}{own_vlans}", draft.network.vlans), true),
            };
            let texts = vec![
                String::new(),
                topology::tree_name(&draft.network.label(site.id), depth),
                site.address.to_string(),
                vlans,
                format::role(
                    site.uplink.map(|up| draft.network.label(up)).as_deref(),
                    topology::has_clients(&draft.network, site.id),
                ),
                policy::managed_label(&draft.network, site),
                tunnel,
            ];
            let style = severity_style(draft.worst_within(&FieldPath::site(i)));
            (i, marker, texts, style, default_vlans, tunnel_style)
        })
        .collect();
    let headers = ["", "NAME", "ADDRESS", "VLANS", "ROLE", "CHANGES", "TUNNEL"];
    let texts: Vec<Vec<String>> = entries.iter().map(|e| e.2.clone()).collect();
    // Inside the border and the scrollbar.
    let widths = list::fit_widths(
        area.width.saturating_sub(3),
        &headers,
        &texts,
        &[1, 16, 15, 40, 30, 20, 12],
        3,
    );
    let rows: Vec<Row> = entries
        .into_iter()
        .map(|(_, marker, texts, style, default_vlans, tunnel_style)| {
            let cell = |c: usize| list::fit(&texts[c], widths[c]);
            Row::new(vec![
                Cell::from(marker),
                Cell::from(cell(1)),
                Cell::from(cell(2)),
                if default_vlans {
                    Cell::from(Span::styled(cell(3), theme::dim()))
                } else {
                    Cell::from(cell(3))
                },
                Cell::from(cell(4)),
                Cell::from(cell(5)),
                Cell::from(cell(6)).style(tunnel_style),
            ])
            .style(style)
        })
        .collect();
    let removed = running.map_or(0, |m| {
        m.sites
            .iter()
            .filter(|s| draft.network.site(s.id).is_none())
            .filter(|s| !rekeyed.iter().any(|(old, _)| *old == s.id))
            .count()
    });
    let mut title = format!(" Sites (draft): {} ", draft.network.sites.len());
    if removed > 0 {
        title.push_str(&format!("- {removed} removed "));
    }
    let widths: Vec<Constraint> = widths.into_iter().map(Constraint::Length).collect();
    let table = Table::new(rows, widths)
        .header(header)
        .block(titled(title))
        .row_highlight_style(theme::selected());
    let row = order
        .iter()
        .position(|(i, _)| *i == app.sites_selected)
        .unwrap_or_default();
    list::show(
        frame,
        table,
        area,
        &app.scrolls.sites,
        Some(row),
        order.len(),
        list::BOXED,
    );
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
        // Everything of the row, uncut, and what to do next.
        let vlans = site.vlans.as_ref().map_or_else(
            || format!("{} (the network's)", draft.network.vlans),
            ToString::to_string,
        );
        let own = site
            .own_vlans
            .as_ref()
            .map_or_else(String::new, |o| format!(" +own {o}"));
        // The keys are in the bar; offline only the way back.
        let keys = if site.online {
            String::new()
        } else if app.may_change_quietly(site.id) {
            " (o: take online)".to_string()
        } else {
            // Who may: they take it online.
            format!(
                " (offline: {} takes it online)",
                policy::managers(&draft.network, site)
            )
        };
        let name = draft.network.label(site.id);
        let text = format!(
            " {name}: {}; VLANs {vlans}{own}; changed by {}{keys}",
            format::role(
                site.uplink.map(|up| draft.network.label(up)).as_deref(),
                topology::has_clients(&draft.network, site.id)
            ),
            policy::managers(&draft.network, site)
        );
        lines.push(Line::styled(text, theme::dim()));
        // The peer row has the name the site runs under.
        if let Some(why) = app
            .peer(&app.running_name(site.id))
            .and_then(|p| p.down_reason.clone())
        {
            lines.push(Line::styled(format!(" Tunnel down: {why}"), theme::warn()));
        }
        if let Some(trunk) = draft
            .local
            .tunnel_trunks
            .get(&site.id)
            .map(|p| p.join(", "))
        {
            lines.push(Line::styled(
                format!(" The tunnel to {name} has its own trunk {trunk} here (Settings, 8)."),
                theme::dim(),
            ));
        }
    }
    if app.draft_dirty() {
        lines.push(Line::styled(
            " + new, * changed (not applied yet): a reviews and applies them, x discards them.",
            theme::dim(),
        ));
    } else if lines.is_empty() {
        lines.push(Line::styled(
            " n adds a site, i invites one; m: the VLANs of the network, l: the trunk ports of this host.",
            theme::dim(),
        ));
    }
    lines.truncate(3);
    lines
}
