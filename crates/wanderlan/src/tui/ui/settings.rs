//! View 7: every setting of the network and of this host, with an
//! explanation. Enter opens the form with the focus on that setting; changes
//! go into the draft like everywhere else.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use wanderlan_core::config::FieldPath;
use wanderlan_core::desired::bridge_for;
use wanderlan_core::policy;

use super::{list, not_configured, severity_style, titled};
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
        help: "The interfaces that carry the VLANs to the switch or hypervisor (ports of br0; Enter lists those of this host). Several act like switch ports, the first is the main port. The one the tunnels run over is marked.",
    },
];

fn value(draft: &Draft, key: &str) -> String {
    let m = &draft.network;
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
        "trunk" if draft.local.trunk.is_empty() => "-".into(),
        "trunk" => draft.local.trunk.join(", "),
        _ => String::new(),
    }
}

/// The role of this site, who manages it, which sites it manages and which
/// sites are its clients.
fn permissions(draft: &Draft) -> [(&'static str, String); 4] {
    let network = &draft.network;
    let me = draft.local.self_name.as_deref().unwrap_or_default();
    let site = network.site(me);
    let role = site.map_or_else(
        || "-".to_string(),
        |s| match s.uplink.as_deref() {
            Some(up) => format!("client of {up} (its only tunnel leads there)"),
            None => "server (connected with the other servers)".to_string(),
        },
    );
    let managed = match (site, policy::restricted_by(network, me)) {
        (_, Some(m)) => format!("{m}: no VLAN changes here"),
        (Some(site), None) => match policy::manager(network, site) {
            Some(m) => format!("{m}, may change its own VLANs"),
            None => "- (not managed by another site)".to_string(),
        },
        (None, None) => "-".to_string(),
    };
    let list = |names: Vec<&str>| {
        if names.is_empty() {
            "-".to_string()
        } else {
            names.join(", ")
        }
    };
    let manages = list(
        network
            .sites
            .iter()
            .filter(|s| s.name != me && policy::manager(network, s) == Some(me))
            .map(|s| s.name.as_str())
            .collect(),
    );
    let clients = list(
        network
            .sites
            .iter()
            .filter(|s| s.uplink.as_deref() == Some(me))
            .map(|s| s.name.as_str())
            .collect(),
    );
    [
        ("Role", role),
        ("Clients", clients),
        ("Managed by", managed),
        ("Manages", manages),
    ]
}

/// A row of the settings list that can be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// One of [`ROWS`].
    Fixed(usize),
    /// The trunk of the tunnel to a site (index in the network).
    TunnelTrunk(usize),
}

/// Every selectable setting: the fixed ones, then the trunk of every tunnel.
pub fn settings(draft: Option<&Draft>) -> Vec<Setting> {
    let mut out: Vec<Setting> = (0..ROWS.len()).map(Setting::Fixed).collect();
    if let Some(d) = draft {
        let me = d.local.self_name.as_deref();
        out.extend(
            d.network
                .sites
                .iter()
                .enumerate()
                .filter(|(_, s)| Some(s.name.as_str()) != me)
                .map(|(i, _)| Setting::TunnelTrunk(i)),
        );
    }
    out
}

/// The trunk of the tunnel to `site`, for display.
fn tunnel_trunk(draft: &Draft, site: &str) -> String {
    let default = draft.local.main_trunk().unwrap_or("-");
    match draft.local.tunnel_trunks.get(site) {
        Some(ports) if *ports != draft.local.trunk => format!(
            "{} (own segment {})",
            ports.join(", "),
            bridge_for(ports.first().map_or("", String::as_str), default)
        ),
        _ => format!("default ({})", draft.local.trunk.join(", ")),
    }
}

fn help_text(draft: &Draft, setting: Option<Setting>) -> String {
    match setting {
        Some(Setting::Fixed(i)) => ROWS.get(i).map_or("", |r| r.help).to_string(),
        Some(Setting::TunnelTrunk(i)) => {
            let name = draft.network.sites.get(i).map_or("", |s| s.name.as_str());
            format!(
                "Local interfaces bridged with the tunnel to {name} (Enter lists those of this host); none: the default trunk. Others make it a segment of its own (bridge wl-<first>): only their network reaches {name}."
            )
        }
        None => String::new(),
    }
}

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let Some(draft) = &app.draft else {
        not_configured(frame, area);
        return;
    };
    let [table, help] = Layout::vertical([Constraint::Min(8), Constraint::Length(5)]).areas(area);
    let selectable = settings(Some(draft));
    let selected = selectable.get(app.settings_selected).copied();
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
        if selected == Some(Setting::Fixed(i)) {
            selected_row = rows.len();
        }
        let path = if row.local {
            FieldPath::local(row.key)
        } else {
            FieldPath::network(row.key)
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
        Cell::from(Span::styled("Role and rights", theme::header())),
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
    rows.push(Row::new(vec![
        Cell::from(Span::styled("Trunk per tunnel", theme::header())),
        Cell::from(Span::styled(
            "this host only; another trunk = own segment",
            theme::dim(),
        )),
    ]));
    for setting in &selectable {
        let Setting::TunnelTrunk(i) = *setting else {
            continue;
        };
        let Some(site) = draft.network.sites.get(i) else {
            continue;
        };
        if selected == Some(*setting) {
            selected_row = rows.len();
        }
        let path = FieldPath::local(&format!("tunnels.{}.trunk", site.name));
        rows.push(
            Row::new(vec![
                Cell::from(format!("  to {}", site.name)),
                Cell::from(tunnel_trunk(draft, &site.name)),
            ])
            .style(severity_style(draft.worst_at(&path))),
        );
    }
    let widths = [Constraint::Length(22), Constraint::Min(20)];
    let total = rows.len();
    let title = if total > usize::from(table.height.saturating_sub(2)) {
        " Settings (Enter: change, Up/Down: more) "
    } else {
        " Settings (Enter: change) "
    };
    list::show(
        frame,
        Table::new(rows, widths)
            .block(titled(title))
            .row_highlight_style(theme::selected()),
        table,
        &app.scrolls.settings,
        Some(selected_row),
        total,
        list::Chrome {
            bordered: true,
            header: false,
        },
    );
    let text = help_text(draft, selected);
    let mut lines: Vec<Line> = format::wrap(&text, usize::from(help.width.saturating_sub(2)))
        .into_iter()
        .map(|l| Line::raw(format!(" {l}")))
        .collect();
    lines.truncate(3);
    lines.extend([Line::styled(
        " Changes go into the draft; a reviews and applies them, c confirms.",
        theme::dim(),
    )]);
    frame.render_widget(Paragraph::new(lines), help);
}
