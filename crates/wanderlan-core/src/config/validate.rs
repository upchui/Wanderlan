//! Semantic validation of a parsed configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::diag::{ConfigFile, Diagnostic, FieldPath};
use super::model::{Local, LocalVid, MAX_SITE_NAME_LEN, Network, Site};
use super::vlan::NATIVE_VID;
use crate::identity::SiteId;
use crate::topology::TUNNEL_PREFIX;

/// Smallest accepted fixed MSS.
pub const MSS_MIN: u16 = 536;
/// Largest accepted fixed MSS (1500 byte inner MTU minus 40).
pub const MSS_MAX: u16 = 1460;
/// Smallest accepted MTU probe interval.
pub const PROBE_INTERVAL_MIN: Duration = Duration::from_secs(10);
/// Longest interval between path MTU measurements.
pub const PROBE_INTERVAL_MAX: Duration = Duration::from_secs(24 * 60 * 60);
/// Accepted range for the commit timeout.
pub const COMMIT_TIMEOUT_RANGE: (Duration, Duration) =
    (Duration::from_secs(10), Duration::from_secs(3600));
/// Maximum interface name length (IFNAMSIZ - 1).
pub const IFNAME_MAX: usize = 15;

/// Validates the network and, if given and configured, the local configuration.
///
/// The returned diagnostics have no line numbers; [`super::load`] fills them
/// in from the source spans. The TUI uses the [`FieldPath`]s directly.
pub fn validate(network: &Network, local: Option<&Local>) -> Vec<Diagnostic> {
    let mut v = Validator::default();
    v.network_settings(network);
    v.sites(network);
    v.uplinks(network);
    v.maps(network);
    v.tunnels(network);
    v.passing_on(network);
    if let Some(local) = local.filter(|l| l.is_configured()) {
        v.local(network, local);
    }
    v.diags
}

/// Checks whether a string is a valid site name.
pub fn site_name_error(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("site name is empty".into());
    }
    if name.len() > MAX_SITE_NAME_LEN {
        return Some(format!(
            "site name \"{name}\" is longer than {MAX_SITE_NAME_LEN} characters (it names its tunnels on sites of older versions: {TUNNEL_PREFIX}{name})"
        ));
    }
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let valid_start = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !valid_chars || !valid_start {
        return Some(format!(
            "site name \"{name}\" may only contain a-z, 0-9 and '-' and must not start with '-'"
        ));
    }
    None
}

/// Checks whether a string is a usable network interface name.
pub fn ifname_error(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("interface name is empty".into());
    }
    if name.len() > IFNAME_MAX {
        return Some(format!(
            "interface name \"{name}\" is longer than {IFNAME_MAX} characters"
        ));
    }
    if name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c == '/' || c == ':' || c.is_whitespace())
    {
        return Some(format!("\"{name}\" is not a valid interface name"));
    }
    None
}

#[derive(Default)]
struct Validator {
    diags: Vec<Diagnostic>,
}

impl Validator {
    fn error(&mut self, path: FieldPath, message: impl Into<String>) {
        self.diags
            .push(Diagnostic::error(ConfigFile::Network, path, message));
    }

    fn warning(&mut self, path: FieldPath, message: impl Into<String>) {
        self.diags
            .push(Diagnostic::warning(ConfigFile::Network, path, message));
    }

