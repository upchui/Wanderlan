//! Raw TOML structures with source spans and their conversion into the model.
//!
//! All keys are optional at this level so that missing values produce our own
//! messages with line numbers instead of generic serde errors. Unknown keys are
//! rejected to catch typos.
//!
//! `network.toml` comes in three formats with the same model:
//! * format 3, the one the daemon writes and the sites exchange:
//!   `[site."<identity>"]` tables with the site's `name` inside; every
//!   reference to a site (`server`, `tunnel_vlans`) is its
//!   identity, so several sites may have the same name. A site without a
//!   box yet has a placeholder ([`SiteId::provisional`]) that follows its
//!   name.
//! * format 2 (written by 1.0.21 and later): `[site.<name>]` tables with
//!   the mappings and tunnel limits inside the site; references are names.
//! * format 1: `[[site]]`, `[[map]]` and `[[tunnel]]` arrays, references
//!   are names.
//!
//! Names are for people: written by hand, a file may name a site by its
//! name anywhere (also as the key of a new site, next to tables named by
//! identities); older versions wrote formats 1 and 2. All of it is read,
//! and written back in format 3. A name no site of the file has stands for
//! its provisional identity, which keeps the name.

use std::net::Ipv4Addr;
use std::ops::Range;
use std::time::Duration;

use serde::Deserialize;
use serde_spanned::Spanned;

use super::diag::{ConfigFile, Diagnostic, FieldPath, SpanMap};
use super::model::{
    Breakout, Breakouts, Changes, DEFAULT_COMMIT_TIMEOUT, DEFAULT_CONTROL_PORT,
    DEFAULT_MTU_PROBE_INTERVAL, Local, LocalVid, Network, Site, SitePorts, TunnelVlans, VlanMap,
    site_pair,
};
use super::vlan::{VID_MAX, VID_MIN, VlanSet};
use crate::identity::SiteId;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetworkFile {
    /// `[network]`; files and sites of older versions call it `[mesh]`.
    #[serde(alias = "mesh")]
    network: Option<Spanned<RawNetworkSection>>,
    #[serde(default)]
    site: Vec<Spanned<RawSite>>,
    #[serde(default)]
    map: Vec<Spanned<RawMap>>,
    #[serde(default)]
    tunnel: Vec<Spanned<RawTunnel>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetworkSection {
    revision: Option<Spanned<i64>>,
    control_port: Option<Spanned<i64>>,
    tunnel_port: Option<Spanned<i64>>,
    vlans: Option<Spanned<RawVlans>>,
    mtu_probe_interval: Option<Spanned<String>>,
    mss: Option<Spanned<i64>>,
    commit_timeout: Option<Spanned<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawVlans {
    Text(String),
    List(Vec<RawVlanItem>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawVlanItem {
    Num(i64),
    Text(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSite {
    name: Option<Spanned<String>>,
    address: Option<Spanned<String>>,
    id: Option<Spanned<String>>,
    vlans: Option<Spanned<RawVlans>>,
    own_vlans: Option<Spanned<RawVlans>>,
    online: Option<Spanned<bool>>,
    changes: Option<Spanned<String>>,
    /// Before `changes`: read to take its meaning over, never written.
    managed_by: Option<Spanned<String>>,
    self_managed: Option<Spanned<bool>>,
    rename_by_manager: Option<Spanned<bool>>,
    uplink: Option<Spanned<String>>,
    tunnel_mode: Option<Spanned<String>>,
    /// VLAN ID (key) = its local breakout.
    breakout: Option<Spanned<Ordered<Spanned<RawBreakout>>>>,
    control_port: Option<Spanned<i64>>,
    tunnel_port: Option<Spanned<i64>>,
    /// Since 0.2 there are no roles; read and ignored with a warning.
    role: Option<Spanned<String>>,
    /// Since 0.2 there are no hubs; read and ignored with a warning.
    hub: Option<Spanned<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMap {
    site: Option<Spanned<String>>,
    local: Option<Spanned<RawLocalVid>>,
    shared: Option<Spanned<i64>>,
}

/// One VLAN of `breakout = { <VLAN> = { uplink = "ens20", except = [..] } }`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBreakout {
    uplink: Option<Spanned<String>>,
    except: Option<Spanned<Vec<Spanned<String>>>>,
}

/// `[[tunnel]]`: the VLANs one tunnel carries at most.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTunnel {
    between: Option<Spanned<Vec<String>>>,
    vlans: Option<Spanned<RawVlans>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawLocalVid {
    Num(i64),
    Text(String),
}

/// A TOML table read in the order of the file (sites keep their order),
/// with the position of every key.
#[derive(Debug)]
struct Ordered<V>(Vec<(Spanned<String>, V)>);

impl<'de, V: Deserialize<'de>> Deserialize<'de> for Ordered<V> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visit<V>(std::marker::PhantomData<V>);
        impl<'de, V: Deserialize<'de>> serde::de::Visitor<'de> for Visit<V> {
            type Value = Ordered<V>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a table")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Ordered(entries))
            }
        }
        d.deserialize_map(Visit(std::marker::PhantomData))
    }
}

/// `network.toml` in format 2.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetworkFile2 {
    network: Option<Spanned<RawNetworkSection>>,
    site: Option<Ordered<Spanned<RawSite2>>>,
    /// `[[map]]` of format 1 next to `[site.<name>]`: explained, not read.
    map: Option<Spanned<toml::Value>>,
    /// `[[tunnel]]` of format 1 next to `[site.<name>]`: explained.
    tunnel: Option<Spanned<toml::Value>>,
}

/// `[site.<name>]` in format 2, `[site."<identity>"]` in format 3.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSite2 {
    /// Format 3: the name of the site (the key is its identity).
    name: Option<Spanned<String>>,
    address: Option<Spanned<String>>,
    id: Option<Spanned<String>>,
    server: Option<Spanned<String>>,
    changes: Option<Spanned<String>>,
    managed_by: Option<Spanned<String>>,
    self_managed: Option<Spanned<bool>>,
    rename_by_manager: Option<Spanned<bool>>,
    online: Option<Spanned<bool>>,
    tunnel_mode: Option<Spanned<String>>,
    vlans: Option<Spanned<RawVlans>>,
    own_vlans: Option<Spanned<RawVlans>>,
    /// Source VLAN (key) = VLAN on the trunk of the site, or `"native"`.
    map: Option<Spanned<Ordered<Spanned<RawLocalVid>>>>,
    /// Other site (key) = the most the tunnel to it carries.
    tunnel_vlans: Option<Spanned<Ordered<Spanned<RawVlans>>>>,
    /// VLAN ID (key) = its local breakout.
    breakout: Option<Spanned<Ordered<Spanned<RawBreakout>>>>,
    control_port: Option<Spanned<i64>>,
    tunnel_port: Option<Spanned<i64>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLocal {
    #[serde(rename = "self")]
    self_name: Option<Spanned<String>>,
    trunk: Option<Spanned<RawPorts>>,
    tunnels: Option<std::collections::BTreeMap<String, RawLocalTunnel>>,
    commit_timeout: Option<Spanned<String>>,
    mtu_probe_interval: Option<Spanned<String>>,
    mss: Option<Spanned<i64>>,
}

/// `[tunnels.<site>]` in `local.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLocalTunnel {
    trunk: Option<Spanned<RawPorts>>,
}

