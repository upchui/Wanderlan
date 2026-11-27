use crate::config::{
    Changes, ConfigFile, Diagnostic, Loaded, Network, Severity, check, fingerprint, has_errors,
    load, parse_network, render_local, render_network,
};

const EXAMPLE_NETWORK: &str = include_str!("../testdata/network-v1.toml");
const EXAMPLE_LOCAL: &str = include_str!("../../../../../packaging/examples/local.toml");
const LOCAL: &str = "self = \"haus2\"\ntrunk = \"ens19\"\n";

/// One-based line of the first occurrence of `needle` after `after`.
fn line_of(text: &str, after: &str, needle: &str) -> usize {
    let start = text.find(after).expect("anchor present");
    let pos = start + text[start..].find(needle).expect("needle present");
    text[..pos].matches('\n').count() + 1
}

fn errors(network: &str, local: &str) -> Vec<Diagnostic> {
    match load(network, local) {
        Ok(outcome) => panic!("expected errors, got {:?}", outcome.loaded),
        Err(diags) => diags.into_iter().filter(Diagnostic::is_error).collect(),
    }
}

fn single_error(network: &str, local: &str) -> Diagnostic {
    let errs = errors(network, local);
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:#?}");
    errs.into_iter().next().expect("one error")
}

fn assert_error(network: &str, line: usize, contains: &str) {
    let err = single_error(network, LOCAL);
    assert_eq!(err.file, ConfigFile::Network);
    assert!(
        err.message.contains(contains),
        "message {:?} does not contain {contains:?}",
        err.message
    );
    assert_eq!(err.line, Some(line), "wrong line for {err}");
    assert!(
        err.to_string()
            .starts_with(&format!("network.toml:{line}: error: "))
    );
}

#[test]
fn example_is_valid() {
    let outcome = load(EXAMPLE_NETWORK, EXAMPLE_LOCAL).expect("example loads");
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    let Loaded::Configured(cfg) = outcome.loaded else {
        panic!("example must be configured");
    };
    assert_eq!(cfg.self_name(), "haus2");
    assert_eq!(cfg.trunk(), "ens19");
    assert_eq!(cfg.network.sites.len(), 4);
    assert_eq!(cfg.network.maps.len(), 2);
    assert_eq!(cfg.network.vlans.to_string(), "10-50");
    assert_eq!(cfg.network.commit_timeout.as_secs(), 60);
    assert_eq!(cfg.network.mtu_probe_interval.as_secs(), 300);
    assert_eq!(cfg.network.mss, None);
    assert_eq!(cfg.network.control_port, 51821);
}

#[test]
fn render_roundtrip_is_identical() {
    let network = parse_network(EXAMPLE_NETWORK).unwrap().network.unwrap();
    let text = render_network(&network);
    let again = parse_network(&text).unwrap().network.unwrap();
    assert_eq!(network, again);
    assert_eq!(text, render_network(&again));

    let local = crate::config::parse_local(EXAMPLE_LOCAL, Some(&network))
        .unwrap()
        .local;
    let text = render_local(&local, Some(&network));
    assert_eq!(
        crate::config::parse_local(&text, Some(&network))
            .unwrap()
            .local,
        local
    );
    // The sites are identities now, with their names as comments.
    assert!(text.contains("  # haus2\n"), "{text}");
}

#[test]
fn fingerprint_ignores_revision_and_formatting() {
    let network = parse_network(EXAMPLE_NETWORK).unwrap().network.unwrap();
    let mut bumped = network.clone();
    bumped.revision = 42;
    assert_eq!(fingerprint(&network), fingerprint(&bumped));
    let reformatted = parse_network(&render_network(&network))
        .unwrap()
        .network
        .unwrap();
    assert_eq!(fingerprint(&network), fingerprint(&reformatted));
    let mut changed = network.clone();
    changed.sites[0].name = "heim2".into();
    assert_ne!(fingerprint(&network), fingerprint(&changed));
    assert_eq!(fingerprint(&network).len(), 64);
}

#[test]
fn template_without_settings_is_unconfigured() {
    let network = "# all commented out\n# [network]\n";
    let local = "# self = \"x\"\n";
    let outcome = load(network, local).unwrap();
    assert!(matches!(
        outcome.loaded,
        Loaded::Unconfigured { network: None, .. }
    ));
}

#[test]
fn self_without_network_is_an_error() {
    let err = single_error("", LOCAL);
    assert!(err.message.contains("contains no network configuration"));
}

#[test]
fn unconfigured_local_with_network_still_validates_network() {
    let outcome = load(EXAMPLE_NETWORK, "").unwrap();
    assert!(matches!(
        outcome.loaded,
        Loaded::Unconfigured {
            network: Some(_),
            ..
        }
    ));
    let bad = EXAMPLE_NETWORK.replace("address = \"10.8.0.2\"", "address = \"nope\"");
    assert!(load(&bad, "").is_err());
}

#[test]
fn syntax_error_has_line() {
    let network = EXAMPLE_NETWORK.replace("name = \"buero\"", "name = buero");
    let err = single_error(&network, LOCAL);
    assert_eq!(err.line, Some(line_of(&network, "", "name = buero")));
}

#[test]
fn unknown_key_is_rejected_with_line() {
    let network = EXAMPLE_NETWORK.replace("address = \"10.8.0.2\"", "adress = \"10.8.0.2\"");
    let err = single_error(&network, LOCAL);
    assert!(err.message.contains("adress"), "{err}");
    assert_eq!(err.line, Some(line_of(&network, "", "adress")));
}

#[test]
fn wrong_type_is_rejected_with_line() {
    let network = EXAMPLE_NETWORK.replace("shared = 30", "shared = \"30\"");
    let err = single_error(&network, LOCAL);
    assert_eq!(err.line, Some(line_of(&network, "local = 130", "shared")));
}

#[test]
fn duplicate_site_name() {
    let network = EXAMPLE_NETWORK.replace("name = \"haus3\"", "name = \"haus2\"");
    let errs = errors(&network, LOCAL);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("\"haus2\" is defined twice")
                && e.line
                    == Some(line_of(
                        &network,
                        "address = \"10.8.0.11\"",
                        "name = \"haus2\""
                    )))
    );
}

#[test]
fn duplicate_address() {
    let network = EXAMPLE_NETWORK.replace("10.8.0.12", "10.8.0.11");
    let line = line_of(&network, "name = \"haus3\"", "address");
    assert_error(&network, line, "already used by site \"haus2\"");
}

#[test]
fn invalid_address() {
    let network = EXAMPLE_NETWORK.replace("10.8.0.2\"", "10.8.0.300\"");
    let line = line_of(&network, "", "10.8.0.300");
    assert_error(&network, line, "is not an IPv4 address");
    let network = EXAMPLE_NETWORK.replace("10.8.0.2\"", "255.255.255.255\"");
    let line = line_of(&network, "", "255.255.255.255");
    assert_error(&network, line, "not a usable unicast address");
}

#[test]
fn the_table_name_of_older_versions_is_read() {
    // Older versions call the table [mesh]; their files and the views their
    // daemons send still work, and the file is written as [network].
    let network = EXAMPLE_NETWORK.replace("[network]", "[mesh]");
    assert_ne!(network, EXAMPLE_NETWORK);
    let outcome = load(&network, LOCAL).expect("the old table name is valid");
    let Loaded::Configured(cfg) = outcome.loaded else {
        panic!("configured");
    };
    let text = render_network(&cfg.network);
    assert!(text.contains("\n[network]\n"), "{text}");
}

