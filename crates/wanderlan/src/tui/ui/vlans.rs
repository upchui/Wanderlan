//! View 3: the VLANs of the network and the mapping table. Conflicts are marked from
//! the draft's diagnostics, which are recomputed on every edit.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use wanderlan_core::config::{FieldPath, LocalVid, Severity, VlanMap};

use super::{diagnostic_line, not_configured, severity_style, titled};
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
            " A mapping changes the VLAN ID on one site's trunk; all other sites see the shared VLAN.",
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
            format!(" Managed by {manager}: VLANs and mappings change only there."),
            theme::warn(),
        )),
        (None, None) => lines.push(Line::styled(
            " Default of every site (native = untagged); limit a site in Sites (2).",
            theme::dim(),
        )),
    }
    // The own VLANs of this site: always its own business.
    let me = draft.local.self_name.as_deref().unwrap_or_default();
    let own = draft
        .network
        .site(me)
        .and_then(|s| s.own_vlans.as_ref())
        .map_or_else(|| "none".to_string(), ToString::to_string);
    let up = draft
        .network
        .site(me)
        .and_then(|s| s.uplink.as_deref())
        .map_or_else(String::new, |u| format!(", never to {u}"));
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

fn effect(map: &VlanMap) -> String {
    match map.local {
        LocalVid::Native => format!("untagged at {} = shared VLAN {}", map.site, map.shared),
        LocalVid::Vid(v) => format!("VLAN {v} at {} = shared VLAN {}", map.site, map.shared),
    }
}

fn draw_maps(frame: &mut Frame, app: &App, draft: &Draft, area: Rect) {
    let header = Row::new(vec!["#", "SITE", "LOCAL", "SHARED", "MEANING"]).style(theme::header());
    let rows: Vec<Row> = draft
        .network
        .maps
        .iter()
        .enumerate()
        .map(|(i, map)| {
            let at = |key: &str| draft.worst_at(&FieldPath::map_field(i, key));
            Row::new(vec![
                Cell::from(format!("{}", i + 1)),
                marked(
                    draft
                        .network
                        .site(&map.site)
                        .map_or_else(|| map.site.clone(), format::site_with_kind),
                    at("site"),
                ),
                marked(map.local.to_string(), at("local")),
                marked(map.shared.to_string(), at("shared")),
                Cell::from(effect(map)),
            ])
            .style(severity_style(draft.worst_within(&FieldPath::map(i))))
        })
        .collect();
    let empty = rows.is_empty();
    let widths = [
        Constraint::Length(3),
        Constraint::Length(21),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Min(10),
    ];
    let errors = (0..draft.network.maps.len())
        .filter(|&i| draft.worst_within(&FieldPath::map(i)) == Some(Severity::Error))
        .count();
    let title = if errors > 0 {
        format!(
            " VLAN mappings: {} ({errors} with conflicts) ",
            draft.network.maps.len()
        )
    } else {
        format!(" VLAN mappings: {} ", draft.network.maps.len())
    };
    let table = Table::new(rows, widths)
        .header(header)
        .block(titled(title))
        .row_highlight_style(theme::selected());
    let mut state = TableState::default();
    if empty {
        frame.render_widget(
            Paragraph::new(" No mappings: every site uses the shared VLAN IDs. n adds one.")
                .style(theme::dim())
                .block(titled(" VLAN mappings ")),
            area,
        );
    } else {
        state.select(Some(app.maps_selected));
        frame.render_stateful_widget(table, area, &mut state);
    }
}
