use crate::config::{
    ConfigFile, Diagnostic, Loaded, Network, Severity, check, fingerprint, has_errors, load,
    parse_network, render_local, render_network,
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
    assert_eq!(text.matches("server = \"alex\"").count(), 2, "{text}");
    assert_eq!(parse_network(&text).unwrap().network.unwrap(), network);
    // A server with a manager stays a server: no `server` in format 2,
    // `uplink = ""` in the form the sites exchange.
    let mut server = network.clone();
    server.sites[1].uplink = None;
    let text = render_network(&server);
    assert!(
        text.contains("[site.marcel]\naddress = \"10.0.10.5\"\nmanaged_by = \"alex\"\n"),
        "{text}"
    );
    assert_eq!(parse_network(&text).unwrap().network.unwrap(), server);
    let wire = crate::config::render_network_wire(&server);
    assert!(wire.contains("uplink = \"\""), "{wire}");
    assert_eq!(parse_network(&wire).unwrap().network.unwrap(), server);
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
    let limit = cfg.network.tunnel_limit("buero", "heim").expect("limit");
    assert_eq!(limit.to_string(), "20-30");
    assert!(
        cfg.network
            .tunnel_limit("heim", "haus2")
            .expect("none")
            .is_empty()
    );
    // Three servers: both limits at heim, the first of them.
    let text = render_network(&cfg.network);
    assert!(
        text.contains("tunnel_vlans = { buero = \"20-30\", haus2 = \"none\" }\n"),
        "{text}"
    );
    let again = parse_network(&text).unwrap().network.unwrap();
    assert_eq!(again.tunnels, cfg.network.tunnels);
    let wire = crate::config::render_network_wire(&cfg.network);
    assert!(
        wire.contains("[[tunnel]]\nbetween = [\"buero\", \"heim\"]\nvlans = \"20-30\"\n"),
        "{wire}"
    );
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

// Format 2: `[site.<name>]` with `map` and `tunnel_vlans` inside.

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
    assert!(old.old_format);
    assert!(!new.old_format);
    assert_eq!(old.network, new.network);
    let outcome = load(EXAMPLE_NETWORK_2, EXAMPLE_LOCAL).expect("example loads");
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
}

#[test]
fn format_2_is_written_and_read_back() {
    let network = parse_network(EXAMPLE_NETWORK_2).unwrap().network.unwrap();
    let text = render_network(&network);
    assert!(
        text.contains(
            "\n[site.haus3]\naddress = \"10.8.0.12\"\nmap = { 20 = \"native\", 30 = 130 }\n"
        ),
        "{text}"
    );
    assert!(
        !text.contains("[[site]]") && !text.contains("[[map]]"),
        "{text}"
    );
    let again = parse_network(&text).unwrap();
    assert!(!again.old_format);
    assert_eq!(again.network.as_ref(), Some(&network));
    assert_eq!(render_network(&network), text, "canonical");
}

/// The form the sites send each other, and its fingerprint: exactly what
/// wanderlan 1.0 sent. If this changes, sites of different versions no
/// longer see the same network as the same.
#[test]
fn the_wire_form_never_changes() {
    let network = parse_network(EXAMPLE_NETWORK_2).unwrap().network.unwrap();
    let wire = crate::config::render_network_wire(&network);
    assert_eq!(
        wire,
        "# wanderlan network configuration, shared by all sites.\n\
         # This file is kept identical on all sites by the wanderlan daemon.\n\
         # Change it with the `wanderlan` TUI or edit it and run `wanderlan apply`.\n\
         \n[network]\nrevision = 0\nvlans = \"10-50\"\ncontrol_port = 51821\n\
         mtu_probe_interval = \"5m\"\ncommit_timeout = \"1m\"\n\
         \n[[site]]\nname = \"heim\"\naddress = \"10.8.0.1\"\n\
         \n[[site]]\nname = \"buero\"\naddress = \"10.8.0.2\"\n\
         \n[[site]]\nname = \"haus2\"\naddress = \"10.8.0.11\"\nvlans = \"20,30\"\n\
         \n[[site]]\nname = \"haus3\"\naddress = \"10.8.0.12\"\n\
         \n[[map]]\nsite = \"haus3\"\nlocal = \"native\"\nshared = 20\n\
         \n[[map]]\nsite = \"haus3\"\nlocal = 130\nshared = 30\n"
    );
    assert_eq!(
        fingerprint(&network),
        "8bafc5da21a7724f3687059252586b7ce8751c9a463d6831eefdeaaa260b76cf"
    );
    // And older sites read it: format 1.
    assert!(parse_network(&wire).unwrap().old_format);
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
        network.site("haus2").unwrap().uplink.as_deref(),
        Some("heim")
    );
    assert_eq!(network.tunnels.len(), 2);
    assert_eq!(
        network
            .tunnel_limit("heim", "haus2")
            .map(ToString::to_string),
        Some("20".to_string())
    );
    assert!(network.tunnel_limit("buero", "heim").unwrap().is_empty());
    // Written back at the client, between two servers at the first one.
    let rendered = render_network(&network);
    assert!(
        rendered.contains("[site.haus2]\naddress = \"10.8.0.11\"\nserver = \"heim\"\nvlans = \"20,30\"\ntunnel_vlans = { heim = \"20\" }\n"),
        "{rendered}"
    );
    assert!(
        rendered
            .contains("[site.heim]\naddress = \"10.8.0.1\"\ntunnel_vlans = { buero = \"none\" }\n"),
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
    assert_eq!(network.site("buero").unwrap().uplink, None);
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
        network.site("haus2").unwrap().uplink.as_deref(),
        Some("heim")
    );
    let new = render_network(&network);
    assert!(
        new.contains(
            "[site.haus2]\naddress = \"10.8.0.11\"\nserver = \"heim\"\nmanaged_by = \"heim\"\n"
        ),
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
            "belongs to the older format",
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
    let site = parsed.site("haus2").unwrap();
    assert_eq!(site.id.unwrap().to_string(), ID_A);
    assert!(parsed.sites.iter().filter(|s| s.id.is_some()).count() == 1);
    for text in [
        render_network(&parsed),
        crate::config::render_network_wire(&parsed),
    ] {
        assert!(text.contains(&format!("id = \"{ID_A}\"")), "{text}");
        assert_eq!(parse_network(&text).unwrap().network.unwrap(), parsed);
    }
}

#[test]
fn without_ids_the_wire_form_and_fingerprint_stay_as_before() {
    // The bytes older versions exchange and hash: no `id` line appears.
    let network = parse_network(EXAMPLE_NETWORK).unwrap().network.unwrap();
    let wire = crate::config::render_network_wire(&network);
    assert!(!wire.contains("id ="), "{wire}");
    assert!(!render_network(&network).contains("id ="));
    let mut with_id = network.clone();
    with_id.sites[0].id = Some(ID_A.parse().unwrap());
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
    let id = |n: u8| Some(crate::identity::SiteId::from_bytes([n; 32]));
    let address = network.sites[1].address;
    let server = network.sites[1].name.clone();
    for (i, n) in [(2usize, 3u8), (3, 4)] {
        let site = &mut network.sites[i];
        site.address = address;
        site.uplink = Some(server.clone());
        site.id = id(n);
    }
    let text = render_network(&network);
    let errs = errors(&text, LOCAL);
    assert!(
        errs.iter().any(|e| e.message.contains("already used")),
        "the server has no identity: {errs:?}"
    );
    network.sites[1].id = id(2);
    assert!(load(&render_network(&network), LOCAL).is_ok());
    // Without identities they must not.
    network.sites[2].id = None;
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