#[test]
fn old_role_and_hub_keys_are_ignored_with_a_warning() {
    let network = EXAMPLE_NETWORK.replace(
        "name = \"haus2\"\n",
        "name = \"haus2\"\nrole = \"spoke\"\nhub = \"heim\"\n",
    );
    let outcome = load(&network, LOCAL).expect("old keys do not make the file invalid");
    let warnings: Vec<_> = outcome
        .warnings
        .iter()
        .filter(|w| w.message.contains("no longer used"))
        .collect();
    assert_eq!(warnings.len(), 2, "{:?}", outcome.warnings);
    assert_eq!(
        warnings[0].line,
        Some(line_of(&network, "name = \"haus2\"", "role"))
    );
    let Loaded::Configured(cfg) = outcome.loaded else {
        panic!("configured");
    };
    assert_eq!(cfg.network.sites.len(), 4);
    assert!(!render_network(&cfg.network).contains("role"));
}

#[test]
fn a_single_site_is_a_valid_network() {
    let network = "[network]\nvlans = \"10\"\n\n[[site]]\nname = \"a\"\naddress = \"10.0.0.1\"\n";
    let outcome = load(network, "self = \"a\"\ntrunk = \"eth0\"\n").expect("valid");
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
}

#[test]
fn site_vlans_are_parsed_and_rendered() {
    let outcome = load(EXAMPLE_NETWORK, LOCAL).expect("example loads");
    let Loaded::Configured(cfg) = outcome.loaded else {
        panic!("configured");
    };
    let haus2 = cfg.network.by_name("haus2").expect("haus2");
    assert_eq!(cfg.network.vlans_of(haus2).to_string(), "20,30");
    let heim = cfg.network.by_name("heim").expect("heim");
    assert_eq!(cfg.network.vlans_of(heim).to_string(), "10-50");
    assert!(render_network(&cfg.network).contains("vlans = \"20,30\""));
}

#[test]
fn invalid_site_vlans_have_a_line() {
    let network = EXAMPLE_NETWORK.replace("vlans = \"20,30\"", "vlans = \"20,5000\"");
    let line = line_of(&network, "name = \"haus2\"", "vlans");
    assert_error(
        &network,
        line,
        "site \"haus2\": vlans: VLAN ID 5000 is outside 1-4094",
    );
}

#[test]
fn map_target_must_be_a_vlan_of_the_site() {
    let network = EXAMPLE_NETWORK.replace(
        "name = \"haus3\"\naddress = \"10.8.0.12\"\n",
        "name = \"haus3\"\naddress = \"10.8.0.12\"\nvlans = \"30\"\n",
    );
    let line = line_of(&network, "local = \"native\"", "shared");
    assert_error(
        &network,
        line,
        "source VLAN 20 is not one of the VLANs of site \"haus3\" (30)",
    );
}

#[test]
fn invalid_site_name() {
    let network = EXAMPLE_NETWORK
        .replace("name = \"haus3\"", "name = \"Haus_3\"")
        .replace("site = \"haus3\"", "site = \"Haus_3\"");
    let line = line_of(&network, "", "name = \"Haus_3\"");
    assert_error(&network, line, "may only contain a-z");
    let long = "averyveryverylongname";
    let network = EXAMPLE_NETWORK
        .replace("name = \"haus3\"", &format!("name = \"{long}\""))
        .replace("site = \"haus3\"", &format!("site = \"{long}\""));
    let line = line_of(&network, "", long);
    assert_error(&network, line, "longer than 12 characters");
}

#[test]
fn shared_vid_must_be_in_range() {
    let network = EXAMPLE_NETWORK.replace("shared = 30", "shared = 60");
    let line = line_of(&network, "", "shared = 60");
    assert_error(
        &network,
        line,
        "source VLAN 60 is not one of the VLANs of site \"haus3\" (10-50)",
    );
}

#[test]
fn at_most_one_native_mapping_per_site() {
    let network = EXAMPLE_NETWORK.replace("local = 130", "local = \"native\"");
    let line = line_of(&network, "shared = 20", "local = \"native\"");
    assert_error(
        &network,
        line,
        "site \"haus3\" has more than one native mapping",
    );
}

#[test]
fn two_locals_onto_one_shared_vlan() {
    let network = EXAMPLE_NETWORK.replace("shared = 30", "shared = 20");
    let line = line_of(&network, "local = 130", "shared = 20");
    assert_error(&network, line, "maps source VLAN 20 twice");
}

#[test]
fn duplicate_local_vid() {
    let network =
        format!("{EXAMPLE_NETWORK}\n[[map]]\nsite = \"haus3\"\nlocal = 130\nshared = 40\n");
    let line = line_of(&network, "shared = 30", "local = 130");
    assert_error(&network, line, "uses mapped VLAN 130 twice");
}

#[test]
fn map_onto_itself() {
    let network = EXAMPLE_NETWORK.replace("local = 130", "local = 30");
    let errs = errors(&network, LOCAL);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("mapped onto itself"))
    );
}

#[test]
fn map_site_must_exist() {
    let network = EXAMPLE_NETWORK.replacen("site = \"haus3\"", "site = \"haus9\"", 1);
    let line = line_of(&network, "", "site = \"haus9\"");
    assert_error(&network, line, "site \"haus9\" does not exist");
}

#[test]
fn local_vid_inside_shared_range_is_a_warning() {
    let network = EXAMPLE_NETWORK.replace("local = 130", "local = 15");
    let outcome = load(&network, LOCAL).expect("only a warning");
    assert_eq!(outcome.warnings.len(), 1);
    let w = &outcome.warnings[0];
    assert_eq!(w.severity, Severity::Warning);
    assert_eq!(w.line, Some(line_of(&network, "", "local = 15")));
}

#[test]
fn vlan_list_syntax_and_errors() {
    let network = EXAMPLE_NETWORK.replace("vlans = \"10-50\"", "vlans = [10, \"20-50\"]");
    assert!(load(&network, LOCAL).is_ok());
    let network = EXAMPLE_NETWORK.replace("vlans = \"10-50\"", "vlans = \"10-5000\"");
    let line = line_of(&network, "", "vlans");
    assert_error(&network, line, "outside 1-4094");
    let network = EXAMPLE_NETWORK.replace("vlans = \"10-50\"", "vlans = []");
    assert_error(&network, line, "empty VLAN list");
}

#[test]
fn without_vlans_everything_passes() {
    for network in [
        EXAMPLE_NETWORK.replace("vlans = \"10-50\"", ""),
        EXAMPLE_NETWORK.replace("vlans = \"10-50\"", "vlans = \"\""),
        EXAMPLE_NETWORK.replace("vlans = \"10-50\"", "vlans = \"all\""),
    ] {
        let outcome = load(&network, LOCAL).expect("valid");
        let Loaded::Configured(cfg) = outcome.loaded else {
            panic!("configured");
        };
        assert!(cfg.network.vlans.is_all());
        assert!(render_network(&cfg.network).contains("vlans = \"all\""));
    }
    // An empty list of a site means the default of the network.
    let network = EXAMPLE_NETWORK.replace("vlans = \"20,30\"", "vlans = \"\"");
    let Loaded::Configured(cfg) = load(&network, LOCAL).unwrap().loaded else {
        panic!("configured");
    };
    assert_eq!(cfg.network.by_name("haus2").unwrap().vlans, None);
}

