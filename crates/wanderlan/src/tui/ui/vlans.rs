//! View 3: the VLANs of the network and the mapping table, or (`t`) the
//! tunnels with the VLANs each carries. Conflicts are marked from the draft's
//! diagnostics, which are recomputed on every edit.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, Wrap};
use wanderlan_core::config::{FieldPath, LocalVid, Network, Severity, VlanMap, VlanSet};

use super::{diagnostic_line, list, not_configured, severity_style, titled};
use crate::tui::app::App;
use crate::tui::draft::Draft;
use crate::tui::{format, theme};

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let Some(draft) = &app.draft else {
        not_configured(frame, area);
        return;
    };
    let [vlans, table, detail] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .areas(area);
    draw_vlans(frame, draft, app.restricted_by().as_deref(), vlans);
    if app.vlans_tunnels {
        draw_tunnels(frame, app, draft, table, detail);
        return;
    }
    draw_maps(frame, app, draft, table);
    let selected = FieldPath::map(app.maps_selected);
    let mut lines: Vec<Line> = draft.within(&selected).map(diagnostic_line).collect();
    if lines.is_empty() {
        // Conflicts elsewhere in the table are shown right away, not only
        // when their row is selected.
        let paths: Vec<FieldPath> = (0..draft.network.maps.len()).map(FieldPath::map).collect();
        lines = paths
            .iter()
            .flat_map(|p| draft.within(p))
            .filter(|d| d.is_error())
            .map(diagnostic_line)
            .collect();
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            " A mapping changes the VLAN ID on one site's trunk; all other sites see the source VLAN.",
            theme::dim(),
        ));
    }
    lines.truncate(3);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), detail);
}

fn draw_vlans(frame: &mut Frame, draft: &Draft, restricted_by: Option<&str>, area: Rect) {
    let path = FieldPath::network("vlans");
    let vlans = &draft.network.vlans;
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(" {vlans}"),
            severity_style(draft.worst_at(&path)).patch(theme::bold()),
        ),
        Span::styled(
            if vlans.is_all() {
                "  (every VLAN and the untagged traffic)".to_string()
            } else {
                format!("  ({} VLANs)", vlans.len())
            },
            theme::dim(),
        ),
    ])];
    match (draft.at(&path).next(), restricted_by) {
        (Some(d), _) => lines.push(diagnostic_line(d)),
        (None, Some(manager)) => lines.push(Line::styled(
            format!(" Only the server {manager} changes the tunnel of this site: VLANs and mappings change there."),
            theme::warn(),
        )),
        (None, None) => lines.push(Line::styled(
            " Default of every site (native = untagged); limit a site in Sites (2).",
            theme::dim(),
        )),
    }
    // The own VLANs of this site: always its own business.
    let me_id = draft.local.self_id;
    let me_site = me_id.and_then(|id| draft.network.site(id));
    let me = me_id.map(|id| draft.network.label(id)).unwrap_or_default();
    let own = me_site
        .and_then(|s| s.own_vlans.as_ref())
        .map_or_else(|| "none".to_string(), ToString::to_string);
    let up = me_site
        .and_then(|s| s.uplink)
        .map_or_else(String::new, |u| {
            format!(", never to {}", draft.network.label(u))
        });
    lines.push(Line::from(vec![
        Span::styled(format!(" Own VLANs of {me} (o): "), theme::dim()),
        Span::styled(own, theme::bold()),
        Span::styled(format!("  trunk and own clients only{up}"), theme::dim()),
    ]));
    let title = if restricted_by.is_some() {
        " VLANs of the network (read only) "
    } else {
        " VLANs of the network (v) "
    };
    frame.render_widget(Paragraph::new(lines).block(titled(title)), area);
}

/// A cell that is marked when a diagnostic points at it.
fn marked(text: String, severity: Option<Severity>) -> Cell<'static> {
    match severity {
        Some(s) => Cell::from(format!("{text} !")).style(theme::severity(s)),
        None => Cell::from(text),
    }
}

fn effect(network: &Network, map: &VlanMap) -> String {
    let site = network.label(map.site);
    match map.local {
        LocalVid::Native => format!("source VLAN {} = untagged at {site}", map.shared),
        LocalVid::Vid(v) => format!("source VLAN {} = VLAN {v} at {site}", map.shared),
    }
}

fn vlans_text(vlans: &VlanSet) -> String {
    if vlans.is_empty() {
        "none".into()
    } else {
        vlans.to_string()
    }
}

