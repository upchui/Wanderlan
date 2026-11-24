//! Raw TOML structures with source spans and their conversion into the model.
//!
//! All keys are optional at this level so that missing values produce our own
//! messages with line numbers instead of generic serde errors. Unknown keys are
//! rejected to catch typos.

use std::net::Ipv4Addr;
use std::ops::Range;
use std::time::Duration;

use serde::Deserialize;
use serde_spanned::Spanned;

use super::diag::{ConfigFile, Diagnostic, FieldPath, SpanMap};
use super::model::{
    DEFAULT_COMMIT_TIMEOUT, DEFAULT_CONTROL_PORT, DEFAULT_MTU_PROBE_INTERVAL, Local, LocalVid,
    Mesh, Role, Site, VlanMap,
};
use super::vlan::{VID_MAX, VID_MIN, VlanSet};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMeshFile {
    mesh: Option<Spanned<RawMeshSection>>,
    #[serde(default)]
    site: Vec<Spanned<RawSite>>,
    #[serde(default)]
    map: Vec<Spanned<RawMap>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMeshSection {
    revision: Option<Spanned<i64>>,
    control_port: Option<Spanned<i64>>,
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
    role: Option<Spanned<String>>,
    hub: Option<Spanned<String>>,
    address: Option<Spanned<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMap {
    site: Option<Spanned<String>>,
    local: Option<Spanned<RawLocalVid>>,
    shared: Option<Spanned<i64>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawLocalVid {
    Num(i64),
    Text(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLocal {
    #[serde(rename = "self")]
    self_name: Option<Spanned<String>>,
    trunk: Option<Spanned<String>>,
}

/// Result of parsing `mesh.toml`.
#[derive(Debug, Clone)]
pub struct ParsedMesh {
    /// The mesh, or `None` if the file contains no configuration at all
    /// (e.g. the commented template shipped with the package).
    pub mesh: Option<Mesh>,
    /// Source positions for line numbers in later diagnostics.
    pub spans: SpanMap,
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

/// Parses `mesh.toml`. Returns all syntax and type errors at once.
pub fn parse_mesh(text: &str) -> Result<ParsedMesh, Vec<Diagnostic>> {
    let mut lw = Lowering::new(ConfigFile::Mesh, text);
    let raw: RawMeshFile = match toml::from_str(text) {
        Ok(raw) => raw,
        Err(e) => return Err(vec![toml_error(ConfigFile::Mesh, &lw.spans, &e)]),
    };

    if raw.mesh.is_none() && raw.site.is_empty() && raw.map.is_empty() {
        return Ok(ParsedMesh {
            mesh: None,
            spans: lw.spans,
        });
    }

    let mesh_path = FieldPath::mesh("");
    let section = lw.take(&mesh_path, raw.mesh);
    let mut mesh = Mesh::new(VlanSet::new());
    match section {
        None => lw.error(
            FieldPath::root(),
            "the [mesh] table is missing (it needs at least `vlans = \"10-50\"`)",
        ),
        Some(section) => lower_section(&mut lw, section, &mut mesh),
    }

    for (i, raw_site) in raw.site.into_iter().enumerate() {
        let path = FieldPath::site(i);
        if let Some(site) = lw
            .take(&path, Some(raw_site))
            .and_then(|s| lower_site(&mut lw, i, s))
        {
            mesh.sites.push(site);
        }
    }
    for (i, raw_map) in raw.map.into_iter().enumerate() {
        let path = FieldPath::map(i);
        if let Some(map) = lw
            .take(&path, Some(raw_map))
            .and_then(|m| lower_map(&mut lw, i, m))
        {
            mesh.maps.push(map);
        }
    }

    if lw.diags.is_empty() {
        Ok(ParsedMesh {
            mesh: Some(mesh),
            spans: lw.spans,
        })
    } else {
        Err(lw.diags)
    }
}

fn lower_section(lw: &mut Lowering, s: RawMeshSection, mesh: &mut Mesh) {
    let path = FieldPath::mesh("revision");
    if let Some(v) = lw.take(&path, s.revision) {
        mesh.revision = lw.int_in_range(&path, v, 0, i64::MAX).unwrap_or(0);
    }

    let path = FieldPath::mesh("control_port");
    mesh.control_port = match lw.take(&path, s.control_port) {
        Some(v) => lw
            .int_in_range(&path, v, 1, 65535)
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(DEFAULT_CONTROL_PORT),
        None => DEFAULT_CONTROL_PORT,
    };

    let path = FieldPath::mesh("vlans");
    let vlans = lw.take(&path, s.vlans);
    if let Some(vlans) = lw.required(path.clone(), vlans, "mesh.vlans (e.g. \"10-50\")") {
        let parsed = match vlans {
            RawVlans::Text(text) => text.parse::<VlanSet>(),
            RawVlans::List(items) => {
                let mut set = VlanSet::new();
                items
                    .iter()
                    .try_for_each(|item| match item {
                        RawVlanItem::Num(n) => set.insert_checked(*n),
                        RawVlanItem::Text(t) => set.insert_item(t),
                    })
                    .and_then(|()| {
                        if set.is_empty() {
                            Err(super::vlan::VlanSetError::Empty)
                        } else {
                            Ok(set)
                        }
                    })
            }
        };
        match parsed {
            Ok(set) => mesh.vlans = set,
            Err(e) => lw.error(path, format!("mesh.vlans: {e}")),
        }
    }

    let path = FieldPath::mesh("mtu_probe_interval");
    mesh.mtu_probe_interval = match lw.take(&path, s.mtu_probe_interval) {
        Some(text) => lw
            .duration(&path, &text)
            .unwrap_or(DEFAULT_MTU_PROBE_INTERVAL),
        None => DEFAULT_MTU_PROBE_INTERVAL,
    };

    let path = FieldPath::mesh("commit_timeout");
    mesh.commit_timeout = match lw.take(&path, s.commit_timeout) {
        Some(text) => lw.duration(&path, &text).unwrap_or(DEFAULT_COMMIT_TIMEOUT),
        None => DEFAULT_COMMIT_TIMEOUT,
    };

    let path = FieldPath::mesh("mss");
    if let Some(v) = lw.take(&path, s.mss) {
        mesh.mss = lw
            .int_in_range(&path, v, 1, 65535)
            .and_then(|v| u16::try_from(v).ok());
    }
}

fn lower_site(lw: &mut Lowering, i: usize, s: RawSite) -> Option<Site> {
    let name_path = FieldPath::site_field(i, "name");
    let name = lw.take(&name_path, s.name);
    let role_path = FieldPath::site_field(i, "role");
    let role = lw.take(&role_path, s.role);
    let hub_path = FieldPath::site_field(i, "hub");
    let hub = lw.take(&hub_path, s.hub);
    let addr_path = FieldPath::site_field(i, "address");
    let address = lw.take(&addr_path, s.address);

    let label = name
        .as_deref()
        .map_or_else(|| format!("site #{}", i + 1), |n| format!("site \"{n}\""));
    let name = lw.required(name_path, name, &format!("{label}: name"));
    let role = lw
        .required(
            role_path.clone(),
            role,
            &format!("{label}: role (\"hub\" or \"spoke\")"),
        )
        .and_then(|r| match r.parse::<Role>() {
            Ok(role) => Some(role),
            Err(e) => {
                lw.error(role_path, format!("{label}: {e}"));
                None
            }
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

    Some(Site {
        name: name?,
        role: role?,
        hub,
        address: address?,
    })
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
    let trunk = lw.take(&trunk_path, raw.trunk);
    let local = Local { self_name, trunk };
    if lw.diags.is_empty() {
        Ok(ParsedLocal {
            local,
            spans: lw.spans,
        })
    } else {
        Err(lw.diags)
    }
}