#[test]
fn vlan_1_is_the_native_vlan_and_no_mapping_target() {
    let network = EXAMPLE_NETWORK.replace("shared = 30", "shared = 1");
    let line = line_of(&network, "local = 130", "shared = 1");
    assert_error(&network, line, "VLAN 1 is the native VLAN");
    let network = EXAMPLE_NETWORK.replace("local = 130", "local = 1");
    let line = line_of(&network, "", "local = 1");
    assert_error(&network, line, "write \"native\" as the mapped VLAN");
}

#[test]
fn durations_and_mss() {
    let network = EXAMPLE_NETWORK.replace("commit_timeout = \"60s\"", "commit_timeout = \"soon\"");
    let line = line_of(&network, "", "commit_timeout");
    assert_error(&network, line, "is not a duration");
    let network = EXAMPLE_NETWORK.replace("commit_timeout = \"60s\"", "commit_timeout = \"2s\"");
    assert_error(&network, line, "between 10s and 1h");
    let network = EXAMPLE_NETWORK.replace("# mss = 1300", "mss = 100");
    let line = line_of(&network, "", "mss = 100");
    assert_error(&network, line, "network.mss must be between 536 and 1460");
    let network = EXAMPLE_NETWORK.replace("# mss = 1300", "mss = 1300");
    assert!(load(&network, LOCAL).is_ok());
}

#[test]
fn self_must_exist() {
    let err = single_error(EXAMPLE_NETWORK, "self = \"haus9\"\ntrunk = \"ens19\"\n");
    assert_eq!(err.file, ConfigFile::Local);
    assert_eq!(err.line, Some(1));
    assert!(err.message.contains("self = \"haus9\" is not a site"));
    assert!(err.to_string().starts_with("local.toml:1: error: "));
}

#[test]
fn trunk_is_checked() {
    let err = single_error(EXAMPLE_NETWORK, "self = \"haus2\"\n");
    assert!(err.message.contains("trunk is missing"));
    let err = single_error(EXAMPLE_NETWORK, "self = \"haus2\"\ntrunk = \"vx-heim\"\n");
    assert_eq!(err.line, Some(2));
    assert!(err.message.contains("managed by wanderlan"));
    let err = single_error(
        EXAMPLE_NETWORK,
        "self = \"haus2\"\ntrunk = \"a-very-long-ifname\"\n",
    );
    assert!(err.message.contains("longer than 15"));
}

#[test]
fn local_unknown_key() {
    let errs = errors(
        EXAMPLE_NETWORK,
        "self = \"haus2\"\ntrunk = \"ens19\"\nwg_port = 1\n",
    );
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].line, Some(3));
}

#[test]
fn check_without_local() {
    assert!(!has_errors(&check(EXAMPLE_NETWORK, None)));
    let bad = EXAMPLE_NETWORK.replace("site = \"haus3\"", "site = \"nope\"");
    let diags = check(&bad, None);
    assert!(has_errors(&diags));
    assert!(diags[0].line.is_some());
    assert!(has_errors(&check("# nothing\n", None)));
}

#[test]
fn reports_multiple_errors_at_once() {
    let network = EXAMPLE_NETWORK
        .replace("address = \"10.8.0.12\"", "address = \"10.8.0.11\"")
        .replace("shared = 30", "shared = 60");
    assert_eq!(errors(&network, LOCAL).len(), 2);
}

#[test]
fn network_model_validation_without_text() {
    // The TUI validates the model directly; paths identify the field.
    let mut network: Network = parse_network(EXAMPLE_NETWORK).unwrap().network.unwrap();
    network.sites[0].vlans = Some(crate::config::VlanSet::new());
    let diags = crate::config::validate(&network, None);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].path.as_str(), "site[0].vlans");
    assert_eq!(diags[0].line, None);
}

const CLIENTS: &str = r#"
[network]

[[site]]
name = "alex"
address = "10.13.0.103"

[[site]]
name = "marcel"
address = "10.0.10.5"
managed_by = "alex"

[[site]]
name = "site2"
address = "10.13.0.104"
uplink = "alex"
"#;

#[test]
fn uplinks_make_clients_and_old_configurations_are_migrated() {
    let network = parse_network(CLIENTS).unwrap().network.unwrap();
    // No `uplink`, but invited by alex: a client of alex.
    assert_eq!(
        network.by_name("marcel").unwrap().uplink,
        Some("alex".into())
    );
    assert_eq!(
        network.by_name("site2").unwrap().uplink,
        Some("alex".into())
    );
    assert_eq!(network.by_name("alex").unwrap().uplink, None);
    // Who changes the tunnel: before `changes`, the manager alone (its
    // server), with the site itself if released; no manager: both.
    assert_eq!(network.by_name("marcel").unwrap().changes, Changes::Server);
    assert_eq!(network.by_name("site2").unwrap().changes, Changes::Both);
    for (extra, changes) in [
        ("self_managed = true\n", Changes::Both),
        ("changes = \"client\"\n", Changes::Client),
    ] {
        let text = CLIENTS.replace(
            "managed_by = \"alex\"\n",
            &format!("managed_by = \"alex\"\n{extra}"),
        );
        let n = parse_network(&text).unwrap().network.unwrap();
        assert_eq!(n.by_name("marcel").unwrap().changes, changes, "{extra}");
    }
    let wrong = CLIENTS.replace("uplink = \"alex\"", "uplink = \"alex\"\nchanges = \"boss\"");
    let err = single_error(&wrong, "self = \"alex\"\ntrunk = \"ens19\"\n");
    assert!(
        err.message
            .contains("changes: \"boss\" is not server, client or both"),
        "{err}"
    );
    assert_eq!(
        err.line,
        Some(line_of(&wrong, "name = \"site2\"", "changes"))
    );
    // Written explicitly from now on; the canonical form reads back the same.
    let text = render_network(&network);
    let alex = pid("alex");
    assert_eq!(
        text.matches(&format!("server = \"{alex}\"  # alex"))
            .count(),
        2,
        "{text}"
    );
    assert!(text.contains("changes = \"server\"\n") && text.contains("changes = \"both\"\n"));
    assert!(!text.contains("managed_by"), "{text}");
    assert_eq!(parse_network(&text).unwrap().network.unwrap(), network);
    // A server with a manager stays a server: no `server`, in the file and
    // in the form the sites exchange.
    let mut server = network.clone();
    server.sites[1].uplink = None;
    server.sites[1].changes = Changes::default();
    let text = render_network(&server);
    let marcel = pid("marcel");
    assert!(
        text.contains(&format!(
            "[site.\"{marcel}\"]  # no box has joined as this site yet\nname = \"marcel\"\naddress = \"10.0.10.5\"\n\n"
        )),
        "{text}"
    );
    assert_eq!(parse_network(&text).unwrap().network.unwrap(), server);
    assert_eq!(crate::config::render_network_wire(&server), text);
}

