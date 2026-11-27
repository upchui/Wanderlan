//! Choosing the uplink of a local breakout (Breakout form, Enter on
//! "Uplink"): the default route, the interfaces of this box that can be an
//! uplink, or a name typed in. One is chosen; the form gets it back.

use crossterm::event::KeyEvent;
use wanderlan_core::api::{Status, UplinkStatus};
use wanderlan_core::config::ifname_error;
use wanderlan_core::identity::SiteId;

use super::draft::Draft;
use super::form::Form;
use super::input::TextInput;

/// One row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UplinkRow {
    /// The interface; `None`: the default route.
    pub name: Option<String>,
    /// Operationally up (the default route: always).
    pub up: bool,
    /// Its addresses.
    pub addresses: String,
    /// Its default routes, as shown.
    pub routes: String,
    /// Why its traffic would not break out (yet), if so.
    pub problem: Option<String>,
    /// Something worth knowing.
    pub note: Option<String>,
}

impl UplinkRow {
    /// The value of the form field for this row (empty: the default route).
    pub fn value(&self) -> &str {
        self.name.as_deref().unwrap_or("")
    }
}

/// The state of the window, with the form it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct UplinkPicker {
    /// The form, given back with the choice (or as it was).
    pub form: Box<Form>,
    /// The site of the breakout.
    pub site: Option<SiteId>,
    /// How it is called.
    pub site_name: String,
    /// The breakout is at this box: its interfaces are listed.
    pub own: bool,
    /// The value of the row under the cursor (the rows change with every
    /// status: its place may too); `None`: "other:".
    pub selected: Option<String>,
    /// A name typed for "other:".
    pub manual: TextInput,
    /// Why the last choice was not taken.
    pub error: Option<String>,
}

/// The trunk ports of this box in `draft` (never an uplink).
fn trunk_ports(draft: &Draft) -> Vec<&str> {
    std::iter::once(&draft.local.trunk)
        .chain(draft.local.tunnel_trunks.values())
        .flatten()
        .map(String::as_str)
        .collect()
}

/// How the default routes of `uplink` read.
fn routes(uplink: &UplinkStatus) -> String {
    let route = |family: &str, gateway: &Option<String>| {
        gateway.as_ref().map(|g| match g.as_str() {
            "direct" => format!("{family} direct"),
            g => format!("{family} via {g}"),
        })
    };
    let parts: Vec<String> = [
        route("IPv4", &uplink.gateway4),
        route("IPv6", &uplink.gateway6),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(", ")
    }
}

/// Why the traffic of a breakout would not leave by `uplink` (yet), as the
/// daemon decides it: it must be up, have an address and a default route of
/// its own (or be a point-to-point link).
fn problem(uplink: &UplinkStatus) -> Option<String> {
    if !uplink.up {
        Some("down".into())
    } else if uplink.addresses.is_empty() {
        Some("no address: its traffic goes through the tunnel".into())
    } else if uplink.gateway4.is_none() && uplink.gateway6.is_none() && !uplink.pointopoint {
        Some("no default route of its own: its traffic goes through the tunnel".into())
    } else {
        None
    }
}

