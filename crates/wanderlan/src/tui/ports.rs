//! Choosing the trunk ports of this host (Settings, Sites `l`): the host's
//! interfaces in a list, several selectable; the first chosen is the main
//! port. For a tunnel, nothing chosen means the default trunk.

use std::collections::BTreeMap;

use crossterm::event::KeyEvent;
use wanderlan_core::api::Interface;
use wanderlan_core::config::{FieldPath, ifname_error};
use wanderlan_core::desired::{SEGMENT_PREFIX, bridge_for};

use super::draft::Draft;
use super::form::trunk_candidates;
use super::input::TextInput;

/// Whose trunk ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortTarget {
    /// The default trunk of this host (`br0`).
    Default,
    /// The trunk of the tunnel to a site (a segment of its own).
    Tunnel(String),
}

/// One interface of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRow {
    /// Interface name.
    pub name: String,
    /// Kind for display (`physical`, `vlan`, `bridge`, ...); empty if the
    /// interface is not on this host.
    pub kind: String,
    /// Administratively up.
    pub up: bool,
    /// MTU (0 if not on this host).
    pub mtu: u32,
    /// Why it cannot be chosen, if so.
    pub blocked: Option<String>,
    /// Something worth knowing that does not stop it.
    pub note: Option<String>,
    /// Chosen but not on this host (e.g. an unplugged adapter).
    pub missing: bool,
}

/// The state of the window.
#[derive(Debug, Clone, PartialEq)]
pub struct PortPicker {
    /// Whose ports.
    pub target: PortTarget,
    /// The chosen ports in order; the first is the main port.
    pub chosen: Vec<String>,
    /// The row under the cursor; one past the interfaces is "other:".
    pub cursor: usize,
    /// A name typed for "other:".
    pub manual: TextInput,
    /// Why the last action did not work.
    pub error: Option<String>,
    /// Interface → the peers whose tunnels run over it (display text).
    leads: BTreeMap<String, String>,
    initial: Vec<String>,
}

impl PortPicker {
    /// The window for `target`, with the ports the draft has now. `leads`
    /// maps interfaces to the peers reached through them, as text
    /// (`buero, haus2`): never a trunk port, bridging them would cut the
    /// tunnels.
    pub fn new(target: PortTarget, draft: &Draft, leads: BTreeMap<String, String>) -> Self {
        let chosen = match &target {
            PortTarget::Default => draft.local.trunk.clone(),
            PortTarget::Tunnel(site) => draft
                .local
                .tunnel_trunks
                .get(site)
                .cloned()
                .unwrap_or_default(),
        };
        Self {
            target,
            initial: chosen.clone(),
            chosen,
            cursor: 0,
            manual: TextInput::default(),
            error: None,
            leads,
        }
    }

    /// The title of the window.
    pub fn title(&self) -> String {
        match &self.target {
            PortTarget::Default => "Trunk ports of this host".into(),
            PortTarget::Tunnel(site) => format!("Trunk ports of the tunnel to {site}"),
        }
    }

    /// True if the choice differs from what the draft has, or a name is
    /// typed but not added.
    pub fn changed(&self) -> bool {
        self.chosen != self.initial || !self.manual.value().trim().is_empty()
    }

    /// What the choice means, one sentence below the list.
    pub fn summary(&self, draft: &Draft) -> String {
        let list = |ports: &[String]| match ports {
            [] => String::new(),
            [only] => only.clone(),
            [main, rest @ ..] => format!("{main} (main port), {}", rest.join(", ")),
        };
        let default = &draft.local.trunk;
        match &self.target {
            PortTarget::Default if self.chosen.is_empty() => String::new(),
            PortTarget::Default => format!(
                "{}: {}, bridged like the ports of a switch. A loop between two ports shuts the further one down, never the main port.",
                wanderlan_core::BRIDGE_NAME,
                list(&self.chosen)
            ),
            PortTarget::Tunnel(site) if self.chosen.is_empty() || self.chosen == *default => {
                format!(
                    "Nothing chosen: the tunnel to {site} uses the default trunk ({}).",
                    default.join(", ")
                )
            }
            PortTarget::Tunnel(site) => format!(
                "A segment of its own, bridge {}: {}. Only the network on these ports reaches {site} through this site.",
                bridge_for(
                    &self.chosen[0],
                    draft.local.main_trunk().unwrap_or_default()
                ),
                list(&self.chosen)
            ),
        }
    }

    /// The rows: every interface of the host that could be listed, then
    /// chosen ports that are not there.
    pub fn rows(&self, draft: &Draft, interfaces: &[Interface]) -> Vec<PortRow> {
        let mut rows: Vec<PortRow> = trunk_candidates(interfaces)
            .into_iter()
            .map(|i| PortRow {
                name: i.name.clone(),
                kind: if i.kind.is_empty() {
                    "physical".into()
                } else {
                    i.kind.clone()
                },
                up: i.up,
                mtu: i.mtu,
                blocked: self.blocked(draft, i),
                note: self.note(draft, &i.name),
                missing: false,
            })
            .collect();
        for name in &self.chosen {
            if !rows.iter().any(|r| r.name == *name) {
                rows.push(PortRow {
                    name: name.clone(),
                    kind: String::new(),
                    up: false,
                    mtu: 0,
                    blocked: None,
                    note: Some("not on this host (now)".into()),
                    missing: true,
                });
            }
        }
        rows
    }