#[test]
fn uplinks_must_lead_to_a_server() {
    let local = "self = \"alex\"\ntrunk = \"ens19\"\n";
    let unknown = CLIENTS.replace("uplink = \"alex\"", "uplink = \"nowhere\"");
    let err = single_error(&unknown, local);
    assert!(
        err.message.contains("server \"nowhere\" is not a site"),
        "{err}"
    );
    assert_eq!(
        err.line,
        Some(line_of(&unknown, "name = \"site2\"", "uplink"))
    );
    let itself = CLIENTS.replace("uplink = \"alex\"", "uplink = \"site2\"");
    assert!(
        single_error(&itself, local)
            .message
            .contains("cannot be its own server")
    );
    let looped = CLIENTS
        .replace("uplink = \"alex\"", "uplink = \"marcel\"")
        .replace(
            "managed_by = \"alex\"",
            "managed_by = \"alex\"\nuplink = \"site2\"",
        );
    let errs = errors(&looped, local);
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("the servers form a loop: marcel -> site2 -> marcel")),
        "{errs:#?}"
    );
}

#[test]
fn a_tunnel_can_have_its_own_trunk() {
    let local = "self = \"haus2\"\ntrunk = \"ens19\"\n\n[tunnels.heim]\ntrunk = \"ens20\"\n";
    let outcome = load(EXAMPLE_NETWORK, local).expect("valid");
    let Loaded::Configured(cfg) = outcome.loaded else {
        panic!("configured")
    };
    assert_eq!(cfg.trunk_for("heim".into()), "ens20");
    assert_eq!(cfg.trunk_for("buero".into()), "ens19");
    // The canonical form keeps it and reads back the same.
    let rendered = render_local(&cfg.local, Some(&cfg.network));
    let heim = crate::identity::SiteId::provisional("heim");
    assert!(
        rendered.contains(&format!(
            "# The tunnel to \"heim\" has its own trunk (a segment of its own).\n[tunnels.\"{heim}\"]\ntrunk = \"ens20\""
        )),
        "{rendered}"
    );
    let again = load(EXAMPLE_NETWORK, &rendered).expect("valid");
    let Loaded::Configured(again) = again.loaded else {
        panic!("configured")
    };
    assert_eq!(again.local, cfg.local);
}

#[test]
fn the_trunk_of_a_tunnel_is_checked() {
    let local = "self = \"haus2\"\ntrunk = \"ens19\"\n\n[tunnels.heim]\ntrunk = \"vx-heim\"\n";
    let err = single_error(EXAMPLE_NETWORK, local);
    assert_eq!(err.file, ConfigFile::Local);
    assert!(err.message.contains("managed by wanderlan"), "{err}");
    assert_eq!(err.line, Some(5));
    let local = "self = \"haus2\"\ntrunk = \"ens19\"\n\n[tunnels.heim]\nport = 1\n";
    assert_eq!(errors(EXAMPLE_NETWORK, local).len(), 1, "unknown key");
    // A site that is no peer: a warning, the entry is not used.
    let local = "self = \"haus2\"\ntrunk = \"ens19\"\n\n[tunnels.nowhere]\ntrunk = \"ens20\"\n";
    let outcome = load(EXAMPLE_NETWORK, local).expect("only a warning");
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.severity == Severity::Warning
                && w.message.contains("no tunnel to \"nowhere\"")),
        "{:?}",
        outcome.warnings
    );
}

#[test]
fn own_vlans_are_checked() {
    let network = CLIENTS.replace(
        "uplink = \"alex\"",
        "uplink = \"alex\"\nvlans = \"13\"\nown_vlans = \"native,50\"",
    ) + "\n[[map]]\nsite = \"site2\"\nlocal = \"native\"\nshared = 13\n";
    let err = single_error(&network, "self = \"alex\"\ntrunk = \"ens19\"\n");
    assert!(
        err.message.contains("own_vlans cannot contain native"),
        "{err}"
    );
    let fine = network.replace("own_vlans = \"native,50\"", "own_vlans = \"13,50\"");
    let outcome = load(&fine, "self = \"alex\"\ntrunk = \"ens19\"\n").expect("only a warning");
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.message.contains("own_vlans 13 are already VLANs")),
        "{:?}",
        outcome.warnings
    );
    let parsed = parse_network(&fine).unwrap().network.unwrap();
    let text = render_network(&parsed);
    assert!(text.contains("own_vlans = \"13,50\""), "{text}");
}

#[test]
fn tunnel_limits_are_read_written_and_checked() {
    let network = format!(
        "{EXAMPLE_NETWORK}\n[[tunnel]]\nbetween = [\"heim\", \"buero\"]\nvlans = \"20-30\"\n\n[[tunnel]]\nbetween = [\"haus2\", \"heim\"]\nvlans = \"none\"\n"
    );
    let outcome = load(&network, LOCAL).expect("valid");
    let Loaded::Configured(cfg) = outcome.loaded else {
        panic!("configured");
    };
    let limit = cfg
        .network
        .tunnel_limit("buero".into(), "heim".into())
        .expect("limit");
    assert_eq!(limit.to_string(), "20-30");
    assert!(
        cfg.network
            .tunnel_limit("heim".into(), "haus2".into())
            .expect("none")
            .is_empty()
    );
    // Three servers: both limits at heim, the first of them.
    let text = render_network(&cfg.network);
    assert!(
        text.contains(&format!(
            "tunnel_vlans = {{ \"{}\" = \"20-30\", \"{}\" = \"none\" }}  # buero, haus2\n",
            pid("buero"),
            pid("haus2")
        )),
        "{text}"
    );
    let again = parse_network(&text).unwrap().network.unwrap();
    assert_eq!(again.tunnels, cfg.network.tunnels);
    assert_ne!(
        fingerprint(&again),
        fingerprint(&Network {
            tunnels: Vec::new(),
            ..again.clone()
        })
    );
}

#[test]
fn wrong_tunnel_limits_are_errors() {
    for (entry, contains) in [
        (
            "between = [\"heim\"]\nvlans = \"20\"",
            "exactly two site names",
        ),
        (
            "between = [\"heim\", \"heim\"]\nvlans = \"20\"",
            "not \"heim\" with itself",
        ),
        (
            "between = [\"heim\", \"nowhere\"]\nvlans = \"20\"",
            "site \"nowhere\" does not exist",
        ),
        ("between = [\"heim\", \"buero\"]", "vlans (e.g."),
        (
            "between = [\"heim\", \"buero\"]\nvlans = \"5000\"",
            "tunnel #1: vlans:",
        ),
    ] {
        let network = format!("{EXAMPLE_NETWORK}\n[[tunnel]]\n{entry}\n");
        let err = single_error(&network, LOCAL);
        assert!(err.message.contains(contains), "{err}");
        assert!(err.line.is_some(), "{err}");
    }
    let twice = format!(
        "{EXAMPLE_NETWORK}\n[[tunnel]]\nbetween = [\"heim\", \"buero\"]\nvlans = \"20\"\n\n[[tunnel]]\nbetween = [\"buero\", \"heim\"]\nvlans = \"30\"\n"
    );
    let err = single_error(&twice, LOCAL);
    assert!(
        err.message.contains("is limited twice (tunnel #1 and #2)"),
        "{err}"
    );
}