/// One interface or a list of them.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawPorts {
    One(String),
    Many(Vec<String>),
}

impl RawPorts {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(port) => vec![port],
            Self::Many(ports) => ports,
        }
    }
}

/// Result of parsing `network.toml`.
#[derive(Debug, Clone)]
pub struct ParsedNetwork {
    /// The network, or `None` if the file contains no configuration at all
    /// (e.g. the commented template shipped with the package).
    pub network: Option<Network>,
    /// Source positions for line numbers in later diagnostics.
    pub spans: SpanMap,
    /// Warnings found while reading (e.g. keys that are no longer used).
    pub warnings: Vec<Diagnostic>,
    /// The format of the file: 1 (`[[site]]`, `[[map]]`, `[[tunnel]]`), 2
    /// (`[site.<name>]`), 3 (`[site."<identity>"]`), 0 for tables of both
    /// format 2 and 3.
    pub format: u8,
    /// Sites without a box renamed by hand: their placeholders as written
    /// and as they follow the names now (`local.toml` follows too).
    pub rekeyed: Vec<(SiteId, SiteId)>,
}

impl ParsedNetwork {
    /// The file is in format 1 (`[[site]]`, `[[map]]`, `[[tunnel]]`).
    pub fn old_format(&self) -> bool {
        self.format == 1
    }
}

/// A reference to a site as written: its identity or its name.
struct Ref {
    text: String,
    path: FieldPath,
    /// Who refers, for messages: `site "x": server`.
    what: String,
}

/// The references of one site, resolved once all sites are read.
struct SiteRefs {
    uplink: Option<Ref>,
    /// `changes` as written; without it, taken from `managed_by` and
    /// `self_managed` (files of earlier versions).
    changes: Option<Changes>,
    managed_by: Option<Ref>,
    self_managed: bool,
    /// `uplink` was given at all (format 1: `uplink = ""` marks a server).
    uplink_given: bool,
}

/// The site of a mapping or a tunnel limit.
enum Target {
    /// The site whose table it is in.
    Known(SiteId),
    /// Written in it.
    Named(Ref),
}

/// A mapping before its site is known.
struct PendingMap {
    site: Target,
    local: LocalVid,
    shared: u16,
}

/// A tunnel limit before its sites are known.
struct PendingTunnel {
    index: usize,
    sites: [Target; 2],
    vlans: VlanSet,
}

/// Result of parsing `local.toml`.
#[derive(Debug, Clone)]
pub struct ParsedLocal {
    /// The local configuration.
    pub local: Local,
    /// Source positions for line numbers in later diagnostics.
    pub spans: SpanMap,
}

/// Collects diagnostics and spans while lowering one file.
struct Lowering {
    file: ConfigFile,
    spans: SpanMap,
    diags: Vec<Diagnostic>,
}

impl Lowering {
    fn new(file: ConfigFile, text: &str) -> Self {
        Self {
            file,
            spans: SpanMap::new(text),
            diags: Vec::new(),
        }
    }

    fn span(&mut self, path: &FieldPath, span: Range<usize>) {
        self.spans.insert(path.clone(), span);
    }

    fn warning(&mut self, path: FieldPath, message: impl Into<String>) {
        let mut diag = Diagnostic::warning(self.file, path, message);
        diag.line = self.spans.line(&diag.path);
        self.diags.push(diag);
    }

    fn error(&mut self, path: FieldPath, message: impl Into<String>) {
        let mut diag = Diagnostic::error(self.file, path, message);
        diag.line = self.spans.line(&diag.path);
        self.diags.push(diag);
    }

    /// Records the span of an optional value and returns the inner value.
    fn take<T>(&mut self, path: &FieldPath, value: Option<Spanned<T>>) -> Option<T> {
        value.map(|v| {
            self.span(path, v.span());
            v.into_inner()
        })
    }

    fn required<T>(&mut self, path: FieldPath, value: Option<T>, what: &str) -> Option<T> {
        if value.is_none() {
            self.error(path, format!("{what} is missing"));
        }
        value
    }

    fn int_in_range(&mut self, path: &FieldPath, value: i64, min: i64, max: i64) -> Option<u64> {
        if (min..=max).contains(&value) {
            u64::try_from(value).ok()
        } else {
            self.error(
                path.clone(),
                format!("{path} must be between {min} and {max}, got {value}"),
            );
            None
        }
    }

    fn duration(&mut self, path: &FieldPath, text: &str) -> Option<Duration> {
        match humantime::parse_duration(text) {
            Ok(d) => Some(d),
            Err(e) => {
                self.error(
                    path.clone(),
                    format!("{path}: \"{text}\" is not a duration like \"60s\" or \"5m\" ({e})"),
                );
                None
            }
        }
    }
}