fn draw_tunnels(frame: &mut Frame, app: &App, draft: &Draft, area: Rect, detail: Rect) {
    let rows = app.visible_tunnels();
    let selected = app.tunnels_selected.min(rows.len().saturating_sub(1));
    let area = if app.tunnels_filter.is_shown() {
        let [filter, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
        app.tunnels_filter
            .draw(frame, filter, rows.len(), draft.tunnel_rows().len());
        rest
    } else {
        area
    };
    let header = Row::new(vec!["", "TUNNEL", "KIND", "CARRIES", "LIMIT"]).style(theme::header());
    let running = app.running.as_ref().and_then(|r| r.network.as_ref());
    let table_rows: Vec<Row> = rows
        .iter()
        .map(|r| {
            let [a_id, b_id] = r.sites;
            let (a, b) = (draft.network.label(a_id), draft.network.label(b_id));
            let kind = match r.servers {
                Some(true) => "servers",
                Some(false) => "client",
                None => "-",
            };
            let carries = r
                .carries
                .as_ref()
                .map_or_else(|| "no tunnel".to_string(), vlans_text);
            let limit = r.limit.as_ref().map_or_else(|| "-".to_string(), vlans_text);
            let style = severity_style(
                r.entry
                    .and_then(|i| draft.worst_within(&FieldPath::tunnel(i))),
            );
            let before = running.and_then(|n| n.tunnel_limit(a_id, b_id));
            let marker = match (before, r.limit.as_ref()) {
                (None, Some(_)) => Span::styled("+", theme::ok()),
                (Some(x), Some(y)) if x != y => Span::styled("*", theme::warn()),
                (Some(_), None) => Span::styled("*", theme::warn()),
                _ => Span::raw(" "),
            };
            Row::new(vec![
                Cell::from(marker),
                Cell::from(format!("{a} – {b}")),
                Cell::from(kind),
                Cell::from(carries),
                Cell::from(limit),
            ])
            .style(style)
        })
        .collect();
    let limited = rows.iter().filter(|r| r.limit.is_some()).count();
    let title = format!(" Tunnels: {} ({limited} limited; t: mappings) ", rows.len());
    if table_rows.is_empty() {
        frame.render_widget(
            Paragraph::new(if app.tunnels_filter.is_shown() {
                " No tunnel matches the search (/ changes it)."
            } else {
                " No tunnels: this site is alone in the network."
            })
            .style(theme::dim())
            .block(titled(title)),
            area,
        );
    } else {
        let widths = [
            Constraint::Length(1),
            Constraint::Length(27),
            Constraint::Length(8),
            Constraint::Min(10),
            Constraint::Length(14),
        ];
        let total = table_rows.len();
        let table = Table::new(table_rows, widths)
            .header(header)
            .block(titled(title))
            .row_highlight_style(theme::selected());
        list::show(
            frame,
            table,
            area,
            &app.scrolls.tunnels,
            Some(selected),
            total,
            list::BOXED,
        );
    }
    let mut lines: Vec<Line> = rows
        .get(selected)
        .and_then(|r| r.entry)
        .map(|i| {
            draft
                .within(&FieldPath::tunnel(i))
                .map(diagnostic_line)
                .collect()
        })
        .unwrap_or_default();
    lines.extend(
        draft
            .within(&FieldPath::network("forwarding"))
            .map(diagnostic_line),
    );
    let me_id = draft
        .local
        .self_id
        .unwrap_or_else(|| wanderlan_core::identity::SiteId::provisional(""));
    let me = draft.network.label(me_id);
    for (vlans, groups) in wanderlan_core::forwarding::forwarding(&draft.network, me_id).passes {
        let groups: Vec<String> = groups
            .iter()
            .map(|g| {
                g.iter()
                    .map(|id| draft.network.label(*id))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .collect();
        lines.push(Line::styled(
            format!(
                " {me} passes VLANs {vlans} on between {}",
                groups.join(" / ")
            ),
            theme::dim(),
        ));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            " A tunnel carries the VLANs of both sites; Enter limits it, d lifts the limit.",
            theme::dim(),
        ));
    }
    lines.truncate(3);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), detail);
}