#[test]
fn several_trunk_ports_are_read_written_and_checked() {
    let local = "self = \"haus2\"\ntrunk = [\"ens19\", \"ens20\"]\n\n[tunnels.heim]\ntrunk = [\"ens21\", \"ens22\"]\n\n[tunnels.buero]\ntrunk = \"ens23\"\n";
    let outcome = load(EXAMPLE_NETWORK, local).expect("valid");
    let Loaded::Configured(cfg) = outcome.loaded else {
        panic!("configured");
    };
    assert_eq!(cfg.local.trunk, ["ens19", "ens20"]);
    assert_eq!(cfg.trunk(), "ens19");
    assert_eq!(cfg.trunk_ports_for("heim".into()), ["ens21", "ens22"]);
    assert_eq!(
        cfg.trunk_ports_for("haus3".into()),
        ["ens19", "ens20"],
        "the default ports"
    );
    let text = render_local(&cfg.local, Some(&cfg.network));
    assert!(text.contains("trunk = [\"ens19\", \"ens20\"]\n"), "{text}");
    assert!(
        text.contains("trunk = \"ens23\"\n"),
        "one port stays a string: {text}"
    );
    assert_eq!(
        crate::config::parse_local(&text, Some(&cfg.network))
            .unwrap()
            .local,
        cfg.local,
        "round trip"
    );
}

#[test]
fn a_trunk_port_belongs_to_one_segment() {
    for (local, contains) in [
        (
            "self = \"haus2\"\ntrunk = [\"ens19\", \"ens20\"]\n\n[tunnels.heim]\ntrunk = [\"ens20\", \"ens21\"]\n",
            "\"ens20\" is a trunk port of two segments",
        ),
        (
            "self = \"haus2\"\ntrunk = [\"ens19\", \"ens19\"]\n",
            "\"ens19\" is listed twice",
        ),
        (
            "self = \"haus2\"\ntrunk = [\"ens19\", \"br0\"]\n",
            "managed by wanderlan",
        ),
        // One segment, one list: the same ports in another order would
        // name another bridge.
        (
            "self = \"haus2\"\ntrunk = [\"ens19\", \"ens20\"]\n\n[tunnels.heim]\ntrunk = [\"ens20\", \"ens19\"]\n",
            "the same ports as trunk in another order",
        ),
        (
            "self = \"haus2\"\ntrunk = \"ens19\"\n\n[tunnels.heim]\ntrunk = []\n",
            "is empty; remove the entry",
        ),
    ] {
        let errs = errors(EXAMPLE_NETWORK, local);
        assert!(
            errs.iter().any(|e| e.message.contains(contains)),
            "{contains}: {errs:#?}"
        );
    }
    // The same list for a tunnel is the default trunk anyway: a warning.
    let local = "self = \"haus2\"\ntrunk = [\"ens19\", \"ens20\"]\n\n[tunnels.heim]\ntrunk = [\"ens19\", \"ens20\"]\n";
    let outcome = load(EXAMPLE_NETWORK, local).expect("valid");
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.message.contains("the default trunk anyway")),
        "{:?}",
        outcome.warnings
    );
}

// Format 2: `[site.<name>]` with `map` and `tunnel_vlans` inside, as
// people write it (and older versions did).

/// The placeholder of a site without a box, as written.
fn pid(name: &str) -> String {
    crate::identity::SiteId::provisional(name).to_string()
}

const EXAMPLE_NETWORK_2: &str = include_str!("../../../../../packaging/examples/network.toml");

/// `text` with `from` replaced; fails if `from` is not there (a silent
/// no-op would test nothing).
fn edit(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "{from:?} not in the text");
    text.replacen(from, to, 1)
}

#[test]
fn both_formats_describe_the_same_network() {
    let old = parse_network(EXAMPLE_NETWORK).unwrap();
    let new = parse_network(EXAMPLE_NETWORK_2).unwrap();
    assert!(old.old_format());
    assert!(!new.old_format());
    assert_eq!(old.network, new.network);
    let outcome = load(EXAMPLE_NETWORK_2, EXAMPLE_LOCAL).expect("example loads");
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
}

#[test]
fn names_by_hand_are_written_back_with_identities() {
    let network = parse_network(EXAMPLE_NETWORK_2).unwrap().network.unwrap();
    let text = render_network(&network);
    assert!(
        text.contains(&format!(
            "\n[site.\"{}\"]  # no box has joined as this site yet\nname = \"haus3\"\naddress = \"10.8.0.12\"\nmap = {{ 20 = \"native\", 30 = 130 }}\n",
            pid("haus3")
        )),
        "{text}"
    );
    assert!(
        !text.contains("[[site]]") && !text.contains("[site.haus3]"),
        "{text}"
    );
    let again = parse_network(&text).unwrap();
    assert_eq!(again.format, 3);
    assert_eq!(again.network.as_ref(), Some(&network));
    assert_eq!(render_network(&network), text, "canonical");
}

/// The sites exchange the form of the file, and its fingerprint is the
/// same on every site whatever the revision.
#[test]
fn the_sites_exchange_the_form_of_the_file() {
    let network = parse_network(EXAMPLE_NETWORK_2).unwrap().network.unwrap();
    let wire = crate::config::render_network_wire(&network);
    assert_eq!(wire, render_network(&network));
    assert_eq!(parse_network(&wire).unwrap().format, 3);
    let newer = Network {
        revision: network.revision + 1,
        ..network.clone()
    };
    assert_eq!(fingerprint(&network), fingerprint(&newer));
}

#[test]
fn a_site_without_a_box_renamed_by_hand_keeps_its_references() {
    let text = edit(
        EXAMPLE_NETWORK_2,
        "[site.haus2]\naddress = \"10.8.0.11\"\n",
        "[site.haus2]\naddress = \"10.8.0.11\"\nserver = \"heim\"\n",
    );
    let network = parse_network(&text).unwrap().network.unwrap();
    let written = render_network(&network);
    // Only the name changes, the placeholder of heim stays in the file.
    let renamed = edit(&written, "name = \"heim\"", "name = \"home\"");
    let again = parse_network(&renamed).unwrap().network.unwrap();
    let home = again.by_name("home").expect("renamed");
    assert_eq!(home.id, crate::identity::SiteId::provisional("home"));
    assert_eq!(again.by_name("haus2").unwrap().uplink, Some(home.id));
    // Two sites without a box under one name are refused.
    let twice = edit(&written, "name = \"heim\"", "name = \"buero\"");
    assert!(
        parse_network(&twice)
            .unwrap_err()
            .iter()
            .any(|d| d.message.contains("defined twice")),
        "{twice}"
    );
    // Two names swapped: each site keeps what refers to it.
    let swapped = edit(
        &edit(&written, "name = \"heim\"", "name = \"tmp\""),
        "name = \"buero\"",
        "name = \"heim\"",
    );
    let swapped = edit(&swapped, "name = \"tmp\"", "name = \"buero\"");
    let again = parse_network(&swapped).unwrap().network.unwrap();
    let buero = again.by_name("buero").unwrap();
    assert_eq!(buero.address.to_string(), "10.8.0.1", "the former heim");
    assert_eq!(again.by_name("haus2").unwrap().uplink, Some(buero.id));
    // local.toml follows: haus2 keeps its own trunk for the tunnel to it.
    let local = format!(
        "self = \"{}\"\ntrunk = \"ens19\"\n\n[tunnels.\"{}\"]\ntrunk = \"ens20\"\n",
        pid("haus2"),
        pid("heim")
    );
    let outcome = load(&swapped, &local).expect("valid");
    let Loaded::Configured(cfg) = outcome.loaded else {
        panic!("configured")
    };
    assert_eq!(cfg.trunk_for(buero.id), "ens20");
}