/// Converts a `toml` deserialization error into a diagnostic.
fn toml_error(file: ConfigFile, spans: &SpanMap, err: &toml::de::Error) -> Diagnostic {
    let mut diag = Diagnostic::error(file, FieldPath::root(), err.message().trim().to_string());
    diag.line = err.span().map(|s| spans.line_of_offset(s.start));
    diag
}

/// Parses `network.toml` in any format. Returns all syntax and type errors
/// at once.
pub fn parse_network(text: &str) -> Result<ParsedNetwork, Vec<Diagnostic>> {
    parse_network_in(text, None)
}

/// [`parse_network`] for a part of `context`: a name the text has no site
/// for is the site of that name in `context` (the view a client sends back
/// lacks the sites it does not see, but names them).
pub fn parse_network_in(
    text: &str,
    context: Option<&Network>,
) -> Result<ParsedNetwork, Vec<Diagnostic>> {
    if is_format_1(text) {
        parse_network_1(text, context)
    } else {
        parse_network_2(text, context)
    }
}

/// Resolves a reference: an identity as it is; a name to the site of that
/// name in `sites`, else to the one in `context`, else to its provisional
/// identity (which keeps the name: the validation tells if it is missing).
/// A name several sites have is an error.
fn resolve(
    lw: &mut Lowering,
    sites: &[Site],
    context: Option<&Network>,
    r: &Ref,
) -> Option<SiteId> {
    let text = r.text.trim();
    if let Ok(id) = text.parse::<SiteId>() {
        return Some(id);
    }
    let mut named = sites.iter().filter(|s| s.name == text);
    match (named.next(), named.next()) {
        (Some(one), None) => Some(one.id),
        (Some(_), Some(_)) => {
            lw.error(
                r.path.clone(),
                format!(
                    "{}: \"{text}\" names several sites; write the identity of the one it means",
                    r.what
                ),
            );
            None
        }
        (None, _) => Some(
            context
                .and_then(|c| c.by_name(text))
                .map_or_else(|| SiteId::provisional(text), |s| s.id),
        ),
    }
}

/// Fills in the references once all sites are read.
fn link(
    lw: &mut Lowering,
    network: &mut Network,
    refs: Vec<SiteRefs>,
    maps: Vec<PendingMap>,
    tunnels: Vec<PendingTunnel>,
    context: Option<&Network>,
    format_1: bool,
) -> Vec<(SiteId, SiteId)> {
    // Sites without an identity are known by their name alone.
    for (i, site) in network.sites.iter().enumerate() {
        if site.has_identity() {
            continue;
        }
        if let Some(first) = network.sites[..i]
            .iter()
            .position(|s| s.name == site.name && !s.has_identity())
        {
            lw.error(
                FieldPath::site_field(i, "name"),
                format!(
                    "site \"{}\" is defined twice (also site #{}); two sites may only share a name once both have an identity (a wanderlan version that has one)",
                    site.name,
                    first + 1
                ),
            );
        }
    }
    for (i, r) in refs.into_iter().enumerate() {
        let uplink = r
            .uplink
            .and_then(|u| resolve(lw, &network.sites, context, &u));
        let managed_by = r
            .managed_by
            .and_then(|m| resolve(lw, &network.sites, context, &m));
        let me = network.sites[i].id;
        // Configurations from before uplinks: a site that joined with an
        // invitation (it has a manager) hangs off that site.
        let uplink = match (uplink, managed_by) {
            (None, Some(m))
                if format_1 && !r.uplink_given && m != me && network.site(m).is_some() =>
            {
                Some(m)
            }
            (uplink, _) => uplink,
        };
        // Before `changes`: the manager (usually the server) alone, with the
        // site itself if released, or the site alone if it managed itself;
        // no manager: anyone (both).
        let changes = r.changes.unwrap_or(match managed_by {
            Some(m) if m == me => Changes::Client,
            Some(_) if r.self_managed => Changes::Both,
            Some(_) => Changes::Server,
            None => Changes::Both,
        });
        let site = &mut network.sites[i];
        site.uplink = uplink;
        // A server has no such option.
        site.changes = if uplink.is_some() {
            changes
        } else {
            Changes::default()
        };
    }
    let target = |lw: &mut Lowering, network: &Network, t: &Target| match t {
        Target::Known(id) => Some(*id),
        Target::Named(r) => resolve(lw, &network.sites, context, r),
    };
    for m in maps {
        if let Some(site) = target(lw, network, &m.site) {
            network.maps.push(VlanMap {
                site,
                local: m.local,
                shared: m.shared,
            });
        }
    }
    for t in tunnels {
        let [a, b] = &t.sites;
        let (Some(a), Some(b)) = (target(lw, network, a), target(lw, network, b)) else {
            continue;
        };
        let path = FieldPath::tunnel_field(t.index, "between");
        if a == b {
            let name = network.label(a);
            lw.error(
                path,
                format!("tunnel limit: a tunnel connects two sites, not \"{name}\" with itself"),
            );
            continue;
        }
        let sites = site_pair(a, b);
        // Format 2: written at both of its sites.
        if !format_1 && network.tunnels.iter().any(|t| t.sites == sites) {
            let (a, b) = (network.label(a), network.label(b));
            lw.error(
                path,
                format!(
                    "the tunnel between \"{a}\" and \"{b}\" is limited at both sites; keep one `tunnel_vlans` entry"
                ),
            );
            continue;
        }
        network.tunnels.push(TunnelVlans {
            sites,
            vlans: t.vlans,
        });
    }
    // A site without a box that was renamed by hand: its placeholder
    // follows its name, with every reference to it (two such sites with
    // one name are reported above).
    let renamed: Vec<(SiteId, SiteId)> = network
        .sites
        .iter()
        .filter(|s| s.id.is_provisional() && s.id != SiteId::provisional(&s.name))
        .map(|s| (s.id, SiteId::provisional(&s.name)))
        .collect();
    // All at once (two sites may have swapped their names): first to
    // placeholders no name has, then to those of the names.
    for (i, (old, _)) in renamed.iter().enumerate() {
        network.rekey(*old, SiteId::provisional(&format!("\0{i}")));
    }
    for (i, (_, new)) in renamed.iter().enumerate() {
        network.rekey(SiteId::provisional(&format!("\0{i}")), *new);
    }
    renamed
}

