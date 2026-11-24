//! Test data: site `heim` in a network of six sites, with peers in
//! different states.

use wanderlan_core::api::{
    ConfigTexts, Event, Interface, Level, MacEntry, PeerStatus, PortState, SiteState, SiteSummary,
    Status,
};
use wanderlan_core::config::{Local, fingerprint, parse_mesh, render_local, render_mesh};

/// `mesh.toml` of the fixture mesh (revision 7).
pub const MESH: &str = r#"
[mesh]
revision = 7
vlans = "10-50"
mtu_probe_interval = "5m"
commit_timeout = "60s"

[[site]]
name = "heim"
address = "10.8.0.1"

[[site]]
name = "buero"
address = "10.8.0.2"

[[site]]
name = "haus2"
address = "10.8.0.11"
vlans = "20,30"

[[site]]
name = "haus3"
address = "10.8.0.12"

[[site]]
name = "haus5"
address = "10.8.0.15"

[[site]]
name = "haus6"
address = "10.8.0.16"
online = false
managed_by = "buero"

[[map]]
site = "haus3"
local = "native"
shared = 20

[[map]]
site = "haus3"
local = 130
shared = 30
"#;

/// The running configuration as the daemon sends it (canonical form).
pub fn config_texts() -> ConfigTexts {
    let mesh = parse_mesh(MESH).unwrap().mesh.unwrap();
    let local = Local {
        self_name: Some("heim".into()),
        trunk: Some("ens19".into()),
        ..Default::default()
    };
    ConfigTexts {
        configured: true,
        mesh: render_mesh(&mesh),
        local: render_local(&local),
        revision: 7,
        fingerprint: fingerprint(&mesh),
    }
}

/// The answer of `get_config` on a site without configuration.
pub fn unconfigured_texts() -> ConfigTexts {
    ConfigTexts {
        configured: false,
        mesh: String::new(),
        local: "# wanderlan site-local configuration.\n".into(),
        revision: 0,
        fingerprint: String::new(),
    }
}

fn peer(name: &str, address: &str) -> PeerStatus {
    PeerStatus {
        name: name.into(),
        address: address.into(),
        port: format!("gt-{name}"),
        port_state: PortState::Up,
        vlans: "10-50".into(),
        trunk: "ens19".into(),
        reachable: true,
        last_reply_secs: Some(1),
        rtt_ms: Some(12.4),
        rtt_stats: Some(wanderlan_core::api::RttStats {
            min_ms: 11.2,
            avg_ms: 12.6,
            max_ms: 15.9,
            jitter_ms: 0.7,
            loss_percent: 0.0,
            samples: 60,
        }),
        traffic: wanderlan_core::api::Traffic {
            rx_bytes: 1_234_567_890,
            tx_bytes: 345_678_901,
            rx_packets: 1_000_000,
            tx_packets: 400_000,
            ..wanderlan_core::api::Traffic::default()
        },
        pmtu: Some(1420),
        mss: Some(1338),
        macs: 0,
        rx_bps: 0,
        tx_bps: 0,
        revision: Some(7),
        in_sync: Some(true),
    }
}

fn summary(name: &str, address: &str, vlans: &str) -> SiteSummary {
    SiteSummary {
        name: name.into(),
        address: address.into(),
        vlans: vlans.into(),
        online: true,
        is_self: name == "heim",
        direct: name != "heim",
    }
}

