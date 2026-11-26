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
    Diagnostic, FieldPath, LocalVid, Severity, Site, VID_MAX, VID_MIN, VlanMap, VlanSet,
    site_name_error,
};

use wanderlan_core::policy;

use super::draft::{Draft, is_within};
use super::format;
use super::input::{Outcome, TextInput};

/// What a form edits, by name (or content): a sync or a rollback may
/// replace the draft while the form is open, the entry is found again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormKind {
    /// A site; `None` adds a new one.
    Site(Option<String>),
    /// A VLAN mapping (as it was); `None` adds a new one.
    Map(Option<VlanMap>),
    /// The `[network]` settings.
    Network,
    /// Only the shared VLANs.
    Vlans,
    /// `local.toml`.
    Local,
    /// Rules for a new invitation (the draft is not changed).
    Invite,
    /// The own VLANs of one site: on its trunk and towards its own clients
    /// only.
    OwnVlans(String),
    /// The VLANs of the tunnel between two sites.
    TunnelVlans(String, String),
    /// A new name for a site; everything that names it follows.
    Rename(String),
    /// The local breakout of a VLAN of a site (as it was: site and VLAN);
    /// `None` adds one.
    Breakout(Option<(String, u16)>),
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
    candidate: Option<Draft>,
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
        let title = match existing {
            Some(s) => format!("Edit site {}", s.name),
            None => "New site".to_string(),
        };
        let me = draft.local.self_name.clone().unwrap_or_default();
        let (choices, current) = match existing {
            Some(s) => {
                let manager = policy::manager(&draft.network, s).map(str::to_string);
                let choices = changes_choices(manager.as_deref().unwrap_or(&me), &s.name);
                // Without a manager yet: this site from now on.
                let current = match (manager, s.self_managed) {
                    (None, _) => format!("only {me}"),
                    (Some(m), false) => format!("only {m}"),
                    (Some(m), true) => format!("{m} and {}", s.name),
                };
                (choices, current)
            }
            None => (changes_choices(&me, "the new site"), format!("only {me}")),
        };
        let fields = vec![
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
                "changes",
                "Changes",
                "who may change its VLANs, mappings and online state",
                choices,
                &current,
            ),
            own_vlans_field(existing),
        ];
        let name = existing.map(|s| s.name.clone());
        Self::new(FormKind::Site(name), title, fields, draft)
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
                "Untagged",
                "empty: none; or the source VLAN for untagged traffic",
                "",
            ),
            Field::text(
                "translations",
                "Translations",
                "mapped:source, e.g. 130:30, 140:40",
                "",
            ),
            Field::choice(
                "self_managed",
                "Changes",
                "who may change the VLANs of the new site later",
                vec!["only this site".into(), "this site and the new site".into()],
                "only this site",
            ),
        ];
        let title = match draft.local.self_name.as_deref() {
            Some(me) => format!("Invite a new site (it becomes a client of {me})"),
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
            allow_self: self.value("self_managed") != "only this site",
        }
    }

    /// Moves the focus to the field `key` (Settings view).
    pub fn focus_on(&mut self, key: &str) {
        if let Some(i) = self.fields.iter().position(|f| f.key == key) {
            self.focus = i;
        }
    }

    /// Form for a new VLAN mapping (`None`) or an existing one.
    pub fn map(draft: &Draft, index: Option<usize>, site_hint: Option<&str>) -> Self {
        let existing = index.and_then(|i| draft.network.maps.get(i));
        // "marcel (client)": where the mapped traffic comes from.
        let sites: Vec<String> = draft
            .network
            .sites
            .iter()
            .map(format::site_with_kind)
            .collect();
        let site = existing
            .map(|m| m.site.clone())
            .or_else(|| site_hint.map(str::to_string))
            .or_else(|| draft.local.self_name.clone())
            .unwrap_or_default();
        let site = draft
            .network
            .site(&site)
            .map_or(site, format::site_with_kind);
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
    /// new one (at `site_hint`). `uplinks` names the uplinks of this box.
    pub fn breakout(
        draft: &Draft,
        existing: Option<(String, u16)>,
        site_hint: Option<&str>,
        uplinks: &str,
    ) -> Self {
        let sites: Vec<String> = draft
            .network
            .sites
            .iter()
            .map(format::site_with_kind)
            .collect();
        let site = existing
            .as_ref()
            .map(|(s, _)| s.clone())
            .or_else(|| site_hint.map(str::to_string))
            .or_else(|| draft.local.self_name.clone())
            .unwrap_or_default();
        let entry = existing.as_ref().and_then(|(s, vid)| {
            draft
                .network
                .site(s)
                .and_then(|site| site.breakout.get(vid))
        });
        let shown = draft
            .network
            .site(&site)
            .map_or(site.clone(), format::site_with_kind);
        let vid = existing
            .as_ref()
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
        let uplink = entry.and_then(|b| b.uplink.clone()).unwrap_or_default();
        let except: Vec<String> = entry
            .map(|b| b.except.iter().map(ToString::to_string).collect())
            .unwrap_or_default();
        let uplink_hint = if uplinks.is_empty() {
            "empty: the interface of the site's default route; or e.g. ens20".to_string()
        } else {
            format!("empty: the default route; at this box: {uplinks}")
        };
        let title = match &existing {
            Some((s, v)) => format!("Breakout of VLAN {v} at {s}"),
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
                "the VLAN in the network (untagged traffic: 1, or its mapped source VLAN)",
                vid,
            ),
            Field::text("uplink", "Uplink", uplink_hint, uplink),
            Field::text(
                "except",
                "Except",
                "public destinations that still go through the tunnel, e.g. 203.0.113.0/24, 2001:db8::/32",
                except.join(", "),
            ),
        ];
        Self::new(FormKind::Breakout(existing), title, fields, draft)
    }

    /// Form for the `[network]` settings.
    pub fn network(draft: &Draft) -> Self {
        let m = &draft.network;
        let fields = vec![
            vlans_field(&m.vlans),
            Field::text(
                "control_port",
                "Control port",
                "UDP/TCP port of the daemons on the site addresses",
                m.control_port.to_string(),
            ),
            Field::text(
                "tunnel_port",
                "Tunnel port",
                "UDP port of the VXLAN tunnels (NAT also the one above); empty: control port + 1",
                m.tunnel_port.map(|p| p.to_string()).unwrap_or_default(),
            ),
            Field::text(
                "mtu_probe_interval",
                "MTU probe every",
                "how often the path MTU is measured, e.g. 5m",
                format::duration(m.mtu_probe_interval),
            ),
            Field::text(
                "commit_timeout",
                "Commit timeout",
                "time to confirm a change before it is rolled back, e.g. 60s",
                format::duration(m.commit_timeout),
            ),
            Field::text(
                "mss",
                "Fixed MSS",
                "empty: derive from the measured path MTU; or 536-1460",
                m.mss.map(|v| v.to_string()).unwrap_or_default(),
            ),
        ];
        Self::new(
            FormKind::Network,
            "Network settings (all sites)",
            fields,
            draft,
        )
    }

    /// Form for the own VLANs of the site `site` (index in the network).
    pub fn own_vlans(draft: &Draft, site: usize) -> Self {
        let s = draft.network.sites.get(site);
        let name = s.map(|s| s.name.clone()).unwrap_or_default();
        let fields = vec![own_vlans_field(s)];
        Self::new(
            FormKind::OwnVlans(name.clone()),
            format!("Own VLANs of {name}"),
            fields,
            draft,
        )
    }

    /// Form for a new name of a site.
    pub fn rename(draft: &Draft, site: usize) -> Self {
        let name = draft
            .network
            .sites
            .get(site)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let fields = vec![Field::text(
            "name",
            "New name",
            "a-z, 0-9 and -, at most 12 characters; its address stays, the tunnels to it are built anew",
            name.clone(),
        )];
        Self::new(
            FormKind::Rename(name.clone()),
            format!("Rename site {name}"),
            fields,
            draft,
        )
    }

    /// Form for the VLANs of the tunnel between the sites `a` and `b`.
    pub fn tunnel_vlans(draft: &Draft, a: &str, b: &str) -> Self {
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
        Self::new(
            FormKind::TunnelVlans(a.to_string(), b.to_string()),
            format!("VLANs of the tunnel {a} – {b}"),
            fields,
            draft,
        )
    }

    /// Form for the shared VLANs only.
    pub fn vlans(draft: &Draft) -> Self {
        let fields = vec![vlans_field(&draft.network.vlans)];
        Self::new(FormKind::Vlans, "VLANs of the network", fields, draft)
    }

    /// Form for `local.toml`: which site this host is. The trunk ports
    /// have a window of their own (`ports.rs`).
    pub fn local(draft: &Draft) -> Self {
        let sites: Vec<String> = draft.network.sites.iter().map(|s| s.name.clone()).collect();
        let own = draft.local.self_name.clone().unwrap_or_default();
        let fields = vec![Field::choice(
            "self",
            "This site",
            "which site of the network this host is",
            sites,
            &own,
        )];
        Self::new(FormKind::Local, "This site (local.toml)", fields, draft)
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
            KeyCode::Enter => return FormEvent::Submit,
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
        self.candidate = self.build(draft);
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
            self.candidate.take()
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
    fn site_index(draft: &Draft, name: &str) -> Option<usize> {
        draft.network.sites.iter().position(|s| s.name == name)
    }

    /// Index of the edited entity in the candidate draft.
    fn index(&self, draft: &Draft) -> Option<usize> {
        match &self.kind {
            FormKind::Site(None) | FormKind::Invite => Some(draft.network.sites.len()),
            FormKind::Site(Some(name)) | FormKind::OwnVlans(name) | FormKind::Rename(name) => {
                Self::site_index(draft, name)
            }
            FormKind::Breakout(_) => Self::site_index(draft, &self.breakout_site()),
            FormKind::Map(None) => Some(draft.network.maps.len()),
            FormKind::Map(Some(map)) => draft.network.maps.iter().position(|m| m == map),
            FormKind::TunnelVlans(a, b) => {
                let (a, b) = (a.as_str(), b.as_str());
                Some(
                    draft
                        .network
                        .tunnels
                        .iter()
                        .position(|t| t.is(a, b))
                        .unwrap_or(draft.network.tunnels.len()),
                )
            }
            FormKind::Network | FormKind::Vlans | FormKind::Local => None,
        }
    }

    fn entity_path(&self, draft: &Draft) -> Option<FieldPath> {
        let i = self.index(draft)?;
        match &self.kind {
            FormKind::Site(_) | FormKind::Invite | FormKind::OwnVlans(_) | FormKind::Rename(_) => {
                Some(FieldPath::site(i))
            }
            FormKind::Map(_) => Some(FieldPath::map(i)),
            FormKind::TunnelVlans(..) => Some(FieldPath::tunnel(i)),
            FormKind::Breakout(_) => Some(FieldPath::site_field(i, "breakout")),
            FormKind::Network | FormKind::Vlans | FormKind::Local => None,
        }
    }

    /// Path of a field in the candidate draft.
    fn path_of(&self, key: &str, draft: &Draft) -> FieldPath {
        let i = self.index(draft).unwrap_or_default();
        match &self.kind {
            FormKind::Site(_) | FormKind::Invite | FormKind::OwnVlans(_) | FormKind::Rename(_) => {
                FieldPath::site_field(i, key)
            }
            FormKind::Map(_) => FieldPath::map_field(i, key),
            FormKind::TunnelVlans(..) => FieldPath::tunnel_field(i, key),
            FormKind::Breakout(_) => {
                let vid = self.value("vlan").trim().to_string();
                match key {
                    "vlan" => FieldPath::site_field(i, &format!("breakout.{vid}")),
                    "site" => FieldPath::site_field(i, "name"),
                    _ => FieldPath::site_field(i, &format!("breakout.{vid}.{key}")),
                }
            }
            FormKind::Network | FormKind::Vlans => FieldPath::network(key),
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
            FormKind::Site(name) => {
                let index = match &name {
                    Some(n) => match Self::site_index(draft, n) {
                        Some(i) => Some(i),
                        None => return self.gone(&format!("Site {n}")),
                    },
                    None => None,
                };
                let site = self.parse_site()?;
                // Who may change it changes only when the field was changed.
                let changes_touched = self.field("changes").is_some_and(|f| f.touched);
                let (managed_by, self_managed) = parse_changes(&self.value("changes"));
                let own_vlans = self.parse_own_vlans()?;
                if let (Some(old), Some(me)) = (&name, draft.local.self_name.as_deref())
                    && *old != site.name
                    && !policy::may_rename(&draft.network, old, me)
                {
                    self.set_error("name", policy::why_not_rename(&draft.network, old, me));
                    return None;
                }
                match index.and_then(|i| candidate.network.sites.get_mut(i)) {
                    Some(slot) => {
                        let mut site = site;
                        site.online = slot.online;
                        if changes_touched {
                            site.managed_by = managed_by;
                            site.self_managed = self_managed;
                        } else {
                            site.managed_by = slot.managed_by.clone();
                            site.self_managed = slot.self_managed;
                        }
                        // Its role stays (`wanderlan role` changes it).
                        site.uplink = slot.uplink.clone();
                        // Its identity is the box's, never edited here.
                        site.id = slot.id;
                        site.own_vlans = own_vlans;
                        let old = std::mem::replace(slot, site.clone());
                        if old.name != site.name {
                            candidate.rename_references(&old.name, &site.name);
                        }
                    }
                    // A new site is a client of this one.
                    None => candidate.network.sites.push(Site {
                        managed_by,
                        self_managed,
                        uplink: candidate.local.self_name.clone(),
                        own_vlans,
                        ..site
                    }),
                }
            }
            FormKind::Map(old) => {
                let index = match &old {
                    Some(m) => match candidate.network.maps.iter().position(|x| x == m) {
                        Some(i) => Some(i),
                        None => {
                            return self.gone(&format!(
                                "The mapping {} {} = shared {}",
                                m.site, m.local, m.shared
                            ));
                        }
                    },
                    None => None,
                };
                let map = self.parse_map()?;
                match index.and_then(|i| candidate.network.maps.get_mut(i)) {
                    Some(slot) => *slot = map,
                    None => candidate.network.maps.push(map),
                }
            }
            FormKind::Network => {
                let vlans = self.parse_vlans();
                let port = self.parse_port();
                let tunnel_port = self.parse_tunnel_port();
                let interval = self.parse_duration("mtu_probe_interval");
                let timeout = self.parse_duration("commit_timeout");
                let mss = self.parse_mss();
                let vlans = vlans.and_then(|v| self.guard_network_vlans(draft, v));
                let network = &mut candidate.network;
                network.vlans = vlans?;
                network.control_port = port?;
                network.tunnel_port = tunnel_port?;
                network.mtu_probe_interval = interval?;
                network.commit_timeout = timeout?;
                network.mss = mss?;
            }
            FormKind::Vlans => {
                let vlans = self.parse_vlans()?;
                candidate.network.vlans = self.guard_network_vlans(draft, vlans)?;
            }
            FormKind::Invite => self.build_invite(&mut candidate)?,
            FormKind::OwnVlans(name) => {
                let Some(i) = Self::site_index(draft, &name) else {
                    return self.gone(&format!("Site {name}"));
                };
                let own = self.parse_own_vlans()?;
                candidate.network.sites.get_mut(i)?.own_vlans = own;
            }
            FormKind::Rename(old) => {
                if Self::site_index(draft, &old).is_none() {
                    return self.gone(&format!("Site {old}"));
                }
                let new = self.value("name").trim().to_string();
                if let Some(msg) = site_name_error(&new) {
                    self.set_error("name", msg);
                    return None;
                }
                if new != old {
                    if candidate.network.site(&new).is_some() {
                        self.set_error(
                            "name",
                            format!("\"{new}\" is already a site of the network"),
                        );
                        return None;
                    }
                    candidate.rename_references(&old, &new);
                }
            }
            FormKind::TunnelVlans(a, b) => {
                if let Some(gone) = [&a, &b]
                    .into_iter()
                    .find(|n| Self::site_index(draft, n).is_none())
                {
                    return self.gone(&format!("Site {gone}"));
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
                candidate.network.set_tunnel_limit(&a, &b, limit);
            }
            FormKind::Local => {
                let own = self.value("self");
                candidate.local.self_name = (!own.is_empty()).then_some(own);
            }
            FormKind::Breakout(old) => {
                if let Some((site, vid)) = &old {
                    let still = draft
                        .network
                        .site(site)
                        .is_some_and(|s| s.breakout.contains_key(vid));
                    if !still {
                        return self.gone(&format!("The breakout of VLAN {vid} at {site}"));
                    }
                }
                let site = self.breakout_site();
                let vid = match parse_vid(&self.value("vlan")) {
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
                    && let Some(entry) = candidate
                        .network
                        .sites
                        .iter_mut()
                        .find(|s| s.name == *old_site)
                {
                    entry.breakout.remove(old_vid);
                }
                let Some(entry) = candidate.network.sites.iter_mut().find(|s| s.name == site)
                else {
                    self.set_error("site", "the network has no sites");
                    return None;
                };
                if old.as_ref() != Some(&(site.clone(), vid)) && entry.breakout.contains_key(&vid) {
                    self.set_error(
                        "vlan",
                        format!("VLAN {vid} of {site} breaks out already (Enter on it edits it)"),
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

    /// The site chosen in a breakout form ("marcel (client)" → "marcel").
    fn breakout_site(&self) -> String {
        self.value("site")
            .split(" (")
            .next()
            .unwrap_or_default()
            .to_string()
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
                .find(|n| candidate.network.site(n).is_none())
                .unwrap_or_default()
        } else {
            name.to_string()
        };
        let address = (1..=254u8)
            .map(|last| Ipv4Addr::new(192, 0, 2, last))
            .find(|a| candidate.network.sites.iter().all(|s| s.address != *a))
            .unwrap_or(Ipv4Addr::new(198, 51, 100, 1));
        candidate.network.sites.push(Site {
            name: site_name.clone(),
            address,
            id: None,
            vlans,
            own_vlans: None,
            online: false,
            managed_by: candidate.local.self_name.clone(),
            uplink: candidate.local.self_name.clone(),
            breakout: Default::default(),
            self_managed: false,
        });
        if let Some(shared) = native {
            candidate.network.maps.push(VlanMap {
                site: site_name.clone(),
                local: LocalVid::Native,
                shared,
            });
        }
        for t in translations {
            candidate.network.maps.push(VlanMap {
                site: site_name.clone(),
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
        name_ok.then_some(Site {
            name,
            address,
            id: None,
            vlans,
            own_vlans: None,
            online: true,
            managed_by: None,
            self_managed: false,
            uplink: None,
            breakout: Default::default(),
        })
    }

    fn parse_map(&mut self) -> Option<VlanMap> {
        // The choice reads "marcel (client)"; site names have no spaces.
        let site = self
            .value("site")
            .split(" (")
            .next()
            .unwrap_or_default()
            .to_string();
        if site.is_empty() {
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
        if site.is_empty() {
            return None;
        }
        Some(VlanMap {
            site,
            local: local?,
            shared: shared?,
        })
    }

    /// The VLANs of the network; empty means all VLANs and untagged.
    /// The network's VLANs may only change on a site that is not restricted
    /// (see [`policy`]).
    fn guard_network_vlans(&mut self, draft: &Draft, vlans: VlanSet) -> Option<VlanSet> {
        let me = draft.local.self_name.as_deref().unwrap_or_default();
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

    fn parse_port(&mut self) -> Option<u16> {
        match self.value("control_port").parse::<u16>() {
            Ok(p) if p > 0 => Some(p),
            _ => {
                self.set_error("control_port", "a port number between 1 and 65535");
                None
            }
        }
    }

    /// Empty: `None` (it follows the control port).
    fn parse_tunnel_port(&mut self) -> Option<Option<u16>> {
        let text = self.value("tunnel_port");
        if text.trim().is_empty() {
            return Some(None);
        }
        match text.trim().parse::<u16>() {
            Ok(p) if (1..u16::MAX).contains(&p) => Some(Some(p)),
            _ => {
                self.set_error("tunnel_port", "empty or a port number between 1 and 65534");
                None
            }
        }
    }

    fn parse_duration(&mut self, key: &str) -> Option<Duration> {
        let text = self.value(key);
        match humantime::parse_duration(&text) {
            Ok(d) => Some(d),
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

/// Choices of the "Changes" field of a site: only its manager, or the
/// manager and the site itself.
fn changes_choices(manager: &str, site: &str) -> Vec<String> {
    let mut choices = vec![format!("only {manager}")];
    if manager != site {
        choices.push(format!("{manager} and {site}"));
    }
    choices
}

/// `managed_by` and `self_managed` from a "Changes" choice.
fn parse_changes(value: &str) -> (Option<String>, bool) {
    if let Some(manager) = value.strip_prefix("only ") {
        return (Some(manager.to_string()), false);
    }
    match value.split_once(" and ") {
        Some((manager, _)) => (Some(manager.to_string()), true),
        None => (None, false),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::tui::draft::Running;
    use crate::tui::fixtures;

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
            ["name", "address", "vlans", "changes", "own_vlans"]
        );
        assert_eq!(form.value("changes"), "only heim", "managed by this site");
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
    fn the_changes_field_sets_who_manages_a_site() {
        let d = draft();
        let haus6 = d
            .network
            .sites
            .iter()
            .position(|s| s.name == "haus6")
            .unwrap();
        let form = Form::site(&d, Some(haus6));
        assert_eq!(form.value("changes"), "only buero");
        assert_eq!(
            changes_choices("buero", "haus6"),
            ["only buero", "buero and haus6"]
        );
        let heim = Form::site(&d, Some(0));
        assert_eq!(
            heim.value("changes"),
            "only heim",
            "no manager yet: this site"
        );
        assert_eq!(changes_choices("heim", "heim"), ["only heim"]);
        assert_eq!(parse_changes("only buero"), (Some("buero".into()), false));
        assert_eq!(
            parse_changes("buero and haus6"),
            (Some("buero".into()), true)
        );
        // A new site: managed by this site unless chosen otherwise.
        let mut form = Form::site(&d, None);
        type_text(&mut form, &d, "neu");
        press(&mut form, &d, KeyCode::Tab);
        type_text(&mut form, &d, "10.8.0.99");
        let candidate = form.submit(&d).unwrap();
        let neu = candidate.network.site("neu").unwrap();
        assert_eq!(neu.managed_by.as_deref(), Some("heim"));
        assert!(!neu.self_managed);
        // A site without a manager keeps none when only its VLANs change.
        let site = d
            .network
            .sites
            .iter()
            .position(|s| s.managed_by.is_none())
            .unwrap();
        let name = d.network.sites[site].name.clone();
        let mut form = Form::site(&d, Some(site));
        form.focus_on("vlans");
        type_text(&mut form, &d, "20");
        let edited = form.submit(&d).unwrap();
        assert_eq!(edited.network.site(&name).unwrap().managed_by, None);
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
                candidate.network.site(&name).unwrap().uplink.as_deref(),
                uplink
            );
        }
        // A new site is a client of this site.
        let mut form = Form::site(&d, None);
        type_text(&mut form, &d, "neu");
        press(&mut form, &d, KeyCode::Tab);
        type_text(&mut form, &d, "10.8.0.99");
        let candidate = form.submit(&d).unwrap();
        assert_eq!(
            candidate.network.site("neu").unwrap().uplink.as_deref(),
            Some("heim")
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
            name.visible_error(false).unwrap().contains("defined twice"),
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
        let site = new.network.site("haus4").unwrap();
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
        let mut map = Form::map(&d, None, Some("haus3"));
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
    fn network_settings_parse_and_validate() {
        let d = draft();
        let mut form = Form::network(&d);
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
        assert_eq!(new.network.mss, Some(1300));
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
    fn the_local_form_only_chooses_the_site() {
        let d = draft();
        let mut form = Form::local(&d);
        assert_eq!(form.fields.len(), 1, "the trunk has a window of its own");
        assert_eq!(form.value("self"), "heim");
        form.handle_key(&KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let saved = form.submit(&d).expect("valid");
        assert_ne!(saved.local.self_name.as_deref(), Some("heim"));
        assert_eq!(saved.local.trunk, d.local.trunk, "the trunk is kept");
    }
}