    fn network_settings(&mut self, network: &Network) {
        if network.vlans.is_empty() {
            self.error(
                FieldPath::network("vlans"),
                "network.vlans must contain at least one VLAN",
            );
        }
        if network.mtu_probe_interval < PROBE_INTERVAL_MIN
            || network.mtu_probe_interval > PROBE_INTERVAL_MAX
        {
            self.error(
                FieldPath::network("mtu_probe_interval"),
                format!(
                    "network.mtu_probe_interval must be between {} and {}",
                    humantime::format_duration(PROBE_INTERVAL_MIN),
                    humantime::format_duration(PROBE_INTERVAL_MAX)
                ),
            );
        }
        let (min, max) = COMMIT_TIMEOUT_RANGE;
        if network.commit_timeout < min || network.commit_timeout > max {
            self.error(
                FieldPath::network("commit_timeout"),
                format!(
                    "network.commit_timeout must be between {} and {}",
                    humantime::format_duration(min),
                    humantime::format_duration(max)
                ),
            );
        }
        let tunnel_port = network.tunnel_port();
        if tunnel_port == network.control_port
            || tunnel_port.checked_add(1) == Some(network.control_port)
        {
            self.error(
                FieldPath::network("tunnel_port"),
                format!(
                    "network.tunnel_port and the port above it must differ from the control port ({}): a tunnel through NAT sends from both",
                    network.control_port
                ),
            );
        } else if tunnel_port == u16::MAX {
            self.error(
                FieldPath::network("tunnel_port"),
                "network.tunnel_port must be at most 65534: a tunnel through NAT also sends from the port above it",
            );
        }
        if let Some(mss) = network.mss.filter(|m| !(MSS_MIN..=MSS_MAX).contains(m)) {
            self.error(
                FieldPath::network("mss"),
                format!("network.mss must be between {MSS_MIN} and {MSS_MAX}, got {mss}"),
            );
        }
    }

    fn sites(&mut self, network: &Network) {
        if network.sites.is_empty() {
            self.error(FieldPath::root(), "network.toml defines no site");
            return;
        }
        let mut names: BTreeMap<&str, usize> = BTreeMap::new();
        // Every site seen at an address: each new one is compared with all.
        let mut addresses: BTreeMap<std::net::Ipv4Addr, Vec<&Site>> = BTreeMap::new();
        let mut ids: BTreeMap<SiteId, &str> = BTreeMap::new();
        let mut ifnames: BTreeMap<String, &str> = BTreeMap::new();
        for (i, site) in network.sites.iter().enumerate() {
            let name = site.name.as_str();
            if let Some(msg) = site_name_error(name) {
                self.error(FieldPath::site_field(i, "name"), msg);
            }
            // Sites with identities are told apart by them; a site of an
            // older version is known by its name alone.
            if let Some(first) = names.insert(name, i)
                && !(site.has_identity() && network.sites[first].has_identity())
            {
                self.error(
                    FieldPath::site_field(i, "name"),
                    format!(
                        "site \"{name}\" is defined twice (also site #{}); two sites may only share a name once both have an identity (a wanderlan version that has one)",
                        first + 1
                    ),
                );
            }
            if site.has_identity()
                && !ids.contains_key(&site.id)
                && let Some(other) = ifnames.insert(site.id.ifname_part(), name)
            {
                self.error(
                    FieldPath::site_field(i, "id"),
                    format!(
                        "sites \"{other}\" and \"{name}\" have identities that start alike and would get the same tunnel interface; one of them joins again with a new one (`wanderlan reset` there, then `wanderlan join`)"
                    ),
                );
            }

            let addr = site.address;
            if addr.is_unspecified()
                || addr.is_broadcast()
                || addr.is_multicast()
                || addr.is_loopback()
            {
                self.error(
                    FieldPath::site_field(i, "address"),
                    format!("site \"{name}\": {addr} is not a usable unicast address"),
                );
            }
            // Boxes with an identity are told apart by it: several clients
            // behind one NAT share its address. Servers (and sites of older
            // versions) need their own.
            let at = addresses.entry(addr).or_default();
            let clash = at
                .iter()
                .copied()
                .find(|other| !shares_address(site, other));
            at.push(site);
            if let Some(other) = clash {
                self.error(
                    FieldPath::site_field(i, "address"),
                    format!(
                        "site \"{name}\": address {addr} is already used by site \"{}\"",
                        other.name
                    ),
                );
            }
            let id = site.id;
            if let Some(other) = ids.insert(id, name) {
                self.error(
                    FieldPath::site_field(i, "id"),
                    format!(
                        "sites \"{other}\" and \"{name}\" have the same identity {} (one box cannot be two sites; on a cloned machine run `wanderlan reset` and join again)",
                        id.fingerprint()
                    ),
                );
            }

            // Its own ports: the tunnel port and the one above it (NAT)
            // apart from the control port.
            if site.ports != super::model::SitePorts::default() {
                let control = network.control_port_of(site.id);
                let tunnel = network.tunnel_port_of(site.id);
                if tunnel == control || tunnel.checked_add(1) == Some(control) {
                    self.error(
                        FieldPath::site_field(i, "tunnel_port"),
                        format!(
                            "site \"{name}\": its tunnel port {tunnel} and the port above it must differ from its control port {control}"
                        ),
                    );
                } else if tunnel == u16::MAX {
                    self.error(
                        FieldPath::site_field(i, "tunnel_port"),
                        format!("site \"{name}\": its tunnel port must be at most 65534"),
                    );
                }
            }

            if site
                .vlans
                .as_ref()
                .is_some_and(super::vlan::VlanSet::is_empty)
            {
                self.error(
                    FieldPath::site_field(i, "vlans"),
                    format!("site \"{name}\": vlans must contain at least one VLAN"),
                );
            }
            if let Some(own) = &site.own_vlans {
                let path = FieldPath::site_field(i, "own_vlans");
                let native = network
                    .maps_of(site.id)
                    .find(|m| m.local == LocalVid::Native)
                    .map(|m| m.shared);
                if let Some(shared) = native
                    && own.contains(NATIVE_VID)
                {
                    self.error(
                        path.clone(),
                        format!(
                            "site \"{name}\": own_vlans cannot contain native: its untagged traffic is VLAN {shared}"
                        ),
                    );
                }
                let both: super::vlan::VlanSet = own
                    .iter()
                    .filter(|v| network.vlans_of(site).contains(*v))
                    .collect();
                if !both.is_empty() {
                    self.warning(
                        path,
                        format!(
                            "site \"{name}\": own_vlans {both} are already VLANs of the site (they reach its server too)"
                        ),
                    );
                }
            }
            self.breakout(network, i, site);
        }
    }