#[test]
fn sites_keep_the_order_of_the_file() {
    let text = "[network]\nvlans = \"10\"\n\n[site.zeta]\naddress = \"10.0.0.1\"\nmap = { 30 = 130, 10 = \"native\" }\n\n[site.alpha]\naddress = \"10.0.0.2\"\n";
    let network = parse_network(text).unwrap().network.unwrap();
    let names: Vec<&str> = network.sites.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["zeta", "alpha"]);
    let shared: Vec<u16> = network.maps.iter().map(|m| m.shared).collect();
    assert_eq!(shared, [30, 10]);
}

#[test]
fn server_and_tunnel_limits_in_format_2() {
    let text = edit(
        EXAMPLE_NETWORK_2,
        "[site.haus2]\naddress = \"10.8.0.11\"\n",
        "[site.haus2]\naddress = \"10.8.0.11\"\nserver = \"heim\"\ntunnel_vlans = { heim = \"20\" }\n",
    );
    let text = edit(
        &text,
        "[site.heim]\naddress = \"10.8.0.1\"\n",
        "[site.heim]\naddress = \"10.8.0.1\"\ntunnel_vlans = { buero = \"none\" }\n",
    );
    let network = parse_network(&text).unwrap().network.unwrap();
    assert_eq!(
        network.by_name("haus2").unwrap().uplink,
        Some("heim".into())
    );
    assert_eq!(network.tunnels.len(), 2);
    assert_eq!(
        network
            .tunnel_limit("heim".into(), "haus2".into())
            .map(ToString::to_string),
        Some("20".to_string())
    );
    assert!(
        network
            .tunnel_limit("buero".into(), "heim".into())
            .unwrap()
            .is_empty()
    );
    // Written back at the client, between two servers at the first one.
    let rendered = render_network(&network);
    let (heim, buero) = (pid("heim"), pid("buero"));
    assert!(
        rendered.contains(&format!("name = \"haus2\"\naddress = \"10.8.0.11\"\nserver = \"{heim}\"  # heim\nchanges = \"both\"\nvlans = \"20,30\"\ntunnel_vlans = {{ \"{heim}\" = \"20\" }}  # heim\n")),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!(
            "name = \"heim\"\naddress = \"10.8.0.1\"\ntunnel_vlans = {{ \"{buero}\" = \"none\" }}  # buero\n"
        )),
        "{rendered}"
    );
    assert_eq!(parse_network(&rendered).unwrap().network.unwrap(), network);
    // A manager alone does not make a site a client (only in format 1).
    let managed = edit(
        EXAMPLE_NETWORK_2,
        "[site.buero]\naddress = \"10.8.0.2\"\n",
        "[site.buero]\naddress = \"10.8.0.2\"\nmanaged_by = \"heim\"\n",
    );
    let network = parse_network(&managed).unwrap().network.unwrap();
    assert_eq!(network.by_name("buero").unwrap().uplink, None);
}

#[test]
fn format_1_files_with_a_manager_become_explicit_clients() {
    let old = edit(
        EXAMPLE_NETWORK,
        "name = \"haus2\"\naddress = \"10.8.0.11\"\n",
        "name = \"haus2\"\naddress = \"10.8.0.11\"\nmanaged_by = \"heim\"\n",
    );
    let network = parse_network(&old).unwrap().network.unwrap();
    assert_eq!(
        network.by_name("haus2").unwrap().uplink,
        Some("heim".into())
    );
    let new = render_network(&network);
    let heim = pid("heim");
    assert!(
        new.contains(&format!(
            "name = \"haus2\"\naddress = \"10.8.0.11\"\nserver = \"{heim}\"  # heim\nchanges = \"server\"\n"
        )),
        "{new}"
    );
    assert_eq!(parse_network(&new).unwrap().network.unwrap(), network);
}

#[test]
fn errors_in_format_2_name_the_line() {
    let haus3 = "[site.haus3]\naddress = \"10.8.0.12\"\n";
    for (from, to, needle, message) in [
        (
            haus3,
            "[site.haus3]\naddress = \"10.8.0.12\"\ncolour = \"red\"\n",
            "colour",
            "unknown field `colour`",
        ),
        (
            haus3,
            "[site.haus3]\naddress = \"10.8.0.12\"\nserver = \"\"\n",
            "server = \"\"",
            "a site without `server` is a server",
        ),
        (
            haus3,
            "[site.haus3]\naddress = \"10.8.0.12\"\nserver = \"nowhere\"\n",
            "server = \"nowhere\"",
            "server \"nowhere\" is not a site of the network",
        ),
        (
            haus3,
            "[site.haus3]\naddress = \"10.8.0.12\"\nserver = \"haus3\"\n",
            "server = \"haus3\"",
            "cannot be its own server",
        ),
        (
            "map = { 20 = \"native\", 30 = 130 }",
            "map = { 20 = \"native\", 30x = 130 }",
            "30x",
            "\"30x\" is not a VLAN ID",
        ),
        (
            "map = { 20 = \"native\", 30 = 130 }",
            "map = { 20 = \"native\", 5000 = 130 }",
            "5000",
            "\"5000\" is not a VLAN ID",
        ),
        (
            "map = { 20 = \"native\", 30 = 130 }",
            "map = { 20 = \"untagged\", 30 = 130 }",
            "\"untagged\"",
            "must be a VLAN ID or \"native\"",
        ),
        (
            "map = { 20 = \"native\", 30 = 130 }",
            "map = { 20 = \"native\", 30 = 9999 }",
            "9999",
            "must be between 1 and 4094",
        ),
        (
            "map = { 20 = \"native\", 30 = 130 }",
            "map = { 20 = \"native\", 30 = 130, 60 = 160 }",
            "60 = 160",
            "source VLAN 60 is not one of the VLANs",
        ),
        (
            haus3,
            "[site.haus3]\naddress = \"10.8.0.12\"\ntunnel_vlans = { nowhere = \"20\" }\n",
            "nowhere",
            "site \"nowhere\" does not exist",
        ),
        (
            haus3,
            "[site.haus3]\naddress = \"10.8.0.12\"\ntunnel_vlans = { haus3 = \"20\" }\n",
            "haus3 = ",
            "not \"haus3\" with itself",
        ),
        (
            haus3,
            "[site.haus3]\naddress = \"10.8.0.12\"\ntunnel_vlans = { heim = \"5000\" }\n",
            "\"5000\"",
            "tunnel_vlans: heim:",
        ),
        (
            "[site.buero]\naddress = \"10.8.0.2\"\n",
            "[site.buero]\n",
            "[site.buero]",
            "site \"buero\": address is missing",
        ),
        (
            "\n[site.heim]",
            "\n[[map]]\nsite = \"heim\"\nlocal = 5\nshared = 20\n\n[site.heim]",
            "[[map]]",
            "belongs to an older format",
        ),
    ] {
        let text = edit(EXAMPLE_NETWORK_2, from, to);
        let line = line_of(&text, "", needle);
        let err = single_error(&text, LOCAL);
        assert!(err.message.contains(message), "{message:?} not in {err}");
        assert_eq!(err.line, Some(line), "wrong line for {err}\n{text}");
    }
    // Limited at both sites.
    let both = edit(
        EXAMPLE_NETWORK_2,
        "[site.heim]\naddress = \"10.8.0.1\"\n",
        "[site.heim]\naddress = \"10.8.0.1\"\ntunnel_vlans = { buero = \"20\" }\n",
    );
    let both = edit(
        &both,
        "[site.buero]\naddress = \"10.8.0.2\"\n",
        "[site.buero]\naddress = \"10.8.0.2\"\ntunnel_vlans = { heim = \"30\" }\n",
    );
    let err = single_error(&both, LOCAL);
    assert!(err.message.contains("is limited at both sites"), "{err}");
    assert_eq!(err.line, Some(line_of(&both, "[site.buero]", "heim = ")));
}