/// Format 1: `[[site]]` is an array, or there are only `[[map]]` or
/// `[[tunnel]]` entries. A file that does not parse at all goes to format 2,
/// whose parser reports the same syntax error.
fn is_format_1(text: &str) -> bool {
    toml::from_str::<toml::Table>(text).is_ok_and(|t| match t.get("site") {
        Some(site) => site.is_array(),
        None => t.contains_key("map") || t.contains_key("tunnel"),
    })
}

fn parse_network_1(
    text: &str,
    context: Option<&Network>,
) -> Result<ParsedNetwork, Vec<Diagnostic>> {
    let mut lw = Lowering::new(ConfigFile::Network, text);
    let raw: RawNetworkFile = match toml::from_str(text) {
        Ok(raw) => raw,
        Err(e) => return Err(vec![toml_error(ConfigFile::Network, &lw.spans, &e)]),
    };

    if raw.network.is_none() && raw.site.is_empty() && raw.map.is_empty() && raw.tunnel.is_empty() {
        return Ok(ParsedNetwork {
            network: None,
            spans: lw.spans,
            warnings: Vec::new(),
            format: 1,
            rekeyed: Vec::new(),
        });
    }

    let network_path = FieldPath::network("");
    let section = lw.take(&network_path, raw.network);
    let mut network = Network::new(VlanSet::new());
    match section {
        None => lw.error(
            FieldPath::root(),
            "the [network] table is missing (it needs at least `vlans = \"10-50\"`)",
        ),
        Some(section) => lower_section(&mut lw, section, &mut network),
    }

    let mut refs = Vec::new();
    for (i, raw_site) in raw.site.into_iter().enumerate() {
        let path = FieldPath::site(i);
        if let Some((site, r)) = lw
            .take(&path, Some(raw_site))
            .and_then(|s| lower_site(&mut lw, i, s))
        {
            network.sites.push(site);
            refs.push(r);
        }
    }
    let mut maps = Vec::new();
    for (i, raw_map) in raw.map.into_iter().enumerate() {
        let path = FieldPath::map(i);
        if let Some(map) = lw
            .take(&path, Some(raw_map))
            .and_then(|m| lower_map(&mut lw, i, m))
        {
            maps.push(map);
        }
    }
    let mut tunnels = Vec::new();
    for (i, raw_tunnel) in raw.tunnel.into_iter().enumerate() {
        let path = FieldPath::tunnel(i);
        if let Some(tunnel) = lw
            .take(&path, Some(raw_tunnel))
            .and_then(|t| lower_tunnel(&mut lw, i, t))
        {
            tunnels.push(tunnel);
        }
    }
    let rekeyed = link(&mut lw, &mut network, refs, maps, tunnels, context, true);

    if lw.diags.iter().any(Diagnostic::is_error) {
        Err(lw.diags)
    } else {
        Ok(ParsedNetwork {
            network: Some(network),
            spans: lw.spans,
            warnings: lw.diags,
            format: 1,
            rekeyed,
        })
    }
}

fn parse_network_2(
    text: &str,
    context: Option<&Network>,
) -> Result<ParsedNetwork, Vec<Diagnostic>> {
    let mut lw = Lowering::new(ConfigFile::Network, text);
    let raw: RawNetworkFile2 = match toml::from_str(text) {
        Ok(raw) => raw,
        Err(e) => return Err(vec![toml_error(ConfigFile::Network, &lw.spans, &e)]),
    };
    let sites = raw.site.map(|s| s.0).unwrap_or_default();
    if raw.network.is_none() && sites.is_empty() && raw.map.is_none() && raw.tunnel.is_none() {
        return Ok(ParsedNetwork {
            network: None,
            spans: lw.spans,
            warnings: Vec::new(),
            format: 2,
            rekeyed: Vec::new(),
        });
    }
    for (key, value, example) in [
        ("map", raw.map, "map = { 30 = 130 }"),
        ("tunnel", raw.tunnel, "tunnel_vlans = { lab = \"30-40\" }"),
    ] {
        if let Some(value) = value {
            let path = FieldPath::network(key);
            lw.span(&path, value.span());
            lw.error(
                path,
                format!(
                    "[[{key}]] belongs to an older format of this file; write it into the table of its site instead, e.g. {example}"
                ),
            );
        }
    }

    let network_path = FieldPath::network("");
    let section = lw.take(&network_path, raw.network);
    let mut network = Network::new(VlanSet::new());
    match section {
        None => lw.error(
            FieldPath::root(),
            "the [network] table is missing (it needs at least `vlans = \"10-50\"`)",
        ),
        Some(section) => lower_section(&mut lw, section, &mut network),
    }

    let (mut by_id, mut by_name) = (0usize, 0usize);
    let mut refs = Vec::new();
    let mut maps = Vec::new();
    let mut tunnels = Vec::new();
    let (mut map_index, mut tunnel_index) = (0, 0);
    for (i, (key, raw_site)) in sites.into_iter().enumerate() {
        lw.span(&FieldPath::site(i), raw_site.span());
        let raw_site = raw_site.into_inner();
        // Format 3: the key is the identity, the name is inside.
        let keyed_by_id = key.get_ref().trim().parse::<SiteId>().is_ok();
        let (name, id) = if keyed_by_id {
            by_id += 1;
            if let Some(id) = &raw_site.id {
                let path = FieldPath::site_field(i, "id");
                lw.span(&path, id.span());
                lw.error(
                    path,
                    "`id` is not needed: the identity of the site is the name of its table, [site.\"<identity>\"]",
                );
            }
            (raw_site.name, Some(key))
        } else {
            by_name += 1;
            if let Some(name) = &raw_site.name {
                let path = FieldPath::site_field(i, "name");
                lw.span(&path, name.span());
                lw.error(
                    path,
                    format!(
                        "site \"{}\": `name` belongs to a table named by the identity of the site, [site.\"<identity>\"]; here the name of the table is the name",
                        key.get_ref()
                    ),
                );
            }
            (Some(key), raw_site.id)
        };
        let site_name = name
            .as_ref()
            .map(|n| n.get_ref().clone())
            .unwrap_or_default();
        let label = format!("site \"{site_name}\"");
        if let Some(server) = &raw_site.server
            && server.get_ref().trim().is_empty()
        {
            let path = FieldPath::site_field(i, "uplink");
            lw.span(&path, server.span());
            lw.error(
                path,
                format!(
                    "{label}: `server = \"\"` is not needed: a site without `server` is a server"
                ),
            );
        }
        let site = RawSite {
            name,
            address: raw_site.address,
            id,
            vlans: raw_site.vlans,
            own_vlans: raw_site.own_vlans,
            online: raw_site.online,
            changes: raw_site.changes,
            managed_by: raw_site.managed_by,
            self_managed: raw_site.self_managed,
            rename_by_manager: raw_site.rename_by_manager,
            uplink: raw_site.server,
            tunnel_mode: raw_site.tunnel_mode,
            breakout: raw_site.breakout,
            control_port: raw_site.control_port,
            tunnel_port: raw_site.tunnel_port,
            role: None,
            hub: None,
        };
        let owner = match lower_site(&mut lw, i, site) {
            Some((site, r)) => {
                let id = site.id;
                network.sites.push(site);
                refs.push(r);
                Some(id)
            }
            None => None,
        };
        if let Some(map) = raw_site.map {
            let table = map.span();
            for (source, mapped) in map.into_inner().0 {
                if let Some((local, shared)) = lower_map_2(
                    &mut lw,
                    map_index,
                    &site_name,
                    table.clone(),
                    source,
                    mapped,
                ) && let Some(owner) = owner
                {
                    maps.push(PendingMap {
                        site: Target::Known(owner),
                        local,
                        shared,
                    });
                }
                map_index += 1;
            }
        }
        if let Some(limits) = raw_site.tunnel_vlans {
            let table = limits.span();
            for (other, vlans) in limits.into_inner().0 {
                if let Some((other, vlans)) = lower_tunnel_2(
                    &mut lw,
                    tunnel_index,
                    &site_name,
                    table.clone(),
                    other,
                    vlans,
                ) && let Some(owner) = owner
                {
                    tunnels.push(PendingTunnel {
                        index: tunnel_index,
                        sites: [Target::Known(owner), Target::Named(other)],
                        vlans,
                    });
                }
                tunnel_index += 1;
            }
        }
    }
    let rekeyed = link(&mut lw, &mut network, refs, maps, tunnels, context, false);

    let format = match (by_id, by_name) {
        (_, 0) if by_id > 0 => 3,
        (0, _) => 2,
        _ => 0,
    };
    if lw.diags.iter().any(Diagnostic::is_error) {
        Err(lw.diags)
    } else {
        Ok(ParsedNetwork {
            network: Some(network),
            spans: lw.spans,
            warnings: lw.diags,
            format,
            rekeyed,
        })
    }
}