    /// The local breakout of a site: only at a client, VLANs of the site,
    /// usable uplink names, not too many exceptions.
    fn breakout(&mut self, network: &Network, i: usize, site: &Site) {
        let name = &site.name;
        // A server is where the gateways and firewalls of its VLANs are: its
        // devices reach them locally, a breakout would only bypass them.
        // Networks of earlier versions may have one: ignored, not an error,
        // so the server keeps running after an update.
        if crate::topology::is_server(site) {
            for vid in site.breakout.keys() {
                self.warning(
                    FieldPath::site_field(i, &format!("breakout.{vid}")),
                    format!(
                        "site \"{name}\": breakout {vid} is ignored: \"{name}\" is a server, its devices reach the internet through their own gateway; only clients break out (wanderlan breakout {name} {vid} off removes it)"
                    ),
                );
            }
            return;
        }
        let vlans: super::vlan::VlanSet = network
            .effective_vlans(site)
            .iter()
            .chain(network.own_vlans(site).iter())
            .collect();
        // A VLAN number a mapping takes at the trunk (source S carried as
        // L there) is not on the trunk as VLAN L, unless L is mapped too.
        let (mut locals, mut sources) = (BTreeSet::new(), BTreeSet::new());
        for m in network.maps_of(site.id) {
            if let LocalVid::Vid(local) = m.local {
                locals.insert(local);
                sources.insert(m.shared);
            }
        }
        for (vid, b) in &site.breakout {
            let path = FieldPath::site_field(i, &format!("breakout.{vid}"));
            if !vlans.contains(*vid) {
                self.error(
                    path.clone(),
                    format!(
                        "site \"{name}\": breakout: VLAN {vid} is not one of the VLANs of the site ({vlans})"
                    ),
                );
            } else if locals.contains(vid) && !sources.contains(vid) {
                self.warning(
                    path.clone(),
                    format!(
                        "site \"{name}\": breakout {vid} does nothing: at this site VLAN {vid} carries a mapped source VLAN, so VLAN {vid} of the network is not on its trunk"
                    ),
                );
            }
            if let Some(uplink) = &b.uplink {
                let path = FieldPath::site_field(i, &format!("breakout.{vid}.uplink"));
                if let Some(msg) = ifname_error(uplink) {
                    self.error(
                        path,
                        format!("site \"{name}\": breakout {vid}: uplink: {msg}"),
                    );
                } else if is_managed_name(uplink) {
                    self.error(
                        path,
                        format!(
                            "site \"{name}\": breakout {vid}: uplink \"{uplink}\" is an interface of wanderlan"
                        ),
                    );
                }
            }
            if b.except.len() > crate::breakout::MAX_EXCEPT {
                self.error(
                    FieldPath::site_field(i, &format!("breakout.{vid}.except")),
                    format!(
                        "site \"{name}\": breakout {vid}: at most {} exceptions",
                        crate::breakout::MAX_EXCEPT
                    ),
                );
            }
        }
    }

