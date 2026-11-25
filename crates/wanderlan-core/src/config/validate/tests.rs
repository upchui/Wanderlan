use crate::config::{
    ConfigFile, Diagnostic, Loaded, Network, Severity, check, fingerprint, has_errors, load,
    parse_network, render_local, render_network,
};

const EXAMPLE_NETWORK: &str = include_str!("../../../../../packaging/examples/network.toml");
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

    let local = crate::config::parse_local(EXAMPLE_LOCAL).unwrap().local;
    let text = render_local(&local);
    assert_eq!(crate::config::parse_local(&text).unwrap().local, local);
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
    let haus2 = cfg.network.site("haus2").expect("haus2");
    assert_eq!(cfg.network.vlans_of(haus2).to_string(), "20,30");
    let heim = cfg.network.site("heim").expect("heim");
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
    assert_eq!(cfg.network.site("haus2").unwrap().vlans, None);
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
    let err = single_error(EXAMPLE_NETWORK, "self = \"haus2\"\ntrunk = \"gt-heim\"\n");
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
        network.site("marcel").unwrap().uplink.as_deref(),
        Some("alex")
    );
    assert_eq!(
        network.site("site2").unwrap().uplink.as_deref(),
        Some("alex")
    );
    assert_eq!(network.site("alex").unwrap().uplink, None);
    // Written explicitly from now on; the canonical form reads back the same.
    let text = render_network(&network);
    assert_eq!(text.matches("uplink = \"alex\"").count(), 2, "{text}");
    assert_eq!(parse_network(&text).unwrap().network.unwrap(), network);
    // A server with a manager stays a server: `uplink = ""`.
    let mut server = network.clone();
    server.sites[1].uplink = None;
    let text = render_network(&server);
    assert!(text.contains("uplink = \"\""), "{text}");
    assert_eq!(parse_network(&text).unwrap().network.unwrap(), server);
}

#[test]
fn uplinks_must_lead_to_a_server() {
    let local = "self = \"alex\"\ntrunk = \"ens19\"\n";
    let unknown = CLIENTS.replace("uplink = \"alex\"", "uplink = \"nowhere\"");
    let err = single_error(&unknown, local);
    assert!(
        err.message.contains("uplink \"nowhere\" is not a site"),
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
            .contains("uplink is the site itself")
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
            .contains("uplinks form a loop: marcel -> site2 -> marcel")),
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
    assert_eq!(cfg.trunk_for("heim"), "ens20");
    assert_eq!(cfg.trunk_for("buero"), "ens19");
    // The canonical form keeps it and reads back the same.
    let rendered = render_local(&cfg.local);
    assert!(
        rendered.contains("[tunnels.heim]\ntrunk = \"ens20\""),
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
    let local = "self = \"haus2\"\ntrunk = \"ens19\"\n\n[tunnels.heim]\ntrunk = \"gt-heim\"\n";
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
    let limit = cfg.network.tunnel_limit("buero", "heim").expect("limit");
    assert_eq!(limit.to_string(), "20-30");
    assert!(
        cfg.network
            .tunnel_limit("heim", "haus2")
            .expect("none")
            .is_empty()
    );
    let text = render_network(&cfg.network);
    assert!(
        text.contains("[[tunnel]]\nbetween = [\"buero\", \"heim\"]\nvlans = \"20-30\"\n"),
        "{text}"
    );
    assert!(text.contains("vlans = \"none\""), "{text}");
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
        err.message.contains("already has its VLANs in tunnel #1"),
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
    assert_eq!(cfg.trunk_ports_for("heim"), ["ens21", "ens22"]);
    assert_eq!(
        cfg.trunk_ports_for("haus3"),
        ["ens19", "ens20"],
        "the default ports"
    );
    let text = render_local(&cfg.local);
    assert!(text.contains("trunk = [\"ens19\", \"ens20\"]\n"), "{text}");
    assert!(
        text.contains("trunk = \"ens23\"\n"),
        "one port stays a string: {text}"
    );
    assert_eq!(
        crate::config::parse_local(&text).unwrap().local,
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
