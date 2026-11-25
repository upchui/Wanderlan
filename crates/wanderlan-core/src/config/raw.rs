//! Raw TOML structures with source spans and their conversion into the model.
//!
//! All keys are optional at this level so that missing values produce our own
//! messages with line numbers instead of generic serde errors. Unknown keys are
//! rejected to catch typos.
//!
//! `network.toml` comes in two formats with the same model:
//! * format 2 (written since 1.0.21): `[site.<name>]` tables with the
//!   mappings (`map = { <source VLAN> = <VLAN on the trunk> }`) and tunnel
//!   limits (`tunnel_vlans = { <site> = "<VLANs>" }`) inside the site;
//! * format 1: `[[site]]`, `[[map]]` and `[[tunnel]]` arrays. The sites
//!   exchange the network in this format (see `render_network_wire`), so it
//!   is read forever.

use std::net::Ipv4Addr;
use std::ops::Range;
use std::time::Duration;

use serde::Deserialize;
use serde_spanned::Spanned;

use super::diag::{ConfigFile, Diagnostic, FieldPath, SpanMap};
use super::model::{
    DEFAULT_COMMIT_TIMEOUT, DEFAULT_CONTROL_PORT, DEFAULT_MTU_PROBE_INTERVAL, Local, LocalVid,
    Network, Site, TunnelMode, TunnelVlans, VlanMap, site_pair,
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
    managed_by: Option<Spanned<String>>,
    self_managed: Option<Spanned<bool>>,
    rename_by_manager: Option<Spanned<bool>>,
    uplink: Option<Spanned<String>>,
    tunnel_mode: Option<Spanned<String>>,
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

/// `[site.<name>]` in format 2.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSite2 {
    address: Option<Spanned<String>>,
    id: Option<Spanned<String>>,
    server: Option<Spanned<String>>,
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLocal {
    #[serde(rename = "self")]
    self_name: Option<Spanned<String>>,
    trunk: Option<Spanned<RawPorts>>,
    tunnels: Option<std::collections::BTreeMap<String, RawLocalTunnel>>,
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
    /// The file is in format 1 (`[[site]]`, `[[map]]`, `[[tunnel]]`).
    pub old_format: bool,
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

/// Parses `network.toml` in either format. Returns all syntax and type
/// errors at once.
pub fn parse_network(text: &str) -> Result<ParsedNetwork, Vec<Diagnostic>> {
    if is_format_1(text) {
        parse_network_1(text)
    } else {
        parse_network_2(text)
    }
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

fn parse_network_1(text: &str) -> Result<ParsedNetwork, Vec<Diagnostic>> {
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
            old_format: false,
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

    let mut uplink_missing = Vec::new();
    for (i, raw_site) in raw.site.into_iter().enumerate() {
        let path = FieldPath::site(i);
        if let Some((site, given)) = lw
            .take(&path, Some(raw_site))
            .and_then(|s| lower_site(&mut lw, i, s))
        {
            if !given {
                uplink_missing.push(network.sites.len());
            }
            network.sites.push(site);
        }
    }
    // Configurations from before uplinks: a site that joined with an
    // invitation (it has a manager) hangs off that site.
    for i in uplink_missing {
        let manager = network.sites.get(i).and_then(|s| s.managed_by.clone());
        if let Some(m) = manager
            && network.site(&m).is_some()
            && network.sites.get(i).is_some_and(|s| s.name != m)
            && let Some(site) = network.sites.get_mut(i)
        {
            site.uplink = Some(m);
        }
    }
    for (i, raw_map) in raw.map.into_iter().enumerate() {
        let path = FieldPath::map(i);
        if let Some(map) = lw
            .take(&path, Some(raw_map))
            .and_then(|m| lower_map(&mut lw, i, m))
        {
            network.maps.push(map);
        }
    }
    for (i, raw_tunnel) in raw.tunnel.into_iter().enumerate() {
        let path = FieldPath::tunnel(i);
        if let Some(tunnel) = lw
            .take(&path, Some(raw_tunnel))
            .and_then(|t| lower_tunnel(&mut lw, i, t))
        {
            network.tunnels.push(tunnel);
        }
    }

    if lw.diags.iter().any(Diagnostic::is_error) {
        Err(lw.diags)
    } else {
        Ok(ParsedNetwork {
            network: Some(network),
            spans: lw.spans,
            warnings: lw.diags,
            old_format: true,
        })
    }
}

fn parse_network_2(text: &str) -> Result<ParsedNetwork, Vec<Diagnostic>> {
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
            old_format: false,
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
                    "[[{key}]] belongs to the older format of this file; write it into its site instead, e.g. {example} under [site.<name>]"
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

    let (mut map_index, mut tunnel_index) = (0, 0);
    for (i, (name, raw_site)) in sites.into_iter().enumerate() {
        lw.span(&FieldPath::site(i), raw_site.span());
        let raw_site = raw_site.into_inner();
        let site_name = name.get_ref().clone();
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
            name: Some(name),
            address: raw_site.address,
            id: raw_site.id,
            vlans: raw_site.vlans,
            own_vlans: raw_site.own_vlans,
            online: raw_site.online,
            managed_by: raw_site.managed_by,
            self_managed: raw_site.self_managed,
            rename_by_manager: raw_site.rename_by_manager,
            uplink: raw_site.server,
            tunnel_mode: raw_site.tunnel_mode,
            role: None,
            hub: None,
        };
        if let Some((site, _)) = lower_site(&mut lw, i, site) {
            network.sites.push(site);
        }
        if let Some(map) = raw_site.map {
            let table = map.span();
            for (source, mapped) in map.into_inner().0 {
                if let Some(map) = lower_map_2(
                    &mut lw,
                    map_index,
                    &site_name,
                    table.clone(),
                    source,
                    mapped,
                ) {
                    network.maps.push(map);
                }
                map_index += 1;
            }
        }
        if let Some(limits) = raw_site.tunnel_vlans {
            let table = limits.span();
            for (other, vlans) in limits.into_inner().0 {
                if let Some(tunnel) = lower_tunnel_2(
                    &mut lw,
                    tunnel_index,
                    &site_name,
                    table.clone(),
                    other,
                    vlans,
                ) {
                    if network.tunnels.iter().any(|t| t.sites == tunnel.sites) {
                        let [a, b] = &tunnel.sites;
                        lw.error(
                            FieldPath::tunnel_field(tunnel_index, "between"),
                            format!(
                                "the tunnel between \"{a}\" and \"{b}\" is limited at both sites; keep one `tunnel_vlans` entry"
                            ),
                        );
                    } else {
                        network.tunnels.push(tunnel);
                    }
                }
                tunnel_index += 1;
            }
        }
    }

    if lw.diags.iter().any(Diagnostic::is_error) {
        Err(lw.diags)
    } else {
        Ok(ParsedNetwork {
            network: Some(network),
            spans: lw.spans,
            warnings: lw.diags,
            old_format: false,
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
) -> Option<VlanMap> {
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
    Some(VlanMap {
        site: site.to_string(),
        local: local?,
        shared: shared?,
    })
}

/// One entry of `tunnel_vlans = { <other site> = "<VLANs>" | "none" }`.
fn lower_tunnel_2(
    lw: &mut Lowering,
    i: usize,
    site: &str,
    table: Range<usize>,
    other: Spanned<String>,
    vlans: Spanned<RawVlans>,
) -> Option<TunnelVlans> {
    lw.span(&FieldPath::tunnel(i), table);
    let between_path = FieldPath::tunnel_field(i, "between");
    lw.span(&between_path, other.span());
    let vlans_path = FieldPath::tunnel_field(i, "vlans");
    lw.span(&vlans_path, vlans.span());
    let other = other.into_inner();
    let label = format!("site \"{site}\": tunnel_vlans");
    let sites = if other == site {
        lw.error(
            between_path,
            format!("{label}: a tunnel connects two sites, not \"{site}\" with itself"),
        );
        None
    } else {
        Some(site_pair(site, &other))
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
    Some(TunnelVlans {
        sites: sites?,
        vlans: vlans?,
    })
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
            .int_in_range(&path, v, 1, 65535)
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

/// Lowers one site; the flag tells whether `uplink` was given at all.
fn lower_site(lw: &mut Lowering, i: usize, s: RawSite) -> Option<(Site, bool)> {
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
    let online = lw
        .take(&FieldPath::site_field(i, "online"), s.online)
        .unwrap_or(true);
    let managed_by = lw.take(&FieldPath::site_field(i, "managed_by"), s.managed_by);
    let self_managed = lw
        .take(&FieldPath::site_field(i, "self_managed"), s.self_managed)
        .unwrap_or(false);
    let mode_path = FieldPath::site_field(i, "tunnel_mode");
    let mode_text = lw.take(&mode_path, s.tunnel_mode);
    // Written by 1.0.22; renaming follows `managed_by` now.
    let rename_path = FieldPath::site_field(i, "rename_by_manager");
    let rename_by_manager = lw.take(&rename_path, s.rename_by_manager).is_some();
    // `uplink = ""` marks a server explicitly.
    let uplink_given = s.uplink.is_some();
    let uplink = lw
        .take(&FieldPath::site_field(i, "uplink"), s.uplink)
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
    if rename_by_manager {
        lw.warning(
            rename_path,
            format!(
                "{label}: `rename_by_manager` is no longer used and is ignored; its manager (`managed_by`) may rename it"
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
    let id = id_text.and_then(|text| match text.parse::<SiteId>() {
        Ok(id) => Some(id),
        Err(e) => {
            lw.error(id_path, format!("{label}: id \"{text}\" is {e}"));
            None
        }
    });
    let tunnel_mode = match mode_text.map(|t| t.parse::<TunnelMode>()) {
        None => TunnelMode::default(),
        Some(Ok(mode)) => mode,
        Some(Err(e)) => {
            lw.error(mode_path, format!("{label}: tunnel_mode: {e}"));
            TunnelMode::default()
        }
    };

    Some((
        Site {
            name: name?,
            address: address?,
            id,
            vlans,
            own_vlans,
            online,
            managed_by,
            self_managed,
            uplink,
            tunnel_mode,
        },
        uplink_given,
    ))
}

fn lower_map(lw: &mut Lowering, i: usize, m: RawMap) -> Option<VlanMap> {
    let site_path = FieldPath::map_field(i, "site");
    let site = lw.take(&site_path, m.site);
    let local_path = FieldPath::map_field(i, "local");
    let local = lw.take(&local_path, m.local);
    let shared_path = FieldPath::map_field(i, "shared");
    let shared = lw.take(&shared_path, m.shared);

    let label = format!("map #{}", i + 1);
    let site = lw.required(site_path, site, &format!("{label}: site"));
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

    Some(VlanMap {
        site: site?,
        local: local?,
        shared: shared?,
    })
}

fn lower_tunnel(lw: &mut Lowering, i: usize, t: RawTunnel) -> Option<TunnelVlans> {
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
            [a, b] if a != b => Some(site_pair(a, b)),
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
    Some(TunnelVlans {
        sites: sites?,
        vlans: vlans?,
    })
}

/// Parses `local.toml`.
pub fn parse_local(text: &str) -> Result<ParsedLocal, Vec<Diagnostic>> {
    let mut lw = Lowering::new(ConfigFile::Local, text);
    let raw: RawLocal = match toml::from_str(text) {
        Ok(raw) => raw,
        Err(e) => return Err(vec![toml_error(ConfigFile::Local, &lw.spans, &e)]),
    };
    let self_path = FieldPath::local("self");
    let self_name = lw.take(&self_path, raw.self_name);
    let trunk_path = FieldPath::local("trunk");
    let trunk = lw
        .take(&trunk_path, raw.trunk)
        .map(RawPorts::into_vec)
        .unwrap_or_default();
    let mut tunnel_trunks = std::collections::BTreeMap::new();
    for (site, tunnel) in raw.tunnels.unwrap_or_default() {
        let path = FieldPath::local(&format!("tunnels.{site}.trunk"));
        if let Some(t) = lw.take(&path, tunnel.trunk) {
            tunnel_trunks.insert(site, t.into_vec());
        }
    }
    let local = Local {
        self_name,
        trunk,
        tunnel_trunks,
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