    /// The position of a port in the choice (0: the main port).
    pub fn position(&self, name: &str) -> Option<usize> {
        self.chosen.iter().position(|p| p == name)
    }

    /// For a tunnel: ports another tunnel uses too. Allowed if both end up
    /// with the same list (one segment for both).
    fn note(&self, draft: &Draft, name: &str) -> Option<String> {
        let PortTarget::Tunnel(me) = &self.target else {
            return None;
        };
        let others: Vec<&str> = draft
            .local
            .tunnel_trunks
            .iter()
            .filter(|(site, ports)| *site != me && ports.iter().any(|p| p == name))
            .map(|(site, _)| site.as_str())
            .collect();
        (!others.is_empty()).then(|| format!("also for {}", others.join(", ")))
    }

    /// Why `interface` cannot be a port of this trunk.
    fn blocked(&self, draft: &Draft, interface: &Interface) -> Option<String> {
        let name = interface.name.as_str();
        if let Some(peers) = self.leads.get(name) {
            return Some(format!("leads to {peers}"));
        }
        if let Some(master) = interface
            .master
            .as_deref()
            .filter(|m| *m != wanderlan_core::BRIDGE_NAME && !m.starts_with(SEGMENT_PREFIX))
        {
            return Some(format!("in {master}"));
        }
        if !matches!(interface.kind.as_str(), "" | "vlan" | "bond" | "veth") {
            return Some(format!("a {}", interface.kind));
        }
        // A port belongs to one segment.
        let local = &draft.local;
        match &self.target {
            PortTarget::Default => local
                .tunnel_trunks
                .iter()
                .find(|(_, ports)| ports.iter().any(|p| p == name))
                .map(|(site, _)| format!("trunk of the tunnel to {site}")),
            // Ports of other tunnels: see `note`.
            PortTarget::Tunnel(_) => local
                .trunk
                .iter()
                .any(|p| p == name)
                .then(|| "in the default trunk".into()),
        }
    }

    /// Chooses the row or takes it out of the choice. A blocked interface
    /// can only be taken out.
    pub fn toggle(&mut self, row: &PortRow) {
        self.error = None;
        if let Some(at) = self.chosen.iter().position(|p| *p == row.name) {
            self.chosen.remove(at);
        } else if let Some(why) = &row.blocked {
            self.error = Some(format!("{} cannot be a trunk port here: {why}", row.name));
        } else {
            self.chosen.push(row.name.clone());
        }
    }

    /// Makes a chosen port the main port (first in the list).
    pub fn make_main(&mut self, name: &str) {
        self.error = None;
        match self.chosen.iter().position(|p| p == name) {
            Some(at) => {
                let port = self.chosen.remove(at);
                self.chosen.insert(0, port);
            }
            None => self.error = Some(format!("Choose {name} first (Space).")),
        }
    }

    /// A key while "other:" is focused.
    pub fn type_key(&mut self, key: &KeyEvent) {
        self.error = None;
        self.manual.handle(key);
    }

    /// Adds the name typed for "other:".
    pub fn add_manual(&mut self) {
        self.error = None;
        let name = self.manual.value().trim().to_string();
        if name.is_empty() {
            return;
        }
        if let Some(msg) = ifname_error(&name) {
            self.error = Some(format!("{name}: {msg}"));
            return;
        }
        if let Some(peers) = self.leads.get(&name) {
            self.error = Some(format!(
                "{name} leads to {peers}; bridging it would cut the connection"
            ));
            return;
        }
        if !self.chosen.contains(&name) {
            self.chosen.push(name);
        }
        self.manual.set("");
    }

    /// The draft with the choice, or why it cannot be saved.
    pub fn apply(&self, draft: &Draft) -> Result<Draft, String> {
        let mut candidate = draft.clone();
        let path = match &self.target {
            PortTarget::Default => {
                if self.chosen.is_empty() {
                    return Err(
                        "Choose at least one port: the trunk carries the VLANs of this site."
                            .into(),
                    );
                }
                candidate.local.trunk = self.chosen.clone();
                FieldPath::local("trunk")
            }
            PortTarget::Tunnel(site) => {
                if self.chosen.is_empty() || self.chosen == candidate.local.trunk {
                    candidate.local.tunnel_trunks.remove(site);
                } else {
                    candidate
                        .local
                        .tunnel_trunks
                        .insert(site.clone(), self.chosen.clone());
                }
                FieldPath::local(&format!("tunnels.{site}.trunk"))
            }
        };
        candidate.revalidate();
        let problem = candidate
            .at(&path)
            .find(|d| d.is_error())
            .map(|d| d.message.clone());
        match problem {
            Some(message) => Err(message),
            None => Ok(candidate),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::draft::Running;
    use crate::tui::fixtures;

    fn draft() -> Draft {
        Draft::from_running(&Running::parse(&fixtures::config_texts()).unwrap()).unwrap()
    }

    fn leads() -> BTreeMap<String, String> {
        BTreeMap::from([("ens18".to_string(), "buero".to_string())])
    }

    fn row<'a>(rows: &'a [PortRow], name: &str) -> &'a PortRow {
        rows.iter().find(|r| r.name == name).expect(name)
    }

