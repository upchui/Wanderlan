//! Modal forms that edit the draft: sites, VLAN mappings, network settings and
//! the local settings.
//!
//! Every change re-runs the field parsers and then the full validation on a
//! candidate draft with the edit applied. Diagnostics whose [`FieldPath`]
//! points at one of the form's fields are shown next to that field; other
//! new problems the edit would cause are listed as notes.

use std::net::Ipv4Addr;
use std::str::FromStr as _;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use wanderlan_core::api::{Interface, SiteRules, Translation};
use wanderlan_core::config::{
    Changes, Diagnostic, FieldPath, LocalVid, Network, Severity, Site, VID_MAX, VID_MIN, VlanMap,
    VlanSet, site_name_error,
};
use wanderlan_core::identity::SiteId;
use wanderlan_core::policy;

use super::draft::{Draft, is_within};
use super::format;
use super::input::{Outcome, TextInput};

/// What a form edits, by name (or content): a sync or a rollback may
/// replace the draft while the form is open, the entry is found again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormKind {
    /// A site; `None` adds a new one.
    Site(Option<SiteId>),
    /// A VLAN mapping (as it was); `None` adds a new one.
    Map(Option<VlanMap>),
    /// The ports of this site (its own entry in the network).
    OwnPorts(SiteId),
    /// Only the shared VLANs.
    Vlans,
    /// `local.toml`.
    Local,
    /// Rules for a new invitation (the draft is not changed).
    Invite,
    /// The own VLANs of one site: on its trunk and towards its own clients
    /// only.
    OwnVlans(SiteId),
    /// The VLANs of the tunnel between two sites.
    TunnelVlans(SiteId, SiteId),
    /// A new name for a site (everything refers to it by its identity).
    Rename(SiteId),
    /// The local breakout of a VLAN of a site (as it was: site and VLAN);
    /// `None` adds one.
    Breakout(Option<(SiteId, u16)>),
}

/// The editable value of a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Free text.
    Text(TextInput),
    /// One of a fixed list of values.
    Choice {
        /// The values.
        options: Vec<String>,
        /// Index of the chosen value.
        selected: usize,
    },
}

/// One field of a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// Key in the configuration file (`name`, `hub`, `vlans`, ...).
    pub key: &'static str,
    /// Label shown in the form.
    pub label: &'static str,
    /// Help text shown below the field.
    pub hint: String,
    /// The value.
    pub input: Input,
    /// Disabled fields are skipped (e.g. `hub` of a hub).
    pub enabled: bool,
    /// The user changed the field.
    pub touched: bool,
    /// Error from parsing or validation.
    pub error: Option<String>,
    /// Warning from validation.
    pub warning: Option<String>,
    /// Enter on the field opens a list to choose its value from (instead
    /// of saving the form).
    pub pick: bool,
}

impl Field {
    fn text(
        key: &'static str,
        label: &'static str,
        hint: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            key,
            label,
            hint: hint.into(),
            input: Input::Text(TextInput::new(value)),
            enabled: true,
            touched: false,
            error: None,
            warning: None,
            pick: false,
        }
    }

    fn choice(
        key: &'static str,
        label: &'static str,
        hint: impl Into<String>,
        options: Vec<String>,
        value: &str,
    ) -> Self {
        let selected = options.iter().position(|o| o == value).unwrap_or(0);
        Self {
            input: Input::Choice { options, selected },
            ..Self::text(key, label, hint, "")
        }
    }

    /// The current value as text.
    pub fn value(&self) -> &str {
        match &self.input {
            Input::Text(t) => t.value(),
            Input::Choice { options, selected } => {
                options.get(*selected).map_or("", String::as_str)
            }
        }
    }

    /// The error to show. Errors of empty fields the user has not touched
    /// yet (a new form) wait for the first submit.
    pub fn visible_error(&self, submitted: bool) -> Option<&str> {
        let shown = self.touched || submitted || !self.value().is_empty();
        self.error.as_deref().filter(|_| self.enabled && shown)
    }
}

/// Result of a key press in a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormEvent {
    /// Nothing to do.
    Nothing,
    /// A value changed; the form must be revalidated.
    Changed,
    /// The user cancelled.
    Cancel,
    /// The user wants to save.
    Submit,
    /// The user wants to choose the value of the focused field from a list
    /// ([`Field::pick`]).
    Pick,
}

/// A modal form.
#[derive(Debug, Clone, PartialEq)]
pub struct Form {
    /// What the form edits.
    pub kind: FormKind,
    /// Title of the popup.
    pub title: String,
    /// The fields.
    pub fields: Vec<Field>,
    /// Index of the focused field.
    pub focus: usize,
    /// Problems the edit causes that belong to no single field.
    pub notes: Vec<Diagnostic>,
    /// The user tried to save at least once.
    pub submitted: bool,
    candidate: Option<Box<Draft>>,
}

impl Form {
    fn new(kind: FormKind, title: impl Into<String>, fields: Vec<Field>, draft: &Draft) -> Self {
        let mut form = Self {
            kind,
            title: title.into(),
            fields,
            focus: 0,
            notes: Vec::new(),
            submitted: false,
            candidate: None,
        };
        form.update_enabled();
        form.revalidate(draft);
        form
    }

    /// Form for a new site (`None`) or an existing one.
    pub fn site(draft: &Draft, index: Option<usize>) -> Self {
        let existing = index.and_then(|i| draft.network.sites.get(i));
        let (name, address, vlans) = match existing {
            Some(s) => (
                s.name.clone(),
                s.address.to_string(),
                s.vlans
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            ),
            None => (String::new(), String::new(), String::new()),
        };
        let network = &draft.network;
        let title = match existing {
            Some(s) => format!("Edit site {}", network.label(s.id)),
            None => "New site".to_string(),
        };
        let me = draft.local.self_id;
        let my_name = me.map(|id| network.label(id)).unwrap_or_default();
        let choices: Vec<String> = Changes::ALL.iter().map(|c| c.label().to_string()).collect();
        // A new site: like an invitation, only its server.
        let current = existing.map_or(Changes::Server, |s| s.changes).label();
        // What it is plugged into: a server, or a client of another site.
        let mut servers = vec![NO_SERVER.to_string()];
        servers.extend(
            network
                .sites
                .iter()
                .filter(|s| Some(s.id) != existing.map(|e| e.id))
                .map(|s| network.label(s.id)),
        );
        let server = match existing {
            Some(s) => s
                .uplink
                .map_or_else(|| NO_SERVER.to_string(), |up| network.label(up)),
            None => my_name.clone(),
        };
        // Where it is, only its server decides; its tunnel as its option
        // says. By the rights of the network the draft's edits are checked
        // against, not by edits in the draft.
        let rights = draft.base.as_deref().unwrap_or(network);
        let (may_manage, may_change, may_rename) =
            match (existing.and_then(|s| rights.site(s.id)), me) {
                (Some(s), Some(me)) => (
                    policy::may_manage(rights, s, me),
                    policy::may_change(rights, s, me),
                    policy::may_rename(rights, s.id, me),
                ),
                _ => (true, true, true),
            };
        let itself = existing.is_some_and(|s| Some(s.id) == me);
        let mut fields = vec![
            Field::text(
                "name",
                "Name",
                "a-z, 0-9 and '-', at most 12 characters",
                name,
            ),
            Field::text(
                "address",
                "Address",
                "IPv4 address in the transport network (e.g. the VPN)",
                address,
            ),
            Field::text(
                "vlans",
                "VLANs",
                format!(
                    "empty: the network's VLANs ({}); or e.g. native,20,30",
                    draft.network.vlans
                ),
                vlans,
            ),
            Field::choice(
                "server",
                "Server",
                "none: a server (connected with all servers); or the site it is a client of",
                servers,
                &server,
            ),
            Field::choice(
                "changes",
                "Changes",
                "of a client: who changes its tunnel (VLANs, mappings, breakouts, online); a server: every server",
                choices,
                current,
            ),
            own_vlans_field(existing),
        ];
        for f in &mut fields {
            let (allowed, who) = match f.key {
                "address" | "server" => (may_manage, "its server decides this"),
                "vlans" | "changes" => (may_change, "only as its option Changes says"),
                "name" => (may_rename, "only the site itself or its server renames it"),
                "own_vlans" => (
                    itself || may_change,
                    "the site itself, or as its option Changes says",
                ),
                _ => (true, ""),
            };
            if !allowed {
                f.enabled = false;
                f.hint = who.into();
            }
        }
        let id = existing.map(|s| s.id);
        Self::new(FormKind::Site(id), title, fields, draft)
    }