    /// Every uplink names another site, and following the uplinks always
    /// ends at a server (no loop).
    fn uplinks(&mut self, network: &Network) {
        for (i, site) in network.sites.iter().enumerate() {
            let Some(up) = site.uplink else {
                continue;
            };
            let path = FieldPath::site_field(i, "uplink");
            if up == site.id {
                self.error(
                    path,
                    format!("site \"{}\" cannot be its own server", site.name),
                );
                continue;
            }
            if network.site(up).is_none() {
                self.error(
                    path,
                    format!(
                        "site \"{}\": server \"{}\" is not a site of the network",
                        site.name,
                        network.label(up)
                    ),
                );
                continue;
            }
            // Follow the chain; a loop is reported at each of its sites.
            let mut chain = vec![site.id];
            let mut current = up;
            loop {
                if chain.contains(&current) {
                    if chain.first() == Some(&current) {
                        chain.push(current);
                        let names: Vec<String> =
                            chain.iter().map(|id| network.label(*id)).collect();
                        self.error(
                            path,
                            format!("the servers form a loop: {}", names.join(" -> ")),
                        );
                    }
                    break;
                }
                chain.push(current);
                match network.site(current).and_then(|s| s.uplink) {
                    Some(next) => current = next,
                    None => break,
                }
            }
        }
        // A manager that is not a site any more is allowed (its sites are
        // open again); one that is a name of a file of an older format the
        // site keeps as it is.
    }

    /// Sites that do not reach each other in a VLAN because the tunnels
    /// carrying it form a ring (only with limits per tunnel).
    fn passing_on(&mut self, network: &Network) {
        if network.tunnels.is_empty() {
            return;
        }
        for (vlans, a, b, pairs) in crate::forwarding::unreachable(network) {
            let more = if pairs > 1 {
                format!(" ({pairs} pairs in all)")
            } else {
                String::new()
            };
            // A direct tunnel whose limit leaves these VLANs out: that
            // limit is the fix.
            let fix = match network.tunnel_limit(a, b) {
                Some(limit) if crate::topology::tunnel_vlans(network, a, b).is_some() => {
                    format!(
                        "the limit of their direct tunnel ({limit}) leaves them out; add them to it or break the ring"
                    )
                }
                _ => "give them their direct tunnel or break the ring".to_string(),
            };
            let (a, b) = (network.label(a), network.label(b));
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            self.warning(
                FieldPath::network("forwarding"),
                format!(
                    "VLANs {vlans}: \"{a}\" and \"{b}\" do not reach each other{more}: their tunnels form a ring, which no site passes on; {fix}"
                ),
            );
        }
    }