/// One entry of `map = { <source VLAN> = <VLAN on the trunk> | "native" }`.
fn lower_map_2(
    lw: &mut Lowering,
    i: usize,
    site: &str,
    table: Range<usize>,
    source: Spanned<String>,
    mapped: Spanned<RawLocalVid>,
) -> Option<(LocalVid, u16)> {
    lw.span(&FieldPath::map(i), table.clone());
    lw.span(&FieldPath::map_field(i, "site"), table);
    let shared_path = FieldPath::map_field(i, "shared");
    lw.span(&shared_path, source.span());
    let local_path = FieldPath::map_field(i, "local");
    lw.span(&local_path, mapped.span());
    let label = format!("site \"{site}\": map");
    let source = source.into_inner();
    let shared = match source.trim().parse::<u16>() {
        Ok(v) if (VID_MIN..=VID_MAX).contains(&v) => Some(v),
        _ => {
            lw.error(
                shared_path,
                format!(
                    "{label}: \"{source}\" is not a VLAN ID ({VID_MIN}-{VID_MAX}); write map = {{ <source VLAN> = <VLAN on the trunk> }}, e.g. {{ 30 = 130 }}"
                ),
            );
            None
        }
    };
    let local = match mapped.into_inner() {
        RawLocalVid::Text(t) if t == "native" => Some(LocalVid::Native),
        RawLocalVid::Text(t) => {
            lw.error(
                local_path,
                format!(
                    "{label}: {source} = \"{t}\": the VLAN on the trunk must be a VLAN ID or \"native\""
                ),
            );
            None
        }
        RawLocalVid::Num(n) => lw
            .int_in_range(&local_path, n, i64::from(VID_MIN), i64::from(VID_MAX))
            .and_then(|v| u16::try_from(v).ok())
            .map(LocalVid::Vid),
    };
    Some((local?, shared?))
}

/// One entry of `tunnel_vlans = { <other site> = "<VLANs>" | "none" }`.
fn lower_tunnel_2(
    lw: &mut Lowering,
    i: usize,
    site: &str,
    table: Range<usize>,
    other: Spanned<String>,
    vlans: Spanned<RawVlans>,
) -> Option<(Ref, VlanSet)> {
    lw.span(&FieldPath::tunnel(i), table);
    let between_path = FieldPath::tunnel_field(i, "between");
    lw.span(&between_path, other.span());
    let vlans_path = FieldPath::tunnel_field(i, "vlans");
    lw.span(&vlans_path, vlans.span());
    let other = other.into_inner();
    let label = format!("site \"{site}\": tunnel_vlans");
    let other_ref = Ref {
        text: other.clone(),
        path: between_path,
        what: label.clone(),
    };
    let vlans = match vlans.into_inner() {
        RawVlans::Text(t) if t.trim() == "none" => Some(VlanSet::new()),
        RawVlans::Text(t) if t.trim().is_empty() => Some(VlanSet::all()),
        v => match parse_vlans(v) {
            Ok(set) => Some(set),
            Err(e) => {
                lw.error(vlans_path, format!("{label}: {other}: {e}"));
                None
            }
        },
    };
    Some((other_ref, vlans?))
}