    /// Form for a new invitation: rules for the site that joins with it.
    pub fn invite(draft: &Draft) -> Self {
        let fields = vec![
            Field::text("name", "Name", "empty: the host name of the new site", ""),
            Field::text(
                "vlans",
                "VLANs",
                format!("empty: the network's VLANs ({})", draft.network.vlans),
                "",
            ),
            Field::text(
                "native",
                "Native",
                "empty: none; or the source VLAN that is untagged (native) on its trunk",
                "",
            ),
            Field::text(
                "translations",
                "Mappings",
                "mapped:source, e.g. 130:30, 140:40 (source VLAN 30 is VLAN 130 on its trunk)",
                "",
            ),
            Field::choice(
                "changes",
                "Changes",
                "who changes its VLANs, mappings, breakouts, online",
                Changes::ALL.iter().map(|c| c.label().to_string()).collect(),
                Changes::Server.label(),
            ),
        ];
        let title = match draft.local.self_id {
            Some(me) => format!(
                "Invite a new site (it becomes a client of {})",
                draft.network.label(me)
            ),
            None => "Invite a new site".to_string(),
        };
        Self::new(FormKind::Invite, title, fields, draft)
    }

    /// The rules of an invitation form.
    pub fn invite_rules(&self) -> SiteRules {
        let text = |key: &str| {
            let v = self.value(key);
            (!v.trim().is_empty()).then(|| v.trim().to_string())
        };
        SiteRules {
            name: text("name"),
            vlans: text("vlans"),
            native: text("native").and_then(|v| v.parse().ok()),
            translations: parse_translations(&self.value("translations")).unwrap_or_default(),
            changes: self.value("changes").parse().ok(),
            ..SiteRules::default()
        }
    }

    /// Offers in the choice `key` only the sites `keep` says yes to (the
    /// sites this site may change). False if none is left.
    pub fn limit_sites(&mut self, key: &str, keep: impl Fn(SiteId) -> bool, draft: &Draft) -> bool {
        let Some(field) = self.field_mut(key) else {
            return true;
        };
        let Input::Choice { options, selected } = &mut field.input else {
            return true;
        };
        let current = options.get(*selected).cloned();
        // "marcel (client)": the site is the first word.
        options.retain(|o| site_choice(&draft.network, o).is_some_and(&keep));
        *selected = current
            .and_then(|c| options.iter().position(|o| *o == c))
            .unwrap_or(0);
        let any = !options.is_empty();
        self.revalidate(draft);
        any
    }

    /// Moves the focus to the field `key` (Settings view).
    pub fn focus_on(&mut self, key: &str) {
        if let Some(i) = self.fields.iter().position(|f| f.key == key) {
            self.focus = i;
        }
    }

    /// Form for a new VLAN mapping (`None`) or an existing one.
    pub fn map(draft: &Draft, index: Option<usize>, site_hint: Option<SiteId>) -> Self {
        let network = &draft.network;
        let existing = index.and_then(|i| network.maps.get(i));
        // "marcel (client)": where the mapped traffic comes from.
        let sites: Vec<String> = network
            .sites
            .iter()
            .map(|s| format::site_with_kind(network, s))
            .collect();
        let site = existing
            .map(|m| m.site)
            .or(site_hint)
            .or(draft.local.self_id)
            .and_then(|id| network.site(id))
            .map(|s| format::site_with_kind(network, s))
            .unwrap_or_default();
        let (local, shared) = existing.map_or_else(
            || ("native".to_string(), String::new()),
            |m| (m.local.to_string(), m.shared.to_string()),
        );
        let title = match index {
            Some(i) => format!("Edit VLAN mapping #{}", i + 1),
            None => "New VLAN mapping".to_string(),
        };
        let fields = vec![
            Field::choice(
                "site",
                "Site",
                "the site whose trunk uses a different VLAN",
                sites,
                &site,
            ),
            Field::text(
                "shared",
                "Source VLAN",
                "the VLAN in the network (one of the site's VLANs)",
                shared,
            ),
            Field::text(
                "local",
                "Mapped VLAN",
                "\"native\" (untagged) or the VLAN ID on this site's trunk",
                local,
            ),
        ];
        Self::new(FormKind::Map(existing.cloned()), title, fields, draft)
    }

    /// Form for the local breakout of a VLAN of a site (`existing`), or a
    /// new one (at `site_hint`). Enter on the uplink lists the interfaces
    /// of this box to choose from.
    pub fn breakout(
        draft: &Draft,
        existing: Option<(SiteId, u16)>,
        site_hint: Option<SiteId>,
    ) -> Self {
        let network = &draft.network;
        let sites: Vec<String> = network
            .sites
            .iter()
            .map(|s| format::site_with_kind(network, s))
            .collect();
        let site = existing
            .map(|(s, _)| s)
            .or(site_hint)
            .or(draft.local.self_id);
        let entry =
            existing.and_then(|(s, vid)| network.site(s).and_then(|site| site.breakout.get(&vid)));
        let shown = site
            .and_then(|id| network.site(id))
            .map(|s| format::site_with_kind(network, s))
            .unwrap_or_default();
        let vid = existing
            .as_ref()
            .map(|(_, v)| {
                if *v == wanderlan_core::config::NATIVE_VID {
                    "native".to_string()
                } else {
                    v.to_string()
                }
            })
            .unwrap_or_default();
        let uplink = entry.and_then(|b| b.uplink.clone()).unwrap_or_default();
        let except: Vec<String> = entry
            .map(|b| b.except.iter().map(ToString::to_string).collect())
            .unwrap_or_default();
        let title = match &existing {
            Some((s, v)) => format!("Breakout of VLAN {v} at {}", network.label(*s)),
            None => "New local breakout".to_string(),
        };
        let fields = vec![
            Field::choice(
                "site",
                "Site",
                "the site whose devices go to the internet through its own uplink",
                sites,
                &shown,
            ),
            Field::text(
                "vlan",
                "VLAN",
                "the VLAN in the network: native (untagged), or a VLAN ID (a mapped one: its source VLAN)",
                vid,
            ),
            Field {
                pick: true,
                ..Field::text(
                    "uplink",
                    "Uplink",
                    "empty: the site's default route; Enter: choose from the interfaces",
                    uplink,
                )
            },
            Field::text(
                "except",
                "Except",
                "public destinations that still go through the tunnel, e.g. 203.0.113.0/24, 2001:db8::/32",
                except.join(", "),
            ),
        ];
        Self::new(FormKind::Breakout(existing), title, fields, draft)
    }