#[test]
fn settings_of_older_versions_are_read_and_ignored() {
    let text = edit(
        EXAMPLE_NETWORK_2,
        "[site.haus2]\naddress = \"10.8.0.11\"\n",
        "[site.haus2]\naddress = \"10.8.0.11\"\nrename_by_manager = true\ntunnel_mode = \"direct\"\n",
    );
    let outcome = load(&text, EXAMPLE_LOCAL).expect("still loads");
    for field in ["rename_by_manager", "tunnel_mode"] {
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.message.contains(&format!("`{field}` is no longer used"))),
            "{field}: {:?}",
            outcome.warnings
        );
    }
    let network = parse_network(&text).unwrap().network.unwrap();
    let rendered = render_network(&network);
    assert!(!rendered.contains("rename_by_manager") && !rendered.contains("tunnel_mode"));
}

const ID_A: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=";
const ID_B: &str = "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8=";

#[test]
fn site_ids_are_read_and_written_in_both_formats() {
    let network = EXAMPLE_NETWORK.replacen(
        "address = \"10.8.0.11\"",
        &format!("address = \"10.8.0.11\"\nid = \"{ID_A}\""),
        1,
    );
    let parsed = parse_network(&network).unwrap().network.unwrap();
    let site = parsed.by_name("haus2").unwrap();
    assert_eq!(site.id.to_string(), ID_A);
    assert!(parsed.sites.iter().filter(|s| s.has_identity()).count() == 1);
    let text = render_network(&parsed);
    // Its table is named by it (no placeholder comment), no `id` line.
    assert!(
        text.contains(&format!("[site.\"{ID_A}\"]\nname = \"haus2\"\n")),
        "{text}"
    );
    assert!(!text.contains("id ="), "{text}");
    assert_eq!(parse_network(&text).unwrap().network.unwrap(), parsed);
}

#[test]
fn placeholders_are_marked_and_identities_change_the_fingerprint() {
    let network = parse_network(EXAMPLE_NETWORK).unwrap().network.unwrap();
    let text = render_network(&network);
    assert_eq!(
        text.matches("# no box has joined as this site yet").count(),
        4,
        "{text}"
    );
    let mut with_id = network.clone();
    let first = with_id.sites[0].id;
    with_id.rekey(first, ID_A.parse().unwrap());
    assert_eq!(
        render_network(&with_id)
            .matches("# no box has joined as this site yet")
            .count(),
        3
    );
    assert_ne!(fingerprint(&network), fingerprint(&with_id));
}

#[test]
fn an_invalid_id_has_a_line() {
    let network = EXAMPLE_NETWORK.replacen(
        "address = \"10.8.0.11\"",
        "address = \"10.8.0.11\"\nid = \"not-a-key\"",
        1,
    );
    let line = line_of(&network, "", "not-a-key");
    assert_error(&network, line, "is not a wanderlan identity");
}

#[test]
fn two_sites_with_the_same_id() {
    let network = EXAMPLE_NETWORK
        .replacen(
            "address = \"10.8.0.11\"",
            &format!("address = \"10.8.0.11\"\nid = \"{ID_A}\""),
            1,
        )
        .replacen(
            "address = \"10.8.0.12\"",
            &format!("address = \"10.8.0.12\"\nid = \"{ID_A}\""),
            1,
        );
    let line = line_of(&network, "10.8.0.12", "id = ");
    assert_error(&network, line, "have the same identity");
    let fine = network.replacen(ID_A, ID_B, 2).replacen(ID_B, ID_A, 1);
    assert!(load(&fine, LOCAL).is_ok());
}

#[test]
fn clients_with_identities_may_share_an_address() {
    // Two clients of haus2 behind the same NAT.
    let mut network = parse_network(EXAMPLE_NETWORK).unwrap().network.unwrap();
    let id = |n: u8| crate::identity::SiteId::from_bytes([n; 32]);
    let address = network.sites[1].address;
    let server = network.sites[1].id;
    for (i, n) in [(2usize, 3u8), (3, 4)] {
        let old = network.sites[i].id;
        network.rekey(old, id(n));
        let site = &mut network.sites[i];
        site.address = address;
        site.uplink = Some(server);
    }
    let text = render_network(&network);
    let errs = errors(&text, LOCAL);
    assert!(
        errs.iter().any(|e| e.message.contains("already used")),
        "the server has no identity: {errs:?}"
    );
    network.rekey(server, id(2));
    assert!(load(&render_network(&network), LOCAL).is_ok());
    // Without identities they must not.
    let name = network.sites[2].name.clone();
    network.rekey(id(3), crate::identity::SiteId::provisional(&name));
    let errs = errors(&render_network(&network), LOCAL);
    assert!(
        errs.iter().any(|e| e.message.contains("already used")),
        "{errs:?}"
    );
}

#[test]
fn the_tunnel_port_follows_the_control_port_unless_set() {
    let mut network = parse_network(EXAMPLE_NETWORK).unwrap().network.unwrap();
    assert_eq!(network.tunnel_port(), 51822);
    assert!(!render_network(&network).contains("tunnel_port"));
    network.tunnel_port = Some(4000);
    let text = render_network(&network);
    assert!(text.contains("tunnel_port = 4000"), "{text}");
    assert_eq!(
        parse_network(&text).unwrap().network.unwrap().tunnel_port(),
        4000
    );
    // Neither the tunnel port nor the one above it (NAT) may be the
    // control port.
    for port in [network.control_port, network.control_port - 1] {
        network.tunnel_port = Some(port);
        let errs = errors(&render_network(&network), LOCAL);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("must differ from the control port")),
            "{port}: {errs:?}"
        );
    }
    // At the end of the port range the tunnel port goes below.
    network.tunnel_port = None;
    network.control_port = 65534;
    assert_eq!(network.tunnel_port(), 65532);
    network.tunnel_port = Some(65535);
    assert!(!errors(&render_network(&network), LOCAL).is_empty());
}