/// Status of `heim`: buero and haus3 fine, haus2 blocked by the loop guard,
/// haus5 on an older configuration, haus6 unreachable.
pub fn status() -> Status {
    Status {
        version: "0.1.0".into(),
        state: SiteState::Running,
        site: Some("heim".into()),
        address: Some("10.8.0.1".into()),
        online: true,
        trunk: Some("ens19".into()),
        vlans: Some("10-50".into()),
        revision: 7,
        fingerprint: "3f2a9c1b04de5566778899aabbccddeeff0011223344556677".into(),
        pending: None,
        last_error: None,
        peers: vec![
            PeerStatus {
                macs: 12,
                rx_bps: 1_234_000,
                tx_bps: 345_600,
                ..peer("buero", "10.8.0.2")
            },
            PeerStatus {
                port_state: PortState::Blocked,
                last_reply_secs: Some(2),
                rtt_ms: Some(3.1),
                vlans: "20,30".into(),
                ..peer("haus2", "10.8.0.11")
            },
            PeerStatus {
                rtt_ms: Some(8.7),
                macs: 4,
                rx_bps: 96_000,
                tx_bps: 31_000,
                ..peer("haus3", "10.8.0.12")
            },
            PeerStatus {
                last_reply_secs: Some(3),
                rtt_ms: Some(25.0),
                pmtu: Some(1392),
                mss: Some(1310),
                macs: 3,
                rx_bps: 52_000,
                tx_bps: 18_000,
                revision: Some(6),
                in_sync: Some(false),
                ..peer("haus5", "10.8.0.15")
            },
            PeerStatus {
                port_state: PortState::Down,
                reachable: false,
                last_reply_secs: Some(340),
                rtt_ms: None,
                pmtu: None,
                mss: None,
                revision: None,
                in_sync: None,
                ..peer("haus6", "10.8.0.16")
            },
        ],
        sites: vec![
            summary("heim", "10.8.0.1", "10-50"),
            summary("buero", "10.8.0.2", "10-50"),
            summary("haus2", "10.8.0.11", "20,30"),
            summary("haus3", "10.8.0.12", "10-50"),
            summary("haus5", "10.8.0.15", "10-50"),
            summary("haus6", "10.8.0.16", "10-50"),
        ],
        blocked: vec!["gt-haus2".into()],
        invites: 1,
        trunk_traffic: None,
        trunks: Vec::new(),
    }
}

/// Status of a site that still waits for the setup wizard.
pub fn unconfigured_status() -> Status {
    Status {
        version: "0.1.0".into(),
        state: SiteState::Unconfigured,
        site: None,
        address: None,
        online: false,
        trunk: None,
        vlans: None,
        revision: 0,
        fingerprint: String::new(),
        pending: None,
        last_error: None,
        peers: Vec::new(),
        sites: Vec::new(),
        blocked: Vec::new(),
        invites: 0,
        trunk_traffic: None,
        trunks: Vec::new(),
    }
}

fn iface(name: &str, kind: &str, up: bool, master: Option<&str>, managed: bool) -> Interface {
    Interface {
        name: name.into(),
        kind: kind.into(),
        up,
        master: master.map(str::to_string),
        mtu: 1500,
        managed,
    }
}

/// Interfaces of the host, including some that must not be offered.
pub fn interfaces() -> Vec<Interface> {
    vec![
        iface("lo", "", true, None, false),
        iface("ens18", "", true, None, false),
        iface("ens19", "", true, None, false),
        iface("eth1", "", false, Some("br-lan"), false),
        iface("br-lan", "bridge", true, None, false),
        iface("br0", "bridge", true, None, true),
        iface("gt-buero", "gretap", true, Some("br0"), true),
    ]
}

fn mac(mac: &str, vlan: Option<u16>, port: &str, age: Option<u64>) -> MacEntry {
    MacEntry {
        mac: mac.into(),
        vlan,
        port: port.into(),
        age_secs: age,
    }
}

/// A small MAC table.
pub fn macs() -> Vec<MacEntry> {
    vec![
        mac("52:54:00:12:34:56", Some(20), "gt-buero", Some(12)),
        mac("02:00:00:00:00:01", Some(10), "ens19", Some(0)),
        mac("52:54:00:ab:cd:ef", Some(20), "gt-haus2", Some(45)),
        mac("52:54:00:aa:00:01", Some(30), "gt-buero", Some(130)),
        mac("3c:ec:ef:00:11:22", None, "ens19", None),
        mac("de:ad:be:ef:00:01", Some(10), "gt-haus5", Some(5)),
    ]
}

/// Base time of the fixture events: 2026-11-24 10:00:00 UTC.
pub const T0: u64 = 1_795_514_400_000;

/// A short event history.
pub fn events() -> Vec<Event> {
    let ev = |secs: u64, level, peer: Option<&str>, message: &str| Event {
        time_ms: T0 + secs * 1000,
        level,
        peer: peer.map(str::to_string),
        message: message.into(),
    };
    vec![
        ev(0, Level::Info, None, "daemon started, mesh revision 7"),
        ev(2, Level::Info, Some("buero"), "tunnel gt-buero is up"),
        ev(
            3,
            Level::Info,
            Some("buero"),
            "path MTU to buero is 1420, MSS 1338",
        ),
        ev(
            65,
            Level::Warn,
            Some("haus5"),
            "haus5 runs mesh revision 6 with a different configuration; it will be updated by the sync",
        ),
        ev(
            130,
            Level::Error,
            Some("haus2"),
            "loop detected on gt-haus2: own frames came back; the port is blocked",
        ),
        ev(
            400,
            Level::Warn,
            Some("haus6"),
            "haus6 does not answer control probes",
        ),
    ]
}