    /// Form for the ports of this site (its own entry: the other sites
    /// connect there). Empty: the default of the network.
    pub fn own_ports(draft: &Draft) -> Self {
        let me = draft
            .local
            .self_id
            .unwrap_or_else(|| SiteId::provisional(""));
        let ports = draft.network.site(me).map(|s| s.ports).unwrap_or_default();
        let shown = |p: Option<u16>| p.map(|p| p.to_string()).unwrap_or_default();
        let fields = vec![
            Field::text(
                "control_port",
                "Control port",
                format!(
                    "UDP/TCP port of this site's daemon; empty: the network's ({})",
                    draft.network.control_port
                ),
                shown(ports.control),
            ),
            Field::text(
                "tunnel_port",
                "Tunnel port",
                "UDP port of this site's tunnels (NAT also the one above); empty: control port + 1",
                shown(ports.tunnel),
            ),
        ];
        Self::new(FormKind::OwnPorts(me), "Ports of this site", fields, draft)
    }

    /// Form for the own VLANs of the site `site` (index in the network).
    pub fn own_vlans(draft: &Draft, site: usize) -> Self {
        let s = draft.network.sites.get(site);
        let id = s.map_or_else(|| SiteId::provisional(""), |s| s.id);
        let name = draft.network.label(id);
        let fields = vec![own_vlans_field(s)];
        Self::new(
            FormKind::OwnVlans(id),
            format!("Own VLANs of {name}"),
            fields,
            draft,
        )
    }

    /// Form for a new name of a site.
    pub fn rename(draft: &Draft, site: usize) -> Self {
        let entry = draft.network.sites.get(site);
        let name = entry.map(|s| s.name.clone()).unwrap_or_default();
        let id = entry.map_or_else(|| SiteId::provisional(""), |s| s.id);
        let fields = vec![Field::text(
            "name",
            "New name",
            "a-z, 0-9 and -, at most 12 characters; another site may have it too (they are told apart by their identities)",
            name,
        )];
        Self::new(
            FormKind::Rename(id),
            format!("Rename site {}", draft.network.label(id)),
            fields,
            draft,
        )
    }

    /// Form for the VLANs of the tunnel between the sites `a` and `b`.
    pub fn tunnel_vlans(draft: &Draft, a: SiteId, b: SiteId) -> Self {
        let value = draft
            .network
            .tunnel_limit(a, b)
            .map(|v| {
                if v.is_empty() {
                    "none".to_string()
                } else {
                    v.to_string()
                }
            })
            .unwrap_or_default();
        let fields = vec![Field::text(
            "vlans",
            "VLANs",
            "empty: all VLANs both sites have; e.g. 30-40 or none",
            value,
        )];
        let (la, lb) = (draft.network.label(a), draft.network.label(b));
        Self::new(
            FormKind::TunnelVlans(a, b),
            format!("VLANs of the tunnel {la} – {lb}"),
            fields,
            draft,
        )
    }

    /// Form for the shared VLANs only.
    pub fn vlans(draft: &Draft) -> Self {
        let fields = vec![vlans_field(&draft.network.vlans)];
        Self::new(FormKind::Vlans, "VLANs of the network", fields, draft)
    }

    /// Form for `local.toml` opened on the setting `key`: which site this
    /// host is (a form of its own: it changes the identity), or the timings
    /// of this host. The trunk ports have a window of their own (`ports.rs`).
    pub fn local(draft: &Draft, key: &str) -> Self {
        if key == "self" {
            let network = &draft.network;
            let sites: Vec<String> = network.sites.iter().map(|s| network.label(s.id)).collect();
            let own = draft
                .local
                .self_id
                .map(|id| network.label(id))
                .unwrap_or_default();
            let fields = vec![Field::choice(
                "self",
                "This site",
                "which site of the network this host is",
                sites,
                &own,
            )];
            return Self::new(FormKind::Local, "This site (local.toml)", fields, draft);
        }
        let l = &draft.local;
        let n = &draft.network;
        let fields = vec![
            Field::text(
                "commit_timeout",
                "Commit timeout",
                format!(
                    "time to confirm a change here, e.g. 60s; empty: the network's ({})",
                    format::duration(n.commit_timeout)
                ),
                l.commit_timeout.map(format::duration).unwrap_or_default(),
            ),
            Field::text(
                "mtu_probe_interval",
                "Path MTU check",
                format!(
                    "how often this site measures the path MTU, e.g. 5m; empty: the network's ({})",
                    format::duration(n.mtu_probe_interval)
                ),
                l.mtu_probe_interval
                    .map(format::duration)
                    .unwrap_or_default(),
            ),
            Field::text(
                "mss",
                "TCP MSS",
                "empty: derive from the measured path MTU; or 536-1460",
                l.mss.map(|v| v.to_string()).unwrap_or_default(),
            ),
        ];
        Self::new(FormKind::Local, "This host (local.toml)", fields, draft)
    }

    /// Looks up a field by key.
    pub fn field(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }

    fn field_mut(&mut self, key: &str) -> Option<&mut Field> {
        self.fields.iter_mut().find(|f| f.key == key)
    }

    fn value(&self, key: &str) -> String {
        self.field(key)
            .map(|f| f.value().trim().to_string())
            .unwrap_or_default()
    }

    fn set_error(&mut self, key: &str, message: impl Into<String>) {
        if let Some(f) = self.field_mut(key) {
            f.error.get_or_insert_with(|| message.into());
        }
    }