fn lower_section(lw: &mut Lowering, s: RawNetworkSection, network: &mut Network) {
    let path = FieldPath::network("revision");
    if let Some(v) = lw.take(&path, s.revision) {
        network.revision = lw.int_in_range(&path, v, 0, i64::MAX).unwrap_or(0);
    }

    let path = FieldPath::network("control_port");
    network.control_port = match lw.take(&path, s.control_port) {
        Some(v) => lw
            .int_in_range(&path, v, 1, 65535)
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(DEFAULT_CONTROL_PORT),
        None => DEFAULT_CONTROL_PORT,
    };

    let path = FieldPath::network("tunnel_port");
    if let Some(v) = lw.take(&path, s.tunnel_port) {
        network.tunnel_port = lw
            .int_in_range(&path, v, 1, 65534)
            .and_then(|v| u16::try_from(v).ok());
    }

    // Without (or with an empty) `vlans` every VLAN and the untagged
    // traffic pass, like on a trunk between two switches.
    let path = FieldPath::network("vlans");
    network.vlans = match lw.take(&path, s.vlans) {
        None => VlanSet::all(),
        Some(RawVlans::Text(t)) if t.trim().is_empty() => VlanSet::all(),
        Some(vlans) => match parse_vlans(vlans) {
            Ok(set) => set,
            Err(e) => {
                lw.error(path, format!("network.vlans: {e}"));
                VlanSet::all()
            }
        },
    };

    let path = FieldPath::network("mtu_probe_interval");
    network.mtu_probe_interval = match lw.take(&path, s.mtu_probe_interval) {
        Some(text) => lw
            .duration(&path, &text)
            .unwrap_or(DEFAULT_MTU_PROBE_INTERVAL),
        None => DEFAULT_MTU_PROBE_INTERVAL,
    };

    let path = FieldPath::network("commit_timeout");
    network.commit_timeout = match lw.take(&path, s.commit_timeout) {
        Some(text) => lw.duration(&path, &text).unwrap_or(DEFAULT_COMMIT_TIMEOUT),
        None => DEFAULT_COMMIT_TIMEOUT,
    };

    let path = FieldPath::network("mss");
    if let Some(v) = lw.take(&path, s.mss) {
        network.mss = lw
            .int_in_range(&path, v, 1, 65535)
            .and_then(|v| u16::try_from(v).ok());
    }
}

/// Parses `"10-50"`, `"10,20,30-40"` or a list `[10, "20-30"]`.
fn parse_vlans(vlans: RawVlans) -> Result<VlanSet, super::vlan::VlanSetError> {
    match vlans {
        RawVlans::Text(text) => text.parse::<VlanSet>(),
        RawVlans::List(items) => {
            let mut set = VlanSet::new();
            items.iter().try_for_each(|item| match item {
                RawVlanItem::Num(n) => set.insert_checked(*n),
                RawVlanItem::Text(t) => set.insert_item(t),
            })?;
            if set.is_empty() {
                Err(super::vlan::VlanSetError::Empty)
            } else {
                Ok(set)
            }
        }
    }
}

/// Lowers one site; its references are resolved once all sites are read.
fn lower_site(lw: &mut Lowering, i: usize, s: RawSite) -> Option<(Site, SiteRefs)> {
    let name_path = FieldPath::site_field(i, "name");
    let name = lw.take(&name_path, s.name);
    let addr_path = FieldPath::site_field(i, "address");
    let address = lw.take(&addr_path, s.address);
    let id_path = FieldPath::site_field(i, "id");
    let id_text = lw.take(&id_path, s.id);
    let vlans_path = FieldPath::site_field(i, "vlans");
    let vlans = lw.take(&vlans_path, s.vlans);
    let own_path = FieldPath::site_field(i, "own_vlans");
    let own_vlans = lw.take(&own_path, s.own_vlans);
    let breakout = lw.take(&FieldPath::site_field(i, "breakout"), s.breakout);
    let mut port = |field: &str, value: Option<Spanned<i64>>, max: i64| {
        let path = FieldPath::site_field(i, field);
        let value = lw.take(&path, value)?;
        lw.int_in_range(&path, value, 1, max)
            .and_then(|v| u16::try_from(v).ok())
    };
    let ports = SitePorts {
        control: port("control_port", s.control_port, 65535),
        tunnel: port("tunnel_port", s.tunnel_port, 65534),
    };
    let online = lw
        .take(&FieldPath::site_field(i, "online"), s.online)
        .unwrap_or(true);
    let changes_path = FieldPath::site_field(i, "changes");
    let changes = lw.take(&changes_path, s.changes);
    let managed_path = FieldPath::site_field(i, "managed_by");
    let managed_by = lw.take(&managed_path, s.managed_by);
    let self_managed = lw
        .take(&FieldPath::site_field(i, "self_managed"), s.self_managed)
        .unwrap_or(false);
    // Written by 1.0.23 and 1.0.24; the mode is detected now.
    let mode_path = FieldPath::site_field(i, "tunnel_mode");
    let tunnel_mode = lw.take(&mode_path, s.tunnel_mode).is_some();
    // Written by 1.0.22; its server may rename a site now.
    let rename_path = FieldPath::site_field(i, "rename_by_manager");
    let rename_by_manager = lw.take(&rename_path, s.rename_by_manager).is_some();
    // `uplink = ""` marks a server explicitly.
    let uplink_given = s.uplink.is_some();
    let uplink_path = FieldPath::site_field(i, "uplink");
    let uplink = lw
        .take(&uplink_path, s.uplink)
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty());

    let label = name
        .as_deref()
        .map_or_else(|| format!("site #{}", i + 1), |n| format!("site \"{n}\""));
    for (field, value) in [("role", s.role), ("hub", s.hub)] {
        let path = FieldPath::site_field(i, field);
        if lw.take(&path, value).is_some() {
            lw.warning(
                path,
                format!(
                    "{label}: `{field}` is no longer used and is ignored; use `server` to make a site a client of another"
                ),
            );
        }
    }
    if tunnel_mode {
        lw.warning(
            mode_path,
            format!(
                "{label}: `tunnel_mode` is no longer used and is ignored; the tunnel finds out by itself whether its server reaches it directly"
            ),
        );
    }
    if rename_by_manager {
        lw.warning(
            rename_path,
            format!(
                "{label}: `rename_by_manager` is no longer used and is ignored; its server may rename it"
            ),
        );
    }
    let name = lw.required(name_path, name, &format!("{label}: name"));
    let vlans = vlans.and_then(|v| match v {
        // Empty: the default VLANs of the network.
        RawVlans::Text(t) if t.trim().is_empty() => None,
        v => match parse_vlans(v) {
            Ok(set) => Some(set),
            Err(e) => {
                lw.error(vlans_path, format!("{label}: vlans: {e}"));
                None
            }
        },
    });
    let own_vlans = own_vlans.and_then(|v| match v {
        RawVlans::Text(t) if t.trim().is_empty() || t.trim() == "none" => None,
        v => match parse_vlans(v) {
            Ok(set) => Some(set),
            Err(e) => {
                lw.error(own_path, format!("{label}: own_vlans: {e}"));
                None
            }
        },
    });
    let address = lw
        .required(addr_path.clone(), address, &format!("{label}: address"))
        .and_then(|a| match a.trim().parse::<Ipv4Addr>() {
            Ok(ip) => Some(ip),
            Err(_) => {
                lw.error(
                    addr_path,
                    format!("{label}: address \"{a}\" is not an IPv4 address"),
                );
                None
            }
        });
    // A placeholder of a site without a box follows its name (see `link`).
    let id = match id_text.map(|text| (text.parse::<SiteId>(), text)) {
        None => Some(None),
        Some((Ok(id), _)) => Some(Some(id)),
        Some((Err(e), text)) => {
            lw.error(id_path, format!("{label}: id \"{text}\" is {e}"));
            None
        }
    };
    let breakout = breakout.map_or_else(Breakouts::new, |b| lower_breakout(lw, i, &label, b));
    let changes = changes.and_then(|c| match c.parse::<Changes>() {
        Ok(c) => Some(c),
        Err(e) => {
            lw.error(
                changes_path,
                format!("{label}: changes: {e} (who changes its tunnel to its server)"),
            );
            None
        }
    });
    let name = name?;
    let refs = SiteRefs {
        uplink: uplink.map(|text| Ref {
            text,
            path: uplink_path,
            what: format!("site \"{name}\": server"),
        }),
        changes,
        managed_by: managed_by.map(|text| Ref {
            text,
            path: managed_path,
            what: format!("site \"{name}\": managed_by"),
        }),
        self_managed,
        uplink_given,
    };
    Some((
        Site {
            id: id?.unwrap_or_else(|| SiteId::provisional(&name)),
            name,
            address: address?,
            vlans,
            own_vlans,
            online,
            changes: Changes::default(),
            uplink: None,
            breakout,
            ports,
        },
        refs,
    ))
}