    #[test]
    fn the_list_marks_what_cannot_be_a_port() {
        let d = draft();
        let picker = PortPicker::new(PortTarget::Default, &d, leads());
        let rows = picker.rows(&d, &fixtures::interfaces());
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["br-lan", "ens18", "ens19", "eth1"],
            "no lo, br0, gt-*"
        );
        assert_eq!(
            row(&rows, "ens18").blocked.as_deref(),
            Some("leads to buero")
        );
        assert_eq!(row(&rows, "eth1").blocked.as_deref(), Some("in br-lan"));
        assert_eq!(row(&rows, "br-lan").blocked.as_deref(), Some("a bridge"));
        assert_eq!(row(&rows, "ens19").blocked, None);
        assert_eq!(picker.chosen, ["ens19"], "the trunk of the draft");
    }

    #[test]
    fn several_ports_in_order_and_the_main_port() {
        let mut d = draft();
        let interfaces = {
            let mut list = fixtures::interfaces();
            let mut extra = list[2].clone();
            extra.name = "ens20".into();
            list.push(extra);
            list
        };
        let mut picker = PortPicker::new(PortTarget::Default, &d, leads());
        let rows = picker.rows(&d, &interfaces);
        picker.toggle(row(&rows, "ens20"));
        assert_eq!(picker.chosen, ["ens19", "ens20"]);
        picker.make_main("ens20");
        assert_eq!(picker.chosen, ["ens20", "ens19"]);
        picker.toggle(row(&rows, "ens18"));
        assert!(picker.error.as_deref().unwrap().contains("leads to buero"));
        assert_eq!(picker.chosen.len(), 2, "not chosen");
        d = picker.apply(&d).expect("valid");
        assert_eq!(d.local.trunk, ["ens20", "ens19"]);
        // Nothing chosen is not a trunk.
        let mut empty = PortPicker::new(PortTarget::Default, &d, leads());
        empty.chosen.clear();
        assert!(empty.apply(&d).is_err());
    }

    #[test]
    fn a_tunnel_without_ports_uses_the_default_trunk() {
        let d = draft();
        let mut picker = PortPicker::new(PortTarget::Tunnel("haus2".into()), &d, leads());
        assert!(picker.chosen.is_empty());
        let rows = picker.rows(&d, &fixtures::interfaces());
        assert_eq!(
            row(&rows, "ens19").blocked.as_deref(),
            Some("in the default trunk")
        );
        picker.manual.set("usb0");
        picker.add_manual();
        assert_eq!(picker.chosen, ["usb0"]);
        let saved = picker.apply(&d).expect("valid");
        assert_eq!(saved.local.tunnel_trunks.get("haus2").unwrap(), &["usb0"]);
        // usb0 is not on the host: listed as missing, can be taken out.
        let mut again = PortPicker::new(PortTarget::Tunnel("haus2".into()), &saved, leads());
        let rows = again.rows(&saved, &fixtures::interfaces());
        assert!(row(&rows, "usb0").missing);
        again.toggle(row(&rows, "usb0"));
        let back = again.apply(&saved).expect("valid");
        assert!(
            back.local.tunnel_trunks.is_empty(),
            "the default trunk again"
        );
    }

    #[test]
    fn two_tunnels_may_share_a_segment() {
        let mut d = draft();
        d.local
            .tunnel_trunks
            .insert("buero".into(), vec!["ens18".into()]);
        d.revalidate();
        let mut picker = PortPicker::new(PortTarget::Tunnel("haus2".into()), &d, BTreeMap::new());
        let rows = picker.rows(&d, &fixtures::interfaces());
        let ens18 = row(&rows, "ens18");
        assert_eq!(ens18.blocked, None);
        assert_eq!(ens18.note.as_deref(), Some("also for buero"));
        picker.toggle(ens18);
        let saved = picker.apply(&d).expect("the same list: one segment");
        assert_eq!(saved.local.tunnel_trunks.get("haus2").unwrap(), &["ens18"]);
        assert!(picker.summary(&saved).contains("bridge wl-ens18"));
        // A different list with a shared port is two segments with one port.
        picker.manual.set("usb0");
        picker.add_manual();
        let err = picker.apply(&d).unwrap_err();
        assert!(err.contains("two segments"), "{err}");
    }

    #[test]
    fn typed_names_are_checked() {
        let d = draft();
        let mut picker = PortPicker::new(PortTarget::Default, &d, leads());
        picker.manual.set("this-name-is-too-long");
        picker.add_manual();
        assert!(picker.error.is_some());
        picker.manual.set("ens18");
        picker.add_manual();
        assert!(
            picker
                .error
                .as_deref()
                .unwrap()
                .contains("cut the connection")
        );
        assert_eq!(picker.chosen, ["ens19"]);
    }
}