/// The rows: the default route first, then (at this box) its uplinks
/// that work, then the others, each group by name.
pub fn rows(draft: &Draft, status: Option<&Status>, own: bool, site_name: &str) -> Vec<UplinkRow> {
    let uplinks: Vec<&UplinkStatus> = if own {
        let trunks = trunk_ports(draft);
        status
            .map(|s| {
                s.uplinks
                    .iter()
                    .filter(|u| !trunks.contains(&u.name.as_str()))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let via: Vec<String> = uplinks
        .iter()
        .filter(|u| u.default4 || u.default6)
        .map(|u| u.name.clone())
        .collect();
    let default_note = if !own {
        format!("the default route of {site_name}")
    } else if via.is_empty() {
        "this box has no default route now".to_string()
    } else {
        format!("now {}", via.join(", "))
    };
    let mut out = vec![UplinkRow {
        name: None,
        up: true,
        addresses: String::new(),
        routes: String::new(),
        problem: None,
        note: Some(default_note),
    }];
    let mut listed: Vec<UplinkRow> = uplinks
        .into_iter()
        .map(|u| UplinkRow {
            name: Some(u.name.clone()),
            up: u.up,
            addresses: u.addresses.join(", "),
            routes: routes(u),
            problem: problem(u),
            note: u.pointopoint.then(|| "point-to-point".to_string()),
        })
        .collect();
    listed.sort_by(|a, b| (a.problem.is_some(), &a.name).cmp(&(b.problem.is_some(), &b.name)));
    out.extend(listed);
    out
}

impl UplinkPicker {
    /// The window for the uplink field of `form` (a breakout form).
    pub fn new(form: Form, draft: &Draft, status: Option<&Status>) -> Self {
        let site = form.breakout_site(draft);
        let own = site.is_some() && site == draft.local.self_id;
        let site_name = site.map(|s| draft.network.label(s)).unwrap_or_default();
        let current = form.field_value("uplink");
        let rows = rows(draft, status, own, &site_name);
        let (selected, manual) = match rows.iter().find(|r| r.value() == current.trim()) {
            Some(row) => (Some(row.value().to_string()), TextInput::new("")),
            // Typed before: in "other:".
            None => (None, TextInput::new(current.trim())),
        };
        Self {
            form: Box::new(form),
            site,
            site_name,
            own,
            selected,
            manual,
            error: None,
        }
    }

    /// The place of the cursor in `rows` (`rows.len()`: "other:", also for
    /// an interface that is no longer listed).
    pub fn cursor(&self, rows: &[UplinkRow]) -> usize {
        self.selected
            .as_deref()
            .and_then(|v| rows.iter().position(|r| r.value() == v))
            .unwrap_or(rows.len())
    }

    /// Puts the cursor on row `index` of `rows` ("other:" past them).
    pub fn select(&mut self, rows: &[UplinkRow], index: usize) {
        self.selected = rows.get(index).map(|r| r.value().to_string());
    }

    /// The rows now.
    pub fn rows(&self, draft: &Draft, status: Option<&Status>) -> Vec<UplinkRow> {
        rows(draft, status, self.own, &self.site_name)
    }

    /// The title of the window.
    pub fn title(&self) -> String {
        format!("Uplink of the breakout at {}", self.site_name)
    }

    /// The value the form has now.
    pub fn current(&self) -> String {
        self.form.field_value("uplink").trim().to_string()
    }

    /// A key while "other:" is focused.
    pub fn type_key(&mut self, key: &KeyEvent) {
        self.error = None;
        self.manual.handle(key);
    }

    /// The value chosen with Enter: the row under the cursor, or the name
    /// typed for "other:" (checked).
    pub fn chosen(&self, rows: &[UplinkRow], draft: &Draft) -> Result<String, String> {
        if let Some(row) = rows.get(self.cursor(rows)) {
            return Ok(row.value().to_string());
        }
        let name = self.manual.value().trim();
        if name.is_empty() || name == "auto" {
            return Ok(String::new());
        }
        if let Some(e) = ifname_error(name) {
            return Err(format!("\"{name}\" {e}"));
        }
        if self.own && trunk_ports(draft).contains(&name) {
            return Err(format!("{name} is a trunk port of this box, not an uplink"));
        }
        Ok(name.to_string())
    }

    /// The form with `value` as its uplink.
    pub fn take(self, value: &str, draft: &Draft) -> Form {
        let mut form = *self.form;
        form.set_field_value("uplink", value, draft);
        form
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::fixtures;

    fn uplink(name: &str) -> UplinkStatus {
        UplinkStatus {
            name: name.into(),
            up: true,
            addresses: vec!["192.0.2.2".into()],
            gateway4: Some("192.0.2.1".into()),
            ..UplinkStatus::default()
        }
    }

    #[test]
    fn this_box_lists_its_uplinks_without_its_trunk_ports() {
        let draft = fixtures::draft();
        let trunk = draft.local.trunk[0].clone();
        let mut status = fixtures::status();
        status.uplinks = vec![
            UplinkStatus {
                default4: true,
                ..uplink("ens18")
            },
            uplink(&trunk),
            UplinkStatus {
                gateway4: None,
                pointopoint: true,
                ..uplink("wg0")
            },
            UplinkStatus {
                gateway4: None,
                ..uplink("eth9")
            },
            UplinkStatus {
                up: false,
                ..uplink("eth8")
            },
        ];
        let rows = rows(&draft, Some(&status), true, "heim");
        let names: Vec<&str> = rows.iter().map(UplinkRow::value).collect();
        // The default route, those that work, then the others.
        assert_eq!(names, ["", "ens18", "wg0", "eth8", "eth9"]);
        assert_eq!(rows[0].note.as_deref(), Some("now ens18"));
        assert_eq!(rows[1].routes, "IPv4 via 192.0.2.1");
        assert!(rows[2].problem.is_none(), "point-to-point works");
        assert_eq!(rows[3].problem.as_deref(), Some("down"));
        assert!(
            rows[4]
                .problem
                .as_deref()
                .unwrap()
                .contains("no default route")
        );
        // Another site: only its default route (and a name typed in).
        let other = super::rows(&draft, Some(&status), false, "haus6");
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].note.as_deref(), Some("the default route of haus6"));
    }
}