/// `breakout = { <VLAN> = { uplink = "<interface>", except = [<networks>] } }`.
fn lower_breakout(
    lw: &mut Lowering,
    i: usize,
    label: &str,
    raw: Ordered<Spanned<RawBreakout>>,
) -> Breakouts {
    let mut breakouts = Breakouts::new();
    for (key, entry) in raw.0 {
        let key_span = key.span();
        let key = key.into_inner();
        let vid = match key.trim().parse::<u16>() {
            Ok(v) if (VID_MIN..=VID_MAX).contains(&v) => v,
            _ => {
                let path = FieldPath::site_field(i, "breakout");
                lw.span(&path, key_span);
                lw.error(
                    path,
                    format!(
                        "{label}: breakout: \"{key}\" is not a VLAN ID ({VID_MIN}-{VID_MAX}); write breakout = {{ <VLAN> = {{}} }}, e.g. {{ 10 = {{}} }} (untagged traffic: 1)"
                    ),
                );
                continue;
            }
        };
        let path = FieldPath::site_field(i, &format!("breakout.{vid}"));
        lw.span(&path, entry.span());
        if breakouts.contains_key(&vid) {
            lw.error(
                path,
                format!("{label}: breakout: VLAN {vid} is listed twice"),
            );
            continue;
        }
        let entry = entry.into_inner();
        let uplink_path = FieldPath::site_field(i, &format!("breakout.{vid}.uplink"));
        let uplink = lw
            .take(&uplink_path, entry.uplink)
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty() && u != "auto");
        let except_path = FieldPath::site_field(i, &format!("breakout.{vid}.except"));
        let mut except = std::collections::BTreeSet::new();
        for item in lw.take(&except_path, entry.except).unwrap_or_default() {
            let span = item.span();
            let text = item.into_inner();
            match crate::breakout::parse_net(&text) {
                Some(net) => {
                    except.insert(net);
                }
                None => {
                    lw.span(&except_path, span);
                    lw.error(
                        except_path.clone(),
                        format!(
                            "{label}: breakout {vid}: except \"{text}\" is not an address or a network like 203.0.113.0/24 or 2001:db8::/32"
                        ),
                    );
                }
            }
        }
        breakouts.insert(vid, Breakout { uplink, except });
    }
    breakouts
}

fn lower_map(lw: &mut Lowering, i: usize, m: RawMap) -> Option<PendingMap> {
    let site_path = FieldPath::map_field(i, "site");
    let site = lw.take(&site_path, m.site);
    let local_path = FieldPath::map_field(i, "local");
    let local = lw.take(&local_path, m.local);
    let shared_path = FieldPath::map_field(i, "shared");
    let shared = lw.take(&shared_path, m.shared);

    let label = format!("map #{}", i + 1);
    let site = lw
        .required(site_path.clone(), site, &format!("{label}: site"))
        .map(|text| Ref {
            text,
            path: site_path,
            what: label.clone(),
        });
    let local = lw
        .required(
            local_path.clone(),
            local,
            &format!("{label}: local (\"native\" or a VLAN ID)"),
        )
        .and_then(|l| match l {
            RawLocalVid::Text(t) if t == "native" => Some(LocalVid::Native),
            RawLocalVid::Text(t) => {
                lw.error(
                    local_path,
                    format!("{label}: local must be \"native\" or a VLAN ID, got \"{t}\""),
                );
                None
            }
            RawLocalVid::Num(n) => lw
                .int_in_range(&local_path, n, i64::from(VID_MIN), i64::from(VID_MAX))
                .and_then(|v| u16::try_from(v).ok())
                .map(LocalVid::Vid),
        });
    let shared = lw
        .required(shared_path.clone(), shared, &format!("{label}: shared"))
        .and_then(|n| {
            lw.int_in_range(&shared_path, n, i64::from(VID_MIN), i64::from(VID_MAX))
                .and_then(|v| u16::try_from(v).ok())
        });

    Some(PendingMap {
        site: Target::Named(site?),
        local: local?,
        shared: shared?,
    })
}