    /// Handles a key. The caller revalidates on [`FormEvent::Changed`].
    pub fn handle_key(&mut self, key: &KeyEvent) -> FormEvent {
        match key.code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Enter => {
                let pick = self
                    .fields
                    .get(self.focus)
                    .is_some_and(|f| f.pick && f.enabled);
                return if pick {
                    FormEvent::Pick
                } else {
                    FormEvent::Submit
                };
            }
            KeyCode::Tab | KeyCode::Down => {
                self.move_focus(true);
                return FormEvent::Nothing;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.move_focus(false);
                return FormEvent::Nothing;
            }
            _ => {}
        }
        let Some(field) = self.fields.get_mut(self.focus) else {
            return FormEvent::Nothing;
        };
        let changed = match &mut field.input {
            Input::Choice { options, selected } if !options.is_empty() => match key.code {
                KeyCode::Left => {
                    *selected = (*selected + options.len() - 1) % options.len();
                    true
                }
                KeyCode::Right | KeyCode::Char(' ') => {
                    *selected = (*selected + 1) % options.len();
                    true
                }
                _ => false,
            },
            Input::Choice { .. } => false,
            Input::Text(text) => text.handle(key) == Outcome::Edited,
        };
        if changed {
            field.touched = true;
            FormEvent::Changed
        } else {
            FormEvent::Nothing
        }
    }

    fn move_focus(&mut self, forward: bool) {
        let n = self.fields.len();
        for step in 1..=n {
            let i = if forward {
                (self.focus + step) % n
            } else {
                (self.focus + n - step) % n
            };
            if self.fields.get(i).is_some_and(|f| f.enabled) {
                self.focus = i;
                return;
            }
        }
    }

    fn update_enabled(&mut self) {}

    /// Shows an error that belongs to no single field (the daemon refused
    /// what the form sent).
    pub fn set_note_error(&mut self, message: String) {
        self.notes.push(Diagnostic::error(
            wanderlan_core::config::ConfigFile::Network,
            FieldPath::root(),
            message,
        ));
    }

    /// Parses the fields and validates the draft with the edit applied.
    pub fn revalidate(&mut self, draft: &Draft) {
        self.update_enabled();
        for f in &mut self.fields {
            f.error = None;
            f.warning = None;
        }
        self.notes.clear();
        self.candidate = self.build(draft).map(Box::new);
        let Some(candidate) = self.candidate.take() else {
            return;
        };
        let entity = self.entity_path(draft);
        for diag in &candidate.diagnostics {
            let key = self
                .fields
                .iter()
                .find(|f| f.enabled && self.path_of(f.key, draft) == diag.path)
                .map(|f| f.key);
            match key {
                Some(key) => {
                    if let Some(field) = self.field_mut(key) {
                        let slot = match diag.severity {
                            Severity::Error => &mut field.error,
                            Severity::Warning => &mut field.warning,
                        };
                        slot.get_or_insert_with(|| diag.message.clone());
                    }
                }
                None => {
                    let own = entity.as_ref().is_some_and(|e| is_within(&diag.path, e));
                    let new = !draft.diagnostics.iter().any(|d| d.message == diag.message);
                    if own || new {
                        self.notes.push(diag.clone());
                    }
                }
            }
        }
        self.candidate = Some(candidate);
    }

    /// True if all fields are valid.
    pub fn is_valid(&self) -> bool {
        self.candidate.is_some() && self.fields.iter().all(|f| !f.enabled || f.error.is_none())
    }

    /// Tries to save: returns the new draft if all fields are valid.
    pub fn submit(&mut self, draft: &Draft) -> Option<Draft> {
        self.submitted = true;
        self.revalidate(draft);
        if self.is_valid() {
            self.candidate.take().map(|d| *d)
        } else {
            if let Some(i) = self
                .fields
                .iter()
                .position(|f| f.enabled && f.error.is_some())
            {
                self.focus = i;
            }
            None
        }
    }

    /// Where the edited site is in `draft`.
    fn site_index(draft: &Draft, id: SiteId) -> Option<usize> {
        draft.network.sites.iter().position(|s| s.id == id)
    }

    /// Index of the edited entity in the candidate draft.
    fn index(&self, draft: &Draft) -> Option<usize> {
        match &self.kind {
            FormKind::Site(None) | FormKind::Invite => Some(draft.network.sites.len()),
            FormKind::Site(Some(id))
            | FormKind::OwnVlans(id)
            | FormKind::Rename(id)
            | FormKind::OwnPorts(id) => Self::site_index(draft, *id),
            FormKind::Breakout(_) => Self::site_index(draft, self.breakout_site(draft)?),
            FormKind::Map(None) => Some(draft.network.maps.len()),
            FormKind::Map(Some(map)) => draft.network.maps.iter().position(|m| m == map),
            FormKind::TunnelVlans(a, b) => {
                let (a, b) = (*a, *b);
                Some(
                    draft
                        .network
                        .tunnels
                        .iter()
                        .position(|t| t.is(a, b))
                        .unwrap_or(draft.network.tunnels.len()),
                )
            }
            FormKind::Vlans | FormKind::Local => None,
        }
    }

    fn entity_path(&self, draft: &Draft) -> Option<FieldPath> {
        let i = self.index(draft)?;
        match &self.kind {
            FormKind::Site(_)
            | FormKind::Invite
            | FormKind::OwnVlans(_)
            | FormKind::Rename(_)
            | FormKind::OwnPorts(_) => Some(FieldPath::site(i)),
            FormKind::Map(_) => Some(FieldPath::map(i)),
            FormKind::TunnelVlans(..) => Some(FieldPath::tunnel(i)),
            FormKind::Breakout(_) => Some(FieldPath::site_field(i, "breakout")),
            FormKind::Vlans | FormKind::Local => None,
        }
    }

    /// Path of a field in the candidate draft.
    fn path_of(&self, key: &str, draft: &Draft) -> FieldPath {
        let i = self.index(draft).unwrap_or_default();
        match &self.kind {
            FormKind::Site(_)
            | FormKind::Invite
            | FormKind::OwnVlans(_)
            | FormKind::Rename(_)
            | FormKind::OwnPorts(_) => FieldPath::site_field(i, key),
            FormKind::Map(_) => FieldPath::map_field(i, key),
            FormKind::TunnelVlans(..) => FieldPath::tunnel_field(i, key),
            FormKind::Breakout(_) => {
                // As the diagnostics name it: the untagged traffic is VLAN 1.
                let vid = match self.value("vlan").trim() {
                    "native" => wanderlan_core::config::NATIVE_VID.to_string(),
                    other => other.to_string(),
                };
                match key {
                    "vlan" => FieldPath::site_field(i, &format!("breakout.{vid}")),
                    "site" => FieldPath::site_field(i, "name"),
                    _ => FieldPath::site_field(i, &format!("breakout.{vid}.{key}")),
                }
            }
            FormKind::Vlans => FieldPath::network(key),
            FormKind::Local => FieldPath::local(key),
        }
    }

    /// True once something was typed or chosen in the form.
    pub fn is_touched(&self) -> bool {
        self.fields.iter().any(|f| f.touched)
    }

    /// The edited entry is no longer in the draft (a sync or a rollback
    /// replaced it): nothing to save.
    fn gone(&mut self, what: &str) -> Option<Draft> {
        self.notes.clear();
        self.set_note_error(format!(
            "{what} is no longer in the network (the configuration changed); Esc closes"
        ));
        None
    }

    /// Builds the candidate draft; records parse errors on the fields.
    fn build(&mut self, draft: &Draft) -> Option<Draft> {
        let mut candidate = draft.clone();
        match self.kind.clone() {
            FormKind::Site(id) => {
                let index = match id {
                    Some(id) => match Self::site_index(draft, id) {
                        Some(i) => Some(i),
                        None => {
                            return self.gone(&format!("Site {}", draft.network.label(id)));
                        }
                    },
                    None => None,
                };
                let site = self.parse_site()?;
                // Who may change it and where it is plugged in change only
                // when the field was changed.
                let changes_touched = self.field("changes").is_some_and(|f| f.touched);
                let changes: Changes = self.value("changes").parse().unwrap_or(Changes::Server);
                let server_touched = self.field("server").is_some_and(|f| f.touched);
                let server = self.value("server");
                let uplink = (server != NO_SERVER && !server.is_empty())
                    .then(|| site_choice(&draft.network, &server))
                    .flatten();
                let own_vlans = self.parse_own_vlans()?;
                let old_name = id
                    .and_then(|id| draft.network.site(id))
                    .map(|s| s.name.clone());
                if let (Some(old), Some(me)) = (id, draft.local.self_id)
                    && old_name.as_deref() != Some(site.name.as_str())
                    && !policy::may_rename(&draft.network, old, me)
                {
                    self.set_error("name", policy::why_not_rename(&draft.network, old, me));
                    return None;
                }
                // Known by its name until its box has an identity: a name of
                // its own (as in the rename form).
                if let Some(old) = id.and_then(|id| draft.network.site(id))
                    && old.name != site.name
                    && !old.has_identity()
                    && draft.network.named(&site.name).next().is_some()
                {
                    self.set_error(
                        "name",
                        format!(
                            "\"{}\" is already a site of the network, and this one is known by its name until its box has an identity",
                            site.name
                        ),
                    );
                    return None;
                }
                // What was not touched stays as it is now: a sync may have
                // changed it since the form was opened.
                let touched = |key: &str| self.field(key).is_some_and(|f| f.touched);
                let kept = |key: &str| !touched(key);
                let (keep_name, keep_address, keep_vlans, keep_own) = (
                    kept("name"),
                    kept("address"),
                    kept("vlans"),
                    kept("own_vlans"),
                );
                match index.and_then(|i| candidate.network.sites.get_mut(i)) {
                    Some(slot) => {
                        let mut site = site;
                        site.online = slot.online;
                        site.changes = if changes_touched {
                            changes
                        } else {
                            slot.changes
                        };
                        site.uplink = if server_touched { uplink } else { slot.uplink };
                        // A server has no such option.
                        if site.uplink.is_none() {
                            site.changes = Changes::default();
                        }
                        // Its identity is the box's, its ports are its own,
                        // its breakouts have a view of their own: kept;
                        // only as a server it has none (the gateways are
                        // there).
                        site.id = slot.id;
                        site.ports = slot.ports;
                        site.breakout = if site.uplink.is_some() {
                            slot.breakout.clone()
                        } else {
                            Default::default()
                        };
                        site.own_vlans = if keep_own {
                            slot.own_vlans.clone()
                        } else {
                            own_vlans
                        };
                        if keep_address {
                            site.address = slot.address;
                        }
                        if keep_vlans {
                            site.vlans.clone_from(&slot.vlans);
                        }
                        let name = if keep_name {
                            slot.name.clone()
                        } else {
                            site.name.clone()
                        };
                        site.name.clone_from(&slot.name);
                        let old = std::mem::replace(slot, site);
                        if old.name != name {
                            candidate.rename_site(old.id, &name);
                        }
                    }
                    // A new site: a client of this one unless chosen; known
                    // by its name until its box joins.
                    None => {
                        if candidate.network.named(&site.name).next().is_some() {
                            self.set_error(
                                "name",
                                format!(
                                    "\"{}\" is already a site of the network (a site added by hand is known by its name; a box that joins with an invitation may share it)",
                                    site.name
                                ),
                            );
                            return None;
                        }
                        candidate.network.sites.push(Site {
                            changes: if uplink.is_some() {
                                changes
                            } else {
                                Changes::default()
                            },
                            uplink,
                            own_vlans,
                            ..site
                        });
                    }
                }
            }
            FormKind::Map(old) => {
                let index = match &old {
                    Some(m) => match candidate.network.maps.iter().position(|x| x == m) {
                        Some(i) => Some(i),
                        None => {
                            return self.gone(&format!(
                                "The mapping of {} (source VLAN {} = mapped VLAN {})",
                                draft.network.label(m.site),
                                m.shared,
                                m.local
                            ));
                        }
                    },
                    None => None,
                };
                let map = self.parse_map(draft)?;
                match index.and_then(|i| candidate.network.maps.get_mut(i)) {
                    Some(slot) => *slot = map,
                    None => candidate.network.maps.push(map),
                }
            }
            FormKind::OwnPorts(id) => {
                let Some(i) = Self::site_index(draft, id) else {
                    return self.gone(&format!("Site {}", draft.network.label(id)));
                };
                let control = self.parse_opt_port("control_port", u16::MAX);
                let tunnel = self.parse_opt_port("tunnel_port", u16::MAX - 1);
                let ports = &mut candidate.network.sites.get_mut(i)?.ports;
                ports.control = control?;
                ports.tunnel = tunnel?;
            }
            FormKind::Vlans => {
                let vlans = self.parse_vlans()?;
                candidate.network.vlans = self.guard_network_vlans(draft, vlans)?;
            }
            FormKind::Invite => self.build_invite(&mut candidate)?,
            FormKind::OwnVlans(id) => {
                let Some(i) = Self::site_index(draft, id) else {
                    return self.gone(&format!("Site {}", draft.network.label(id)));
                };
                let own = self.parse_own_vlans()?;
                candidate.network.sites.get_mut(i)?.own_vlans = own;
            }
            FormKind::Rename(id) => {
                let Some(site) = draft.network.site(id) else {
                    return self.gone(&format!("Site {}", draft.network.label(id)));
                };
                let new = self.value("name").trim().to_string();
                if let Some(msg) = site_name_error(&new) {
                    self.set_error("name", msg);
                    return None;
                }
                if new != site.name {
                    // Known by its name until its box has an identity: a
                    // name of its own.
                    if !site.has_identity() && candidate.network.named(&new).next().is_some() {
                        self.set_error(
                            "name",
                            format!(
                                "\"{new}\" is already a site of the network, and this one is known by its name until its box has an identity"
                            ),
                        );
                        return None;
                    }
                    candidate.rename_site(id, &new);
                }
            }
            FormKind::TunnelVlans(a, b) => {
                if let Some(gone) = [a, b]
                    .into_iter()
                    .find(|n| Self::site_index(draft, *n).is_none())
                {
                    return self.gone(&format!("Site {}", draft.network.label(gone)));
                }
                let text = self.value("vlans");
                let limit = match text.trim() {
                    "" | "all" => None,
                    "none" => Some(VlanSet::new()),
                    t => match t.parse::<VlanSet>() {
                        Ok(v) if v.is_all() => None,
                        Ok(v) => Some(v),
                        Err(e) => {
                            self.set_error("vlans", e.to_string());
                            return None;
                        }
                    },
                };
                candidate.network.set_tunnel_limit(a, b, limit);
            }
            // Only the fields of the form (see `Form::local`).
            FormKind::Local if self.field("self").is_some() => {
                let own = self.value("self");
                candidate.local.self_id = site_choice(&draft.network, &own);
            }
            FormKind::Local => {
                let timeout = self.parse_opt_duration("commit_timeout");
                let interval = self.parse_opt_duration("mtu_probe_interval");
                let mss = self.parse_mss();
                let local = &mut candidate.local;
                local.commit_timeout = timeout?;
                local.mtu_probe_interval = interval?;
                local.mss = mss?;
            }
            FormKind::Breakout(old) => {
                if let Some((site, vid)) = &old {
                    let still = draft
                        .network
                        .site(*site)
                        .is_some_and(|s| s.breakout.contains_key(vid));
                    if !still {
                        return self.gone(&format!(
                            "The breakout of VLAN {vid} at {}",
                            draft.network.label(*site)
                        ));
                    }
                }
                let Some(site) = self.breakout_site(draft) else {
                    self.set_error("site", "the network has no sites");
                    return None;
                };
                // "native": the untagged traffic (VLAN 1 in the network).
                let vlan = self.value("vlan");
                let vlan = if vlan.trim() == "native" {
                    "1".to_string()
                } else {
                    vlan
                };
                let vid = match parse_vid(&vlan) {
                    Ok(v) => Some(v),
                    Err(msg) => {
                        self.set_error("vlan", msg);
                        None
                    }
                };
                let uplink = self.value("uplink").trim().to_string();
                let uplink = (!uplink.is_empty() && uplink != "auto").then_some(uplink);
                let mut except = std::collections::BTreeSet::new();
                let text = self.value("except");
                for part in text.split([',', ' ']).filter(|p| !p.trim().is_empty()) {
                    match wanderlan_core::breakout::parse_net(part) {
                        Some(net) => {
                            except.insert(net);
                        }
                        None => {
                            self.set_error(
                                "except",
                                format!(
                                    "\"{}\" is not an address or a network like 203.0.113.0/24",
                                    part.trim()
                                ),
                            );
                            return None;
                        }
                    }
                }
                let vid = vid?;
                if let Some((old_site, old_vid)) = &old
                    && let Some(entry) = candidate.network.site_mut(*old_site)
                {
                    entry.breakout.remove(old_vid);
                }
                let label = candidate.network.label(site);
                let Some(entry) = candidate.network.site_mut(site) else {
                    self.set_error("site", "the network has no sites");
                    return None;
                };
                if old != Some((site, vid)) && entry.breakout.contains_key(&vid) {
                    self.set_error(
                        "vlan",
                        format!("VLAN {vid} of {label} breaks out already (Enter on it edits it)"),
                    );
                    return None;
                }
                entry
                    .breakout
                    .insert(vid, wanderlan_core::config::Breakout { uplink, except });
            }
        }
        candidate.revalidate();
        Some(candidate)
    }

    /// The site chosen in a breakout form ("marcel (client)" → marcel).
    pub fn breakout_site(&self, draft: &Draft) -> Option<SiteId> {
        site_choice(&draft.network, &self.value("site"))
    }

    /// The value of field `key` (trimmed).
    pub fn field_value(&self, key: &str) -> String {
        self.value(key)
    }

    /// Sets field `key` of text to `value` (a choice from a list), as if
    /// typed.
    pub fn set_field_value(&mut self, key: &str, value: &str, draft: &Draft) {
        if let Some(field) = self.fields.iter_mut().find(|f| f.key == key)
            && let Input::Text(text) = &mut field.input
        {
            text.set(value);
            field.touched = true;
        }
        self.revalidate(draft);
    }

    /// The network as it would be with the invited site (at an address no
    /// site has), to check the rules before the invitation is created.
    fn build_invite(&mut self, candidate: &mut Draft) -> Option<()> {
        let name = self.value("name");
        let name = name.trim();
        if !name.is_empty()
            && let Some(msg) = site_name_error(name)
        {
            self.set_error("name", msg);
        }
        let vlans_text = self.value("vlans");
        let vlans = if vlans_text.trim().is_empty() {
            None
        } else {
            match VlanSet::from_str(&vlans_text) {
                Ok(set) => Some(set),
                Err(e) => {
                    self.set_error(
                        "vlans",
                        format!("{e} (e.g. native,10-50; empty: the network's)"),
                    );
                    return None;
                }
            }
        };
        let native_text = self.value("native");
        let native = if native_text.trim().is_empty() {
            None
        } else {
            match parse_vid(native_text.trim()) {
                Ok(v) => Some(v),
                Err(msg) => {
                    self.set_error("native", msg);
                    return None;
                }
            }
        };
        let translations = match parse_translations(&self.value("translations")) {
            Ok(t) => t,
            Err(msg) => {
                self.set_error("translations", msg);
                return None;
            }
        };
        if self.fields.iter().any(|f| f.error.is_some()) {
            return None;
        }
        let site_name = if name.is_empty() {
            (1..)
                .map(|n| format!("new-site{n}"))
                .find(|n| candidate.network.named(n).next().is_none())
                .unwrap_or_default()
        } else {
            name.to_string()
        };
        let address = (1..=254u8)
            .map(|last| Ipv4Addr::new(192, 0, 2, last))
            .find(|a| candidate.network.sites.iter().all(|s| s.address != *a))
            .unwrap_or(Ipv4Addr::new(198, 51, 100, 1));
        // An identity of its own (the joining box brings one): it may share
        // a name.
        let id = (0..=u8::MAX)
            .map(|n| SiteId::from_bytes([n; 32]))
            .find(|id| candidate.network.site(*id).is_none())
            .unwrap_or(SiteId::from_bytes([0xa5; 32]));
        candidate.network.sites.push(Site {
            id,
            vlans,
            online: false,
            changes: self.invite_rules().changes(),
            uplink: candidate.local.self_id,
            ..Site::new(&site_name, address)
        });
        if let Some(shared) = native {
            candidate.network.maps.push(VlanMap {
                site: id,
                local: LocalVid::Native,
                shared,
            });
        }
        for t in translations {
            candidate.network.maps.push(VlanMap {
                site: id,
                local: LocalVid::Vid(t.local),
                shared: t.shared,
            });
        }
        Some(())
    }

    fn parse_site(&mut self) -> Option<Site> {
        let name = self.value("name");
        let name_ok = match site_name_error(&name) {
            Some(msg) => {
                self.set_error("name", msg);
                false
            }
            None => true,
        };
        let vlans_text = self.value("vlans");
        let vlans = if vlans_text.trim().is_empty() {
            Some(None)
        } else {
            match VlanSet::from_str(&vlans_text) {
                Ok(set) => Some(Some(set)),
                Err(e) => {
                    self.set_error("vlans", format!("{e} (e.g. 10-50,60; empty for all)"));
                    None
                }
            }
        };
        let address = self.value("address");
        let address = match address.parse::<Ipv4Addr>() {
            Ok(ip) => Some(ip),
            Err(_) => {
                let msg = if address.is_empty() {
                    "address is required".to_string()
                } else {
                    format!("\"{address}\" is not an IPv4 address (e.g. 10.8.0.1)")
                };
                self.set_error("address", msg);
                None
            }
        };
        let address = address?;
        let vlans = vlans?;
        name_ok.then(|| Site {
            vlans,
            ..Site::new(&name, address)
        })
    }

    fn parse_map(&mut self, draft: &Draft) -> Option<VlanMap> {
        // The choice reads "marcel (client)".
        let site = site_choice(&draft.network, &self.value("site"));
        if site.is_none() {
            self.set_error("site", "the network has no sites");
        }
        let local_text = self.value("local");
        let local = if local_text.eq_ignore_ascii_case("native") {
            Some(LocalVid::Native)
        } else {
            match parse_vid(&local_text) {
                Ok(v) => Some(LocalVid::Vid(v)),
                Err(msg) => {
                    self.set_error("local", format!("\"native\" or {msg}"));
                    None
                }
            }
        };
        let shared = match parse_vid(&self.value("shared")) {
            Ok(v) => Some(v),
            Err(msg) => {
                self.set_error("shared", msg);
                None
            }
        };
        Some(VlanMap {
            site: site?,
            local: local?,
            shared: shared?,
        })
    }

    /// The VLANs of the network; empty means all VLANs and untagged.
    /// The network's VLANs may only change on a site that is not restricted
    /// (see [`policy`]).
    fn guard_network_vlans(&mut self, draft: &Draft, vlans: VlanSet) -> Option<VlanSet> {
        let me = draft
            .local
            .self_id
            .unwrap_or_else(|| SiteId::provisional(""));
        if vlans != draft.network.vlans && !policy::may_change_network_vlans(&draft.network, me) {
            self.set_error("vlans", policy::why_not_network(&draft.network, me));
            return None;
        }
        Some(vlans)
    }

    /// The own VLANs field: empty or "none" for none.
    fn parse_own_vlans(&mut self) -> Option<Option<VlanSet>> {
        let text = self.value("own_vlans");
        let text = text.trim();
        if text.is_empty() || text == "none" {
            return Some(None);
        }
        match VlanSet::from_str(text) {
            Ok(set) => Some(Some(set)),
            Err(e) => {
                self.set_error("own_vlans", format!("{e} (e.g. 50,60-70; empty for none)"));
                None
            }
        }
    }

    fn parse_vlans(&mut self) -> Option<VlanSet> {
        let text = self.value("vlans");
        if text.trim().is_empty() {
            return Some(VlanSet::all());
        }
        match VlanSet::from_str(&text) {
            Ok(set) => Some(set),
            Err(e) => {
                self.set_error("vlans", e.to_string());
                None
            }
        }
    }

    /// Empty: `None` (the network's default).
    fn parse_opt_port(&mut self, key: &str, max: u16) -> Option<Option<u16>> {
        let text = self.value(key);
        if text.is_empty() {
            return Some(None);
        }
        match text.parse::<u16>() {
            Ok(p) if (1..=max).contains(&p) => Some(Some(p)),
            _ => {
                self.set_error(key, format!("empty or a port number between 1 and {max}"));
                None
            }
        }
    }

    /// Empty: `None` (the network's default).
    fn parse_opt_duration(&mut self, key: &str) -> Option<Option<Duration>> {
        let text = self.value(key);
        if text.is_empty() {
            return Some(None);
        }
        match humantime::parse_duration(&text) {
            Ok(d) => Some(Some(d)),
            Err(_) => {
                self.set_error(key, format!("\"{text}\" is not a duration like 60s or 5m"));
                None
            }
        }
    }

    fn parse_mss(&mut self) -> Option<Option<u16>> {
        let text = self.value("mss");
        if text.is_empty() {
            return Some(None);
        }
        match text.parse::<u16>() {
            Ok(v) => Some(Some(v)),
            Err(_) => {
                self.set_error("mss", "empty or a number between 536 and 1460");
                None
            }
        }
    }
}

