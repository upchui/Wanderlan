//! View 7: every setting of the network and of this host, with an
//! explanation. Enter opens the form with the focus on that setting; changes
//! go into the draft like everywhere else.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use wanderlan_core::config::FieldPath;
use wanderlan_core::desired::GRETAP_MTU;
use wanderlan_core::policy;

use super::{not_configured, severity_style, titled};
use crate::tui::app::App;
use crate::tui::draft::Draft;
use crate::tui::{format, theme};

/// One row of the settings list.
pub struct SettingRow {
    /// Key of the field in its form.
    pub key: &'static str,
    /// Label.
    pub label: &'static str,
    /// True for `local.toml` (this host), false for the network.
    pub local: bool,
    /// What it does.
    pub help: &'static str,
}

/// The settings, in display order.
pub const ROWS: [SettingRow; 7] = [
    SettingRow {
        key: "vlans",
        label: "VLANs",
        local: false,
        help: "VLANs of every site that does not list its own. Empty: all VLANs and the untagged traffic (native), like a trunk between two switches; or e.g. native,10-50.",
    },
    SettingRow {
        key: "commit_timeout",
        label: "Commit timeout",
        local: false,
        help: "Every change must be confirmed (c) within this time, otherwise it is rolled back, so nobody locks themselves out. 10 s to 1 h.",
    },
    SettingRow {
        key: "mtu_probe_interval",
        label: "Path MTU check",
        local: false,
        help: "How often the path MTU to every peer is measured (at least 10 s); it determines the TCP MSS. Also measured at start and when a peer comes back.",
    },
    SettingRow {
        key: "mss",
        label: "TCP MSS",
        local: false,
        help: "Empty: clamp TCP to the MSS that fits the measured path MTU (path MTU - 82). A fixed value (536-1460) replaces the measurement, e.g. if probes are filtered.",
    },
    SettingRow {
        key: "control_port",
        label: "Control port",
        local: false,
        help: "UDP and TCP port of the daemons on the site addresses: probes, configuration sync and joining. Must be reachable between all sites.",
    },
    SettingRow {
        key: "self",
        label: "This site",
        local: true,
        help: "Which site of the network this host is (local.toml, never synchronised).",
    },
    SettingRow {
        key: "trunk",
        label: "Trunk",
        local: true,
        help: "The local interface that carries the VLANs to the switch or hypervisor; it is a port of the bridge br0. Never the interface that leads to the other sites.",
    },
];

fn value(draft: &Draft, key: &str) -> String {
    let m = &draft.mesh;
    match key {
        "vlans" => {
            if m.vlans.is_all() {
                "all (every VLAN and untagged)".into()
            } else {
                format!("{} ({} VLANs)", m.vlans, m.vlans.len())
            }
        }
        "commit_timeout" => format::duration(m.commit_timeout),
        "mtu_probe_interval" => format!("every {}", format::duration(m.mtu_probe_interval)),
        "mss" => m
            .mss
            .map_or_else(|| "from the path MTU".into(), |v| format!("fixed {v}")),
        "control_port" => m.control_port.to_string(),
        "self" => draft.local.self_name.clone().unwrap_or_else(|| "-".into()),
        "trunk" => draft.local.trunk.clone().unwrap_or_else(|| "-".into()),
        _ => String::new(),
    }
}

/// Who manages this site and which sites it manages.
fn permissions(draft: &Draft) -> [(&'static str, String); 2] {
    let mesh = &draft.mesh;
    let me = draft.local.self_name.as_deref().unwrap_or_default();
    let this = match (mesh.site(me), policy::restricted_by(mesh, me)) {
        (_, Some(m)) => format!("managed by {m}: no VLAN changes here"),
        (Some(site), None) => match policy::manager(mesh, site) {
            Some(m) => format!("managed by {m}, may change its own VLANs"),
            None => "not managed by another site".to_string(),
        },
        (None, None) => "-".to_string(),
    };
    let managed: Vec<&str> = mesh
        .sites
        .iter()
        .filter(|s| s.name != me && policy::manager(mesh, s) == Some(me))
        .map(|s| s.name.as_str())
        .collect();
    let manages = if managed.is_empty() {
        "-".to_string()
    } else {
        managed.join(", ")
    };
    [("This site", this), ("Manages", manages)]
}

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let Some(draft) = &app.draft else {
        not_configured(frame, area);
        return;
    };
    let [table, help] = Layout::vertical([Constraint::Length(15), Constraint::Min(4)]).areas(area);
    let mut rows = vec![Row::new(vec![Cell::from(Span::styled(
        "Network (all sites)",
        theme::header(),
    ))])];
    let mut selected_row = 0;
    for (i, row) in ROWS.iter().enumerate() {
        if row.local && !ROWS[..i].iter().any(|r| r.local) {
            rows.push(Row::new(vec![Cell::from(Span::styled(
                "This host",
                theme::header(),
            ))]));
        }
        if i == app.settings_selected {
            selected_row = rows.len();
        }
        let path = if row.local {
            FieldPath::local(row.key)
        } else {
            FieldPath::mesh(row.key)
        };
        rows.push(
            Row::new(vec![
                Cell::from(format!("  {}", row.label)),
                Cell::from(value(draft, row.key)),
            ])
            .style(severity_style(draft.worst_at(&path))),
        );
    }
    rows.push(Row::new(vec![
        Cell::from(Span::styled("  Tunnel MTU", theme::dim())),
        Cell::from(Span::styled(
            format!("{GRETAP_MTU} (fixed; GRE fragments if the path is smaller)"),
            theme::dim(),
        )),
    ]));
    rows.push(Row::new(vec![
        Cell::from(Span::styled("Permissions", theme::header())),
        Cell::from(Span::styled(
            "who may change VLANs: Sites (2), Enter, Changes",
            theme::dim(),
        )),
    ]));
    for (label, value) in permissions(draft) {
        rows.push(Row::new(vec![
            Cell::from(Span::styled(format!("  {label}"), theme::dim())),
            Cell::from(Span::styled(value, theme::dim())),
        ]));
    }
    let widths = [Constraint::Length(22), Constraint::Min(20)];
    let mut state = TableState::default().with_selected(Some(selected_row));
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .block(titled(" Settings (Enter: change) "))
            .row_highlight_style(theme::selected()),
        table,
        &mut state,
    );
    let text = ROWS
        .get(app.settings_selected)
        .map_or("", |r| r.help)
        .to_string();
    let mut lines: Vec<Line> = format::wrap(&text, usize::from(help.width.saturating_sub(2)))
        .into_iter()
        .map(|l| Line::raw(format!(" {l}")))
        .collect();
    lines.extend([
        Line::raw(""),
        Line::styled(
            " Changes go into the draft; a reviews and applies them, c confirms.",
            theme::dim(),
        ),
    ]);
    frame.render_widget(Paragraph::new(lines), help);
}