    fn tunnels(&mut self, network: &Network) {
        let mut seen: BTreeMap<&[SiteId; 2], usize> = BTreeMap::new();
        for (i, tunnel) in network.tunnels.iter().enumerate() {
            let n = i + 1;
            let path = FieldPath::tunnel_field(i, "between");
            let [a_id, b_id] = tunnel.sites;
            let (a, b) = (network.label(a_id), network.label(b_id));
            if let Some(missing) = tunnel.sites.iter().find(|s| network.site(**s).is_none()) {
                let missing = network.label(*missing);
                self.error(
                    path,
                    format!("tunnel limit between \"{a}\" and \"{b}\": site \"{missing}\" does not exist"),
                );
                continue;
            }
            if let Some(first) = seen.get(&tunnel.sites) {
                self.error(
                    path,
                    format!(
                        "the tunnel between \"{a}\" and \"{b}\" is limited twice (tunnel #{} and #{n})",
                        first + 1
                    ),
                );
                continue;
            }
            seen.insert(&tunnel.sites, i);
            if crate::topology::tunnel_vlans(network, a_id, b_id).is_none() {
                self.warning(
                    path,
                    format!(
                        "tunnel limit between \"{a}\" and \"{b}\": they have no tunnel with each other; this limit has no effect"
                    ),
                );
            }
        }
    }

    fn maps(&mut self, network: &Network) {
        // Per site: local side -> map index, shared VID -> map index.
        let mut locals: BTreeMap<(SiteId, LocalVid), usize> = BTreeMap::new();
        let mut shareds: BTreeMap<(SiteId, u16), usize> = BTreeMap::new();
        for (i, map) in network.maps.iter().enumerate() {
            let id = map.site;
            let site = network.label(id);
            let site_vlans = network
                .site(id)
                .map(|s| (network.vlans_of(s), s.own_vlans.as_ref()));
            match site_vlans {
                None => self.error(
                    FieldPath::map_field(i, "site"),
                    format!("map #{}: site \"{site}\" does not exist", i + 1),
                ),
                // Its VLANs or its own VLANs (on its trunk and towards its
                // clients).
                Some((vlans, own))
                    if map.shared != NATIVE_VID
                        && !vlans.contains(map.shared)
                        && !own.is_some_and(|o| o.contains(map.shared)) =>
                {
                    self.error(
                    FieldPath::map_field(i, "shared"),
                    format!(
                        "map #{}: source VLAN {} is not one of the VLANs of site \"{site}\" ({vlans}) nor of its own VLANs",
                        i + 1,
                        map.shared,
                    ),
                    );
                }
                Some(_) => {}
            }
            // The VLAN on its trunk is a VLAN ID too (an invitation over the
            // API is not checked by the forms).
            if let LocalVid::Vid(local) = map.local
                && !(1..=4094).contains(&local)
            {
                self.error(
                    FieldPath::map_field(i, "local"),
                    format!(
                        "map #{}: mapped VLAN {local} is not a VLAN ID (1 to 4094)",
                        i + 1
                    ),
                );
            }
            if map.shared == NATIVE_VID {
                self.error(
                    FieldPath::map_field(i, "shared"),
                    format!(
                        "map #{}: VLAN 1 is the native VLAN (untagged traffic) and cannot be a source VLAN; add `native` to the VLANs of the sites instead",
                        i + 1
                    ),
                );
            }
            if map.local == LocalVid::Vid(NATIVE_VID) {
                self.error(
                    FieldPath::map_field(i, "local"),
                    format!(
                        "map #{}: VLAN 1 is the native VLAN (untagged traffic); write \"native\" as the mapped VLAN (local = \"native\")",
                        i + 1
                    ),
                );
            }
            if map.local == LocalVid::Vid(map.shared) {
                self.error(
                    FieldPath::map_field(i, "local"),
                    format!("map #{}: VLAN {} is mapped onto itself", i + 1, map.shared),
                );
            }
            if let Some(first) = locals.insert((id, map.local), i) {
                let what = match map.local {
                    LocalVid::Native => "has more than one native mapping".to_string(),
                    LocalVid::Vid(v) => format!("uses mapped VLAN {v} twice"),
                };
                self.error(
                    FieldPath::map_field(i, "local"),
                    format!("site \"{site}\" {what} (map #{} and #{})", first + 1, i + 1),
                );
            }
            if let Some(first) = shareds.insert((id, map.shared), i) {
                self.error(
                    FieldPath::map_field(i, "shared"),
                    format!(
                        "site \"{site}\" maps source VLAN {} twice (map #{} and #{})",
                        map.shared,
                        first + 1,
                        i + 1
                    ),
                );
            }
            let own_vlans = network.site(id).map(|s| network.own_vlans(s));
            if let LocalVid::Vid(local) = map.local
                && own_vlans.as_ref().is_some_and(|v| v.contains(local))
            {
                self.warning(
                    FieldPath::map_field(i, "local"),
                    format!(
                        "site \"{site}\": mapped VLAN {local} is one of its own VLANs; at this site it carries source VLAN {} instead, so the own VLAN {local} does not reach its trunk",
                        map.shared
                    ),
                );
            }
            if let LocalVid::Vid(local) = map.local
                && site_vlans.is_some_and(|(v, _)| v.contains(local))
            {
                self.warning(
                    FieldPath::map_field(i, "local"),
                    format!(
                        "site \"{site}\": mapped VLAN {local} is also a VLAN of the network; at this site it carries source VLAN {}, and VLAN {local} of the network is not available on its trunk",
                        map.shared
                    ),
                );
            }
        }
    }