fn vlans_field(vlans: &VlanSet) -> Field {
    let value = if vlans.is_all() {
        String::new()
    } else {
        vlans.to_string()
    };
    Field::text(
        "vlans",
        "VLANs",
        "empty: all VLANs and untagged; or e.g. native,10-50",
        value,
    )
}

/// Parses `130:30, 140:40`.
fn parse_translations(text: &str) -> Result<Vec<Translation>, String> {
    text.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| {
            let (local, shared) = t
                .split_once(':')
                .ok_or_else(|| format!("\"{t}\" is not mapped:source, e.g. 130:30"))?;
            Ok(Translation {
                local: parse_vid(local.trim())?,
                shared: parse_vid(shared.trim())?,
            })
        })
        .collect()
}

fn parse_vid(text: &str) -> Result<u16, String> {
    text.parse::<u16>()
        .ok()
        .filter(|v| (VID_MIN..=VID_MAX).contains(v))
        .ok_or_else(|| format!("a VLAN ID between {VID_MIN} and {VID_MAX}"))
}

/// Interfaces that can serve as trunk: no loopback, nothing wanderlan
/// created; sorted by name.
pub fn trunk_candidates(interfaces: &[Interface]) -> Vec<&Interface> {
    let mut list: Vec<&Interface> = interfaces.iter().filter(|i| i.is_listed()).collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

/// The own VLANs of a site as a form field.
fn own_vlans_field(site: Option<&Site>) -> Field {
    Field::text(
        "own_vlans",
        "Own VLANs",
        "its trunk and its own clients only, never its server; empty: none",
        site.and_then(|s| s.own_vlans.as_ref())
            .map(ToString::to_string)
            .unwrap_or_default(),
    )
}

/// The "Server" choice of a server.
const NO_SERVER: &str = "none (a server)";

/// The site of a choice: its label (`c1`, `c1#Ab3dK9xQ`), maybe with its
/// kind (`marcel (client)`).
fn site_choice(network: &Network, value: &str) -> Option<SiteId> {
    let label = value.split(" (").next().unwrap_or_default().trim();
    network.find(label).ok().map(|s| s.id)
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::tui::draft::Running;
    use crate::tui::fixtures;
    use crate::tui::fixtures::id;

    fn draft() -> Draft {
        Draft::from_running(&Running::parse(&fixtures::config_texts()).unwrap()).unwrap()
    }

    fn type_text(form: &mut Form, draft: &Draft, text: &str) {
        for c in text.chars() {
            let ev = form.handle_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            if ev == FormEvent::Changed {
                form.revalidate(draft);
            }
        }
    }

    fn press(form: &mut Form, draft: &Draft, code: KeyCode) -> FormEvent {
        let ev = form.handle_key(&KeyEvent::new(code, KeyModifiers::NONE));
        if ev == FormEvent::Changed {
            form.revalidate(draft);
        }
        ev
    }

    #[test]
    fn a_new_site_starts_empty() {
        let d = draft();
        let form = Form::site(&d, None);
        assert_eq!(
            form.fields.iter().map(|f| f.key).collect::<Vec<_>>(),
            ["name", "address", "vlans", "server", "changes", "own_vlans"]
        );
        assert_eq!(
            form.value("changes"),
            "only the server",
            "like an invitation"
        );
        assert_eq!(form.value("server"), "heim", "a client of this site");
        // Empty required fields are errors, but hidden until touched.
        let name = form.field("name").unwrap();
        assert!(name.error.is_some());
        assert_eq!(name.visible_error(false), None);
        assert!(
            form.field("vlans").unwrap().error.is_none(),
            "empty: all VLANs"
        );
        assert!(!form.is_valid());
    }

    #[test]
    fn the_changes_field_sets_who_changes_the_tunnel() {
        let d = draft();
        let haus6 = d
            .network
            .sites
            .iter()
            .position(|s| s.name == "haus6")
            .unwrap();
        let mut form = Form::site(&d, Some(haus6));
        assert_eq!(form.value("changes"), "only the server");
        assert_eq!(
            form.field("changes").map(|f| match &f.input {
                Input::Choice { options, .. } => options.clone(),
                _ => Vec::new(),
            }),
            Some(vec![
                "only the server".to_string(),
                "only the client".to_string(),
                "both".to_string()
            ])
        );
        form.focus_on("changes");
        press(&mut form, &d, KeyCode::Right);
        let candidate = form.submit(&d).unwrap();
        assert_eq!(
            candidate.network.by_name("haus6").unwrap().changes,
            Changes::Client
        );
        // A new site: only its server, like an invitation.
        let mut form = Form::site(&d, None);
        type_text(&mut form, &d, "neu");
        press(&mut form, &d, KeyCode::Tab);
        type_text(&mut form, &d, "10.8.0.99");
        let candidate = form.submit(&d).unwrap();
        let neu = candidate.network.by_name("neu").unwrap();
        assert_eq!(neu.uplink, Some(id("heim")));
        assert_eq!(neu.changes, Changes::Server);

        // Its own ports and its breakouts (as a client) stay; the server can
        // be chosen.
        let mut with = d.clone();
        let i = with
            .network
            .sites
            .iter()
            .position(|s| s.name == "haus3")
            .unwrap();
        with.network.sites[i].uplink = Some(id("heim"));
        with.network.sites[i].ports.control = Some(52821);
        with.network.sites[i]
            .breakout
            .insert(20, wanderlan_core::config::Breakout::default());
        let mut form = Form::site(&with, Some(i));
        form.focus_on("vlans");
        type_text(&mut form, &with, "20");
        let edited = form.submit(&with).unwrap();
        let haus3 = edited.network.by_name("haus3").unwrap();
        assert_eq!(haus3.ports.control, Some(52821));
        assert!(haus3.breakout.contains_key(&20));
        assert_eq!(haus3.uplink, with.network.sites[i].uplink, "untouched");
    }

    #[test]
    fn a_new_site_is_a_client_of_this_site_and_an_edit_keeps_the_role() {
        let d = draft();
        let haus6 = d
            .network
            .sites
            .iter()
            .position(|s| s.name == "haus6")
            .unwrap();
        assert!(Form::site(&d, Some(haus6)).field("uplink").is_none());
        // Editing keeps the role: haus6 a client of heim, buero a server.
        for (index, uplink) in [(haus6, Some("heim")), (1, None)] {
            let mut form = Form::site(&d, Some(index));
            let name = d.network.sites[index].name.clone();
            let candidate = form.submit(&d).unwrap();
            assert_eq!(
                candidate.network.by_name(&name).unwrap().uplink,
                uplink.map(id)
            );
        }
        // A new site is a client of this site.
        let mut form = Form::site(&d, None);
        type_text(&mut form, &d, "neu");
        press(&mut form, &d, KeyCode::Tab);
        type_text(&mut form, &d, "10.8.0.99");
        let candidate = form.submit(&d).unwrap();
        assert_eq!(
            candidate.network.by_name("neu").unwrap().uplink,
            Some(id("heim"))
        );
    }

    #[test]
    fn duplicate_name_is_marked_on_the_name_field() {
        let d = draft();
        let mut form = Form::site(&d, None);
        type_text(&mut form, &d, "buero");
        press(&mut form, &d, KeyCode::Tab);
        type_text(&mut form, &d, "10.8.0.20");
        let name = form.field("name").unwrap();
        assert!(
            name.visible_error(false)
                .unwrap()
                .contains("already a site of the network"),
            "{name:?}"
        );
        assert!(form.submit(&d).is_none());
        assert_eq!(form.focus, 0, "focus moves to the first invalid field");
    }

    #[test]
    fn adding_a_valid_site() {
        let d = draft();
        let mut form = Form::site(&d, None);
        type_text(&mut form, &d, "haus4");
        form.focus = 1;
        type_text(&mut form, &d, "10.8.0.14");
        let new = form.submit(&d).expect("valid");
        let site = new.network.by_name("haus4").unwrap();
        assert_eq!(site.vlans, None);
        assert_eq!(new.errors(), 0);
    }

    #[test]
    fn site_vlans_are_parsed_and_checked_against_mappings() {
        let d = draft();
        // haus3 maps native traffic to VLAN 20 and local 130 to 30.
        let index = d.network.sites.iter().position(|s| s.name == "haus3");
        let mut form = Form::site(&d, index);
        form.focus = 2;
        type_text(&mut form, &d, "x");
        assert!(
            form.field("vlans")
                .unwrap()
                .visible_error(false)
                .is_some_and(|e| e.contains("not a VLAN ID")),
            "{:?}",
            form.field("vlans")
        );
        press(&mut form, &d, KeyCode::Backspace);
        type_text(&mut form, &d, "30");
        assert!(form.field("vlans").unwrap().error.is_none());
        assert!(
            form.notes
                .iter()
                .any(|n| n.message.contains("source VLAN 20 is not one of the VLANs")),
            "{:?}",
            form.notes
        );
    }

    #[test]
    fn invalid_address_and_vlan_mapping_conflict() {
        let d = draft();
        let mut form = Form::site(&d, Some(1));
        form.focus = 1;
        press(&mut form, &d, KeyCode::Backspace);
        type_text(&mut form, &d, "x");
        assert!(
            form.field("address")
                .unwrap()
                .visible_error(false)
                .unwrap()
                .contains("not an IPv4 address")
        );

        // A second native mapping for haus3 conflicts with map #1.
        let mut map = Form::map(&d, None, Some(id("haus3")));
        map.focus = 1;
        type_text(&mut map, &d, "40");
        let local = map.field("local").unwrap();
        assert!(
            local
                .error
                .as_deref()
                .unwrap()
                .contains("more than one native mapping"),
            "{local:?}"
        );
    }

    #[test]
    fn the_settings_of_this_host_parse_and_validate() {
        let d = draft();
        let mut form = Form::local(&d, "mss");
        form.focus = form.fields.iter().position(|f| f.key == "mss").unwrap();
        type_text(&mut form, &d, "100");
        let mss = form.field("mss").unwrap();
        assert!(
            mss.error
                .as_deref()
                .unwrap()
                .contains("between 536 and 1460")
        );
        press(&mut form, &d, KeyCode::Backspace);
        press(&mut form, &d, KeyCode::Backspace);
        press(&mut form, &d, KeyCode::Backspace);
        type_text(&mut form, &d, "1300");
        let new = form.submit(&d).expect("valid");
        assert_eq!(new.local.mss, Some(1300));
        assert_eq!(new.network, d.network, "nothing of the other sites");
        // The ports: only the own entry.
        let mut form = Form::own_ports(&d);
        type_text(&mut form, &d, "52821");
        let new = form.submit(&d).expect("valid");
        let me = d.local.self_id.unwrap();
        assert_eq!(new.network.control_port_of(me), 52821);
        assert_eq!(new.network.control_port, d.network.control_port);
    }

    #[test]
    fn shrinking_vlans_reports_broken_mappings() {
        let d = draft();
        let mut form = Form::vlans(&d);
        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(form.handle_key(&ctrl_u), FormEvent::Changed);
        type_text(&mut form, &d, "10-25");
        assert!(form.is_valid());
        assert!(
            form.notes
                .iter()
                .any(|n| n.message.contains("source VLAN 30 is not one of the VLANs")),
            "{:?}",
            form.notes
        );
    }

    #[test]
    fn the_local_form_chooses_the_site_and_this_host_s_settings() {
        let d = draft();
        let mut form = Form::local(&d, "self");
        assert_eq!(
            form.fields.iter().map(|f| f.key).collect::<Vec<_>>(),
            ["self"],
            "the identity has a form of its own, the trunk a window"
        );
        assert_eq!(form.value("self"), "heim");
        form.handle_key(&KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let saved = form.submit(&d).expect("valid");
        assert_ne!(saved.local.self_id, Some(id("heim")));
        assert_eq!(saved.local.trunk, d.local.trunk, "the trunk is kept");
        let form = Form::local(&d, "commit_timeout");
        assert_eq!(
            form.fields.iter().map(|f| f.key).collect::<Vec<_>>(),
            ["commit_timeout", "mtu_probe_interval", "mss"]
        );
    }
}