#[test]
fn breakouts_are_read_written_and_checked() {
    let text = r#"
[network]
vlans = "native,10-20"

[site.alex]
address = "10.13.0.103"

[site.site2]
address = "10.13.0.104"
server = "alex"
breakout = { 10 = {}, 20 = { uplink = "ens20", except = ["203.0.113.9/24", "2001:db8::1"] } }
"#;
    let network = parse_network(text).unwrap().network.unwrap();
    let site2 = network.by_name("site2").unwrap();
    assert_eq!(site2.breakout.len(), 2);
    let twenty = &site2.breakout[&20];
    assert_eq!(twenty.uplink, Some("ens20".into()));
    let except: Vec<String> = twenty.except.iter().map(ToString::to_string).collect();
    assert_eq!(except, ["203.0.113.0/24", "2001:db8::1/128"]);
    // Both forms carry it, and read back the same.
    let line = "breakout = { 10 = {}, 20 = { uplink = \"ens20\", except = [\"203.0.113.0/24\", \"2001:db8::1/128\"] } }";
    for form in [
        render_network(&network),
        crate::config::render_network_wire(&network),
    ] {
        assert!(form.contains(line), "{form}");
        assert_eq!(
            parse_network(&form).unwrap().network.unwrap().sites,
            network.sites
        );
    }
    // Only the VLANs of the site.
    let local = "self = \"site2\"\ntrunk = \"ens19\"\n";
    let wrong = text.replace("20 = { uplink", "30 = { uplink");
    let err = single_error(&wrong, local);
    assert!(
        err.message
            .contains("breakout: VLAN 30 is not one of the VLANs of the site"),
        "{err}"
    );
    assert_eq!(err.line, Some(line_of(&wrong, "[site.site2]", "breakout")));
    // Source VLAN 10 carried as 20 at the trunk: VLAN 20 is not there.
    let mapped = text.replace("breakout =", "map = { 10 = 20 }\nbreakout =");
    let outcome = load(&mapped, local).unwrap();
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.message.contains("breakout 20 does nothing")),
        "{:#?}",
        outcome.warnings
    );
    // A trunk port is no uplink.
    let err = single_error(text, "self = \"site2\"\ntrunk = \"ens20\"\n");
    assert!(
        err.message.contains("is a trunk port of this site"),
        "{err}"
    );
    // Not an address.
    let bad = text.replace("2001:db8::1", "example.com");
    let err = single_error(&bad, local);
    assert!(
        err.message.contains("\"example.com\" is not an address"),
        "{err}"
    );
    // A server breaks out nothing: an older network with one still loads,
    // it is ignored with a warning.
    let server = text.replace(
        "address = \"10.13.0.103\"\n",
        "address = \"10.13.0.103\"\nbreakout = { 10 = {} }\n",
    );
    let outcome = load(&server, "self = \"alex\"\ntrunk = \"ens19\"\n").unwrap();
    let ignored: Vec<_> = outcome
        .warnings
        .iter()
        .filter(|w| {
            w.message
                .contains("breakout 10 is ignored: \"alex\" is a server")
        })
        .collect();
    assert_eq!(ignored.len(), 1, "{:#?}", outcome.warnings);
    // A client of site2 sees site2 as a server, without its breakout.
    let mut network = parse_network(text).unwrap().network.unwrap();
    let mut below = network.sites[1].clone();
    below.name = "site3".into();
    below.address = "10.13.0.105".parse().unwrap();
    below.uplink = Some("site2".into());
    below.id = "site3".into();
    below.breakout.clear();
    network.sites.push(below);
    let view = crate::view::view_for(&network, "site3".into());
    assert!(view.by_name("site2").unwrap().breakout.is_empty());
}

#[test]
fn a_site_has_its_own_ports_and_settings() {
    let base = parse_network(EXAMPLE_NETWORK).unwrap().network.unwrap();
    let text = edit(
        EXAMPLE_NETWORK_2,
        "[site.haus2]\naddress = \"10.8.0.11\"\n",
        "[site.haus2]\naddress = \"10.8.0.11\"\ncontrol_port = 52821\n",
    );
    let local =
        format!("{LOCAL}commit_timeout = \"2m\"\nmtu_probe_interval = \"1m\"\nmss = 1300\n");
    let Loaded::Configured(cfg) = load(&text, &local).unwrap().loaded else {
        panic!("configured");
    };
    assert_eq!(cfg.control_port(), 52821);
    assert_eq!(cfg.tunnel_port(), 52822, "the one after its control port");
    assert_eq!(
        cfg.network.control_port_of("heim".into()),
        51821,
        "the others keep theirs"
    );
    assert_eq!(cfg.commit_timeout(), std::time::Duration::from_secs(120));
    assert_eq!(cfg.mtu_probe_interval(), std::time::Duration::from_secs(60));
    assert_eq!(cfg.mss(), Some(1300));
    assert!(render_network(&cfg.network).contains("control_port = 52821"));
    let rendered = render_local(&cfg.local, None);
    assert!(rendered.contains("commit_timeout = \"2m\"") && rendered.contains("mss = 1300"));
    // Without own ports the network keeps its form and fingerprint.
    assert!(!render_network(&base).contains("\ntunnel_port ="));
    // Its tunnel port must stay off its control port.
    let clash = edit(
        &text,
        "control_port = 52821\n",
        "control_port = 52821\ntunnel_port = 52820\n",
    );
    assert!(
        errors(&clash, LOCAL)
            .iter()
            .any(|e| e.message.contains("must differ from its control port"))
    );
    assert!(!errors(EXAMPLE_NETWORK, &format!("{LOCAL}mss = 100\n")).is_empty());
}

#[test]
fn two_servers_at_one_address_also_with_a_client_between() {
    let mut network = parse_network(EXAMPLE_NETWORK).unwrap().network.unwrap();
    let id = |b: u8| crate::identity::SiteId::from_bytes([b; 32]);
    let addr = network.sites[0].address;
    // s0 (server), s1 (client of s0), s2 (server): all at one address.
    for i in 0..3 {
        let old = network.sites[i].id;
        network.rekey(old, id(u8::try_from(i).unwrap() + 1));
        let site = &mut network.sites[i];
        site.address = addr;
        site.uplink = None;
    }
    let first = network.sites[0].name.clone();
    network.sites[1].uplink = Some(network.sites[0].id);
    let errs: Vec<String> = crate::config::validate(&network, None)
        .into_iter()
        .filter(Diagnostic::is_error)
        .map(|d| d.message)
        .collect();
    assert!(
        errs.iter()
            .any(|e| e.contains(&format!("already used by site \"{first}\""))),
        "{errs:?}"
    );
}

#[test]
fn an_own_vlan_may_be_mapped() {
    let mut network = parse_network(EXAMPLE_NETWORK).unwrap().network.unwrap();
    let site = network.maps[0].site;
    let entry = network.site_mut(site).unwrap();
    entry.own_vlans = Some("60".parse().unwrap());
    network.maps[0].shared = 60;
    let errs: Vec<Diagnostic> = crate::config::validate(&network, None)
        .into_iter()
        .filter(Diagnostic::is_error)
        .collect();
    assert!(errs.is_empty(), "{errs:?}");
}

#[test]
fn a_mapped_vlan_is_a_vlan_id() {
    let mut network = parse_network(EXAMPLE_NETWORK).unwrap().network.unwrap();
    let i = network
        .maps
        .iter()
        .position(|m| matches!(m.local, crate::config::LocalVid::Vid(_)))
        .unwrap();
    network.maps[i].local = crate::config::LocalVid::Vid(4095);
    let errs: Vec<String> = crate::config::validate(&network, None)
        .into_iter()
        .filter(Diagnostic::is_error)
        .map(|d| d.message)
        .collect();
    assert!(errs.iter().any(|e| e.contains("not a VLAN ID")), "{errs:?}");
}