    fn local(&mut self, network: &Network, local: &Local) {
        let mut push = |path: &str, msg: String| {
            self.diags.push(Diagnostic::error(
                ConfigFile::Local,
                FieldPath::local(path),
                msg,
            ));
        };
        if let Some(id) = local.self_id
            && network.site(id).is_none()
        {
            let known: BTreeSet<&str> = network.sites.iter().map(|s| s.name.as_str()).collect();
            let known: Vec<&str> = known.into_iter().collect();
            push(
                "self",
                format!(
                    "self = \"{}\" is not a site in network.toml (known sites: {})",
                    network.label(id),
                    known.join(", ")
                ),
            );
        }
        // The settings of this site alone, in the ranges of the network's.
        if let Some(t) = local.mtu_probe_interval
            && !(PROBE_INTERVAL_MIN..=PROBE_INTERVAL_MAX).contains(&t)
        {
            push(
                "mtu_probe_interval",
                format!(
                    "mtu_probe_interval must be between {} and {}",
                    humantime::format_duration(PROBE_INTERVAL_MIN),
                    humantime::format_duration(PROBE_INTERVAL_MAX)
                ),
            );
        }
        let (min, max) = COMMIT_TIMEOUT_RANGE;
        if let Some(t) = local.commit_timeout
            && !(min..=max).contains(&t)
        {
            push(
                "commit_timeout",
                format!(
                    "commit_timeout must be between {} and {}",
                    humantime::format_duration(min),
                    humantime::format_duration(max)
                ),
            );
        }
        if let Some(mss) = local.mss.filter(|m| !(MSS_MIN..=MSS_MAX).contains(m)) {
            push(
                "mss",
                format!("mss must be between {MSS_MIN} and {MSS_MAX}, got {mss}"),
            );
        }
        if local.trunk.is_empty() {
            push(
                "trunk",
                "trunk is missing (the local interface carrying the VLANs)".into(),
            );
        }
        // Every port: a valid name, not ours, once per list.
        let mut check_ports = |path: &str, what: &str, ports: &[String]| {
            // The default trunk's own message is above.
            if ports.is_empty() && path != "trunk" {
                push(
                    path,
                    format!("{what} is empty; remove the entry to use the default trunk"),
                );
            }
            let mut seen = BTreeSet::new();
            for port in ports {
                if let Some(msg) = ifname_error(port) {
                    push(path, format!("{what}: {msg}"));
                } else if is_managed_name(port) {
                    push(
                        path,
                        format!("{what}: \"{port}\" is an interface managed by wanderlan"),
                    );
                } else if !seen.insert(port.as_str()) {
                    push(path, format!("{what}: \"{port}\" is listed twice"));
                }
            }
        };
        check_ports("trunk", "trunk", &local.trunk);
        for (site, ports) in &local.tunnel_trunks {
            check_ports(
                &format!("tunnels.{site}.trunk"),
                &format!("trunk of the tunnel to \"{}\"", network.label(*site)),
                ports,
            );
        }
        // A port belongs to one segment: lists are equal or apart.
        let segments: Vec<(String, &Vec<String>)> =
            std::iter::once(("trunk".to_string(), &local.trunk))
                .chain(
                    local
                        .tunnel_trunks
                        .iter()
                        .map(|(site, ports)| (format!("tunnels.{site}.trunk"), ports)),
                )
                .collect();
        for (i, (path, ports)) in segments.iter().enumerate() {
            for (other_path, other) in &segments[..i] {
                let same: BTreeSet<&String> = ports.iter().collect();
                let theirs: BTreeSet<&String> = other.iter().collect();
                // One segment needs one list: the order names the bridge
                // (after the first port) and the main port.
                if same == theirs && ports != other {
                    push(
                        path,
                        format!(
                            "the same ports as {other_path} in another order; write them in the same order (the first is the main port)"
                        ),
                    );
                    break;
                }
                if same != theirs
                    && let Some(port) = same.intersection(&theirs).next()
                {
                    push(
                        path,
                        format!(
                            "\"{port}\" is a trunk port of two segments; a port belongs to one (the same list, or none of its ports)"
                        ),
                    );
                    break;
                }
            }
        }
        let me = local.self_id;
        // The uplink of a breakout is never a trunk port.
        if let Some((i, site)) = network
            .sites
            .iter()
            .enumerate()
            .find(|(_, s)| Some(s.id) == me)
            .filter(|(_, s)| !crate::topology::is_server(s))
        {
            let trunks: BTreeSet<&String> = local
                .trunk
                .iter()
                .chain(local.tunnel_trunks.values().flatten())
                .collect();
            for (vid, b) in &site.breakout {
                if let Some(uplink) = b.uplink.as_ref().filter(|u| trunks.contains(u)) {
                    self.error(
                        FieldPath::site_field(i, &format!("breakout.{vid}.uplink")),
                        format!(
                            "site \"{}\": breakout {vid}: uplink \"{uplink}\" is a trunk port of this site, not an uplink",
                            site.name
                        ),
                    );
                }
            }
        }
        let mut unused = Vec::new();
        for (id, ports) in &local.tunnel_trunks {
            let path = format!("tunnels.{id}.trunk");
            let site = network.label(*id);
            if !local.trunk.is_empty() && *ports == local.trunk {
                unused.push((
                    path.clone(),
                    format!("the trunk of the tunnel to \"{site}\" is the default trunk anyway"),
                ));
            }
            let is_peer = me.is_some_and(|me| {
                crate::topology::tunnels_for(network, me)
                    .iter()
                    .any(|t| t.peer.id == *id)
            });
            if !is_peer {
                unused.push((
                    path,
                    format!("there is no tunnel to \"{site}\" (not a peer of this site); this trunk is not used"),
                ));
            }
        }
        for (path, msg) in unused {
            self.diags.push(Diagnostic::warning(
                ConfigFile::Local,
                FieldPath::local(&path),
                msg,
            ));
        }
    }
}

/// True for names of interfaces wanderlan creates itself.
fn is_managed_name(name: &str) -> bool {
    crate::topology::is_tunnel_name(name)
        || name == crate::BRIDGE_NAME
        || name.starts_with(crate::desired::SEGMENT_PREFIX)
        || crate::breakout::is_breakout_name(name)
}

/// True if two sites may have the same address: both have an identity and
/// at least one of them is a client (behind the same NAT, say).
fn shares_address(a: &Site, b: &Site) -> bool {
    a.has_identity() && b.has_identity() && (a.uplink.is_some() || b.uplink.is_some())
}

#[cfg(test)]
mod tests;