fn lower_tunnel(lw: &mut Lowering, i: usize, t: RawTunnel) -> Option<PendingTunnel> {
    let label = format!("tunnel #{}", i + 1);
    let between_path = FieldPath::tunnel_field(i, "between");
    let between = lw.take(&between_path, t.between);
    let vlans_path = FieldPath::tunnel_field(i, "vlans");
    let vlans = lw.take(&vlans_path, t.vlans);
    let sites = lw
        .required(
            between_path.clone(),
            between,
            &format!("{label}: between (the two sites, e.g. [\"hq\", \"lab\"])"),
        )
        .and_then(|names| match names.as_slice() {
            [a, b] if a != b => Some([a, b].map(|text| {
                Target::Named(Ref {
                    text: text.clone(),
                    path: between_path.clone(),
                    what: label.clone(),
                })
            })),
            [a, _] => {
                lw.error(
                    between_path.clone(),
                    format!("{label}: a tunnel connects two sites, not \"{a}\" with itself"),
                );
                None
            }
            _ => {
                lw.error(
                    between_path.clone(),
                    format!(
                        "{label}: between needs exactly two site names, e.g. [\"hq\", \"lab\"]"
                    ),
                );
                None
            }
        });
    let vlans = lw
        .required(
            vlans_path.clone(),
            vlans,
            &format!("{label}: vlans (e.g. \"10-20\" or \"none\")"),
        )
        .and_then(|v| match v {
            RawVlans::Text(t) if t.trim() == "none" => Some(VlanSet::new()),
            RawVlans::Text(t) if t.trim().is_empty() => Some(VlanSet::all()),
            v => match parse_vlans(v) {
                Ok(set) => Some(set),
                Err(e) => {
                    lw.error(vlans_path, format!("{label}: vlans: {e}"));
                    None
                }
            },
        });
    Some(PendingTunnel {
        index: i,
        sites: sites?,
        vlans: vlans?,
    })
}

/// True if `local.toml` names a site by its name, not by its identity (a
/// file of an older version).
pub fn local_uses_names(text: &str) -> bool {
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return false;
    };
    let is_name = |s: &str| s.trim().parse::<SiteId>().is_err();
    table
        .get("self")
        .and_then(toml::Value::as_str)
        .is_some_and(is_name)
        || table
            .get("tunnels")
            .and_then(toml::Value::as_table)
            .is_some_and(|t| t.keys().any(|k| is_name(k)))
}

/// Parses `local.toml`. Its sites (`self`, `[tunnels.<site>]`) are
/// identities, or names (files of older versions) of sites of `network`.
pub fn parse_local(text: &str, network: Option<&Network>) -> Result<ParsedLocal, Vec<Diagnostic>> {
    let mut lw = Lowering::new(ConfigFile::Local, text);
    let raw: RawLocal = match toml::from_str(text) {
        Ok(raw) => raw,
        Err(e) => return Err(vec![toml_error(ConfigFile::Local, &lw.spans, &e)]),
    };
    let sites = network.map_or(&[][..], |n| n.sites.as_slice());
    let self_path = FieldPath::local("self");
    let self_id = lw.take(&self_path, raw.self_name).and_then(|text| {
        let r = Ref {
            text,
            path: self_path,
            what: "self".into(),
        };
        resolve(&mut lw, sites, None, &r)
    });
    let trunk_path = FieldPath::local("trunk");
    let trunk = lw
        .take(&trunk_path, raw.trunk)
        .map(RawPorts::into_vec)
        .unwrap_or_default();
    let mut tunnel_trunks = std::collections::BTreeMap::new();
    for (site, tunnel) in raw.tunnels.unwrap_or_default() {
        let r = Ref {
            path: FieldPath::local(&format!("tunnels.{site}.trunk")),
            text: site,
            what: "tunnels".into(),
        };
        let Some(id) = resolve(&mut lw, sites, None, &r) else {
            continue;
        };
        // The validation names the entry by the identity.
        let path = FieldPath::local(&format!("tunnels.{id}.trunk"));
        if let Some(t) = lw.take(&path, tunnel.trunk)
            && tunnel_trunks.insert(id, t.into_vec()).is_some()
        {
            let name = sites
                .iter()
                .find(|s| s.id == id)
                .map_or_else(|| id.to_string(), |s| s.name.clone());
            lw.error(
                path,
                format!(
                    "the tunnel to \"{name}\" is listed twice (by its name and by its identity)"
                ),
            );
        }
    }
    let path = FieldPath::local("commit_timeout");
    let commit_timeout = lw
        .take(&path, raw.commit_timeout)
        .and_then(|text| lw.duration(&path, &text));
    let path = FieldPath::local("mtu_probe_interval");
    let mtu_probe_interval = lw
        .take(&path, raw.mtu_probe_interval)
        .and_then(|text| lw.duration(&path, &text));
    let path = FieldPath::local("mss");
    let mss = lw.take(&path, raw.mss).and_then(|v| {
        lw.int_in_range(&path, v, 1, 65535)
            .and_then(|v| u16::try_from(v).ok())
    });
    let local = Local {
        self_id,
        trunk,
        tunnel_trunks,
        commit_timeout,
        mtu_probe_interval,
        mss,
    };
    if lw.diags.is_empty() {
        Ok(ParsedLocal {
            local,
            spans: lw.spans,
        })
    } else {
        Err(lw.diags)
    }
}