fn draw_maps(frame: &mut Frame, app: &App, draft: &Draft, area: Rect) {
    // The marker of a change and the number share a column ("* 2").
    let number_width = draft.network.maps.len().max(1).to_string().len();
    let header = Row::new(vec![
        format!("  {:>number_width$}", "#"),
        "SITE".into(),
        "SOURCE VLAN".into(),
        "MAPPED VLAN".into(),
        "MEANING".into(),
    ])
    .style(theme::header());
    let running = app.running.as_ref().and_then(|r| r.network.as_ref());
    let shown = app.visible_maps();
    let area = if app.maps_filter.is_shown() {
        let [filter, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
        app.maps_filter
            .draw(frame, filter, shown.len(), draft.network.maps.len());
        rest
    } else {
        area
    };
    let selected_row = shown
        .iter()
        .position(|i| *i == app.maps_selected)
        .unwrap_or_default();
    let site_text = |map: &VlanMap| {
        draft.network.site(map.site).map_or_else(
            || draft.network.label(map.site),
            |s| format::site_with_kind(&draft.network, s),
        )
    };
    let rows: Vec<Row> = shown
        .iter()
        .filter_map(|&i| draft.network.maps.get(i).map(|m| (i, m)))
        .map(|(i, map)| {
            let at = |key: &str| draft.worst_at(&FieldPath::map_field(i, key));
            let marker = match running {
                Some(old) if old.maps.contains(map) => Span::raw(" "),
                Some(old)
                    if old
                        .maps
                        .iter()
                        .any(|m| m.site == map.site && m.local == map.local) =>
                {
                    Span::styled("*", theme::warn())
                }
                Some(_) => Span::styled("+", theme::ok()),
                None => Span::raw(" "),
            };
            Row::new(vec![
                Cell::from(Line::from(vec![
                    marker,
                    Span::raw(format!(" {:>number_width$}", i + 1)),
                ])),
                marked(site_text(map), at("site")),
                marked(map.shared.to_string(), at("shared")),
                marked(map.local.to_string(), at("local")),
                Cell::from(effect(&draft.network, map)),
            ])
            .style(severity_style(draft.worst_within(&FieldPath::map(i))))
        })
        .collect();
    let empty = rows.is_empty();
    // SITE as wide as its longest entry (with " !"), # as its largest
    // number, over all mappings (the columns stay put while filtering).
    let site_width = draft
        .network
        .maps
        .iter()
        .enumerate()
        .map(|(i, map)| {
            let mark = draft
                .worst_at(&FieldPath::map_field(i, "site"))
                .map_or(0, |_| 2);
            site_text(map).chars().count() + mark
        })
        .max()
        .unwrap_or_default()
        .clamp(4, 21);
    let meaning_width = draft
        .network
        .maps
        .iter()
        .map(|m| effect(&draft.network, m).chars().count())
        .max()
        .unwrap_or_default();
    // Room between the columns: up to three spaces, as long as the longest
    // meaning still fits (in 80 columns that leaves one).
    let fixed = 2 + number_width + site_width + 11 + 11 + meaning_width;
    let inner = usize::from(area.width.saturating_sub(2));
    let spacing: u16 = (1..=3)
        .rev()
        .find(|&s| fixed + 4 * usize::from(s) <= inner)
        .unwrap_or(1);
    let widths = [
        Constraint::Length(u16::try_from(2 + number_width).unwrap_or(5)),
        Constraint::Length(u16::try_from(site_width).unwrap_or(21)),
        Constraint::Length(11),
        Constraint::Length(11),
        Constraint::Min(10),
    ];
    let errors = (0..draft.network.maps.len())
        .filter(|&i| draft.worst_within(&FieldPath::map(i)) == Some(Severity::Error))
        .count();
    let title = if errors > 0 {
        format!(
            " VLAN mappings: {} ({errors} with conflicts) - t: tunnels ",
            draft.network.maps.len()
        )
    } else {
        format!(" VLAN mappings: {} - t: tunnels ", draft.network.maps.len())
    };
    let total = rows.len();
    let table = Table::new(rows, widths)
        .column_spacing(spacing)
        .header(header)
        .block(titled(title))
        .row_highlight_style(theme::selected());
    if empty {
        frame.render_widget(
            Paragraph::new(
                " No mappings: every site uses the VLAN IDs of the network. n adds one.",
            )
            .style(theme::dim())
            .block(titled(" VLAN mappings ")),
            area,
        );
    } else {
        list::show(
            frame,
            table,
            area,
            &app.scrolls.maps,
            Some(selected_row),
            total,
            list::BOXED,
        );
    }
}
