use crate::config::{
    ConfigFile, Diagnostic, Loaded, Mesh, Severity, check, fingerprint, has_errors, load,
    parse_mesh, render_local, render_mesh,
};

const EXAMPLE_MESH: &str = include_str!("../../../../../packaging/examples/mesh.toml");
const EXAMPLE_LOCAL: &str = include_str!("../../../../../packaging/examples/local.toml");
const LOCAL: &str = "self = \"haus2\"\ntrunk = \"ens19\"\n";

/// One-based line of the first occurrence of `needle` after `after`.
fn line_of(text: &str, after: &str, needle: &str) -> usize {
    let start = text.find(after).expect("anchor present");
    let pos = start + text[start..].find(needle).expect("needle present");
    text[..pos].matches('\n').count() + 1
}

fn errors(mesh: &str, local: &str) -> Vec<Diagnostic> {
    match load(mesh, local) {
        Ok(outcome) => panic!("expected errors, got {:?}", outcome.loaded),
        Err(diags) => diags.into_iter().filter(Diagnostic::is_error).collect(),
    }
}

fn single_error(mesh: &str, local: &str) -> Diagnostic {
    let errs = errors(mesh, local);
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:#?}");
    errs.into_iter().next().expect("one error")
}

fn assert_error(mesh: &str, line: usize, contains: &str) {
    let err = single_error(mesh, LOCAL);
    assert_eq!(err.file, ConfigFile::Mesh);
    assert!(
        err.message.contains(contains),
        "message {:?} does not contain {contains:?}",
        err.message
    );
    assert_eq!(err.line, Some(line), "wrong line for {err}");
    assert!(
        err.to_string()
            .starts_with(&format!("mesh.toml:{line}: error: "))
    );
}

#[test]
fn example_is_valid() {
    let outcome = load(EXAMPLE_MESH, EXAMPLE_LOCAL).expect("example loads");
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    let Loaded::Configured(cfg) = outcome.loaded else {
        panic!("example must be configured");
    };
    assert_eq!(cfg.self_name(), "haus2");
    assert_eq!(cfg.trunk(), "ens19");
    assert_eq!(cfg.mesh.sites.len(), 4);
    assert_eq!(cfg.mesh.maps.len(), 2);
    assert_eq!(cfg.mesh.vlans.to_string(), "10-50");
    assert_eq!(cfg.mesh.commit_timeout.as_secs(), 60);
    assert_eq!(cfg.mesh.mtu_probe_interval.as_secs(), 300);
    assert_eq!(cfg.mesh.mss, None);
    assert_eq!(cfg.mesh.control_port, 51821);
}

#[test]
fn render_roundtrip_is_identical() {
    let mesh = parse_mesh(EXAMPLE_MESH).unwrap().mesh.unwrap();
    let text = render_mesh(&mesh);
    let again = parse_mesh(&text).unwrap().mesh.unwrap();
    assert_eq!(mesh, again);
    assert_eq!(text, render_mesh(&again));

    let local = crate::config::parse_local(EXAMPLE_LOCAL).unwrap().local;
    let text = render_local(&local);
    assert_eq!(crate::config::parse_local(&text).unwrap().local, local);
}

#[test]
fn fingerprint_ignores_revision_and_formatting() {
    let mesh = parse_mesh(EXAMPLE_MESH).unwrap().mesh.unwrap();
    let mut bumped = mesh.clone();
    bumped.revision = 42;
    assert_eq!(fingerprint(&mesh), fingerprint(&bumped));
    let reformatted = parse_mesh(&render_mesh(&mesh)).unwrap().mesh.unwrap();
    assert_eq!(fingerprint(&mesh), fingerprint(&reformatted));
    let mut changed = mesh.clone();
    changed.sites[0].name = "heim2".into();
    assert_ne!(fingerprint(&mesh), fingerprint(&changed));
    assert_eq!(fingerprint(&mesh).len(), 64);
}

#[test]
fn template_without_settings_is_unconfigured() {
    let mesh = "# all commented out\n# [mesh]\n";
    let local = "# self = \"x\"\n";
    let outcome = load(mesh, local).unwrap();
    assert!(matches!(
        outcome.loaded,
        Loaded::Unconfigured { mesh: None, .. }
    ));
}

#[test]
fn self_without_mesh_is_an_error() {
    let err = single_error("", LOCAL);
    assert!(err.message.contains("contains no mesh configuration"));
}

#[test]
fn unconfigured_local_with_mesh_still_validates_mesh() {
    let outcome = load(EXAMPLE_MESH, "").unwrap();
    assert!(matches!(
        outcome.loaded,
        Loaded::Unconfigured { mesh: Some(_), .. }
    ));
    let bad = EXAMPLE_MESH.replace("address = \"10.8.0.2\"", "address = \"nope\"");
    assert!(load(&bad, "").is_err());
}

#[test]
fn syntax_error_has_line() {
    let mesh = EXAMPLE_MESH.replace("name = \"buero\"", "name = buero");
    let err = single_error(&mesh, LOCAL);
    assert_eq!(err.line, Some(line_of(&mesh, "", "name = buero")));
}

#[test]
fn unknown_key_is_rejected_with_line() {
    let mesh = EXAMPLE_MESH.replace("address = \"10.8.0.2\"", "adress = \"10.8.0.2\"");
    let err = single_error(&mesh, LOCAL);
    assert!(err.message.contains("adress"), "{err}");
    assert_eq!(err.line, Some(line_of(&mesh, "", "adress")));
}

#[test]
fn wrong_type_is_rejected_with_line() {
    let mesh = EXAMPLE_MESH.replace("shared = 30", "shared = \"30\"");
    let err = single_error(&mesh, LOCAL);
    assert_eq!(err.line, Some(line_of(&mesh, "local = 130", "shared")));
}

#[test]
fn duplicate_site_name() {
    let mesh = EXAMPLE_MESH.replace("name = \"haus3\"", "name = \"haus2\"");
    let errs = errors(&mesh, LOCAL);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("\"haus2\" is defined twice")
                && e.line
                    == Some(line_of(
                        &mesh,
                        "address = \"10.8.0.11\"",
                        "name = \"haus2\""
                    )))
    );
}

#[test]
fn duplicate_address() {
    let mesh = EXAMPLE_MESH.replace("10.8.0.12", "10.8.0.11");
    let line = line_of(&mesh, "name = \"haus3\"", "address");
    assert_error(&mesh, line, "already used by site \"haus2\"");
}

#[test]
fn invalid_address() {
    let mesh = EXAMPLE_MESH.replace("10.8.0.2\"", "10.8.0.300\"");
    let line = line_of(&mesh, "", "10.8.0.300");
    assert_error(&mesh, line, "is not an IPv4 address");
    let mesh = EXAMPLE_MESH.replace("10.8.0.2\"", "255.255.255.255\"");
    let line = line_of(&mesh, "", "255.255.255.255");
    assert_error(&mesh, line, "not a usable unicast address");
}

#[test]
fn old_role_and_hub_keys_are_ignored_with_a_warning() {
    let mesh = EXAMPLE_MESH.replace(
        "name = \"haus2\"\n",
        "name = \"haus2\"\nrole = \"spoke\"\nhub = \"heim\"\n",
    );
    let outcome = load(&mesh, LOCAL).expect("old keys do not make the file invalid");
    let warnings: Vec<_> = outcome
        .warnings
        .iter()
        .filter(|w| w.message.contains("no longer used"))
        .collect();
    assert_eq!(warnings.len(), 2, "{:?}", outcome.warnings);
    assert_eq!(
        warnings[0].line,
        Some(line_of(&mesh, "name = \"haus2\"", "role"))
    );
    let Loaded::Configured(cfg) = outcome.loaded else {
        panic!("configured");
    };
    assert_eq!(cfg.mesh.sites.len(), 4);
    assert!(!render_mesh(&cfg.mesh).contains("role"));
}

#[test]
fn a_single_site_is_a_valid_network() {
    let mesh = "[mesh]\nvlans = \"10\"\n\n[[site]]\nname = \"a\"\naddress = \"10.0.0.1\"\n";
    let outcome = load(mesh, "self = \"a\"\ntrunk = \"eth0\"\n").expect("valid");
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
}

#[test]
fn site_vlans_are_parsed_and_rendered() {
    let outcome = load(EXAMPLE_MESH, LOCAL).expect("example loads");
    let Loaded::Configured(cfg) = outcome.loaded else {
        panic!("configured");
    };
    let haus2 = cfg.mesh.site("haus2").expect("haus2");
    assert_eq!(cfg.mesh.vlans_of(haus2).to_string(), "20,30");
    let heim = cfg.mesh.site("heim").expect("heim");
    assert_eq!(cfg.mesh.vlans_of(heim).to_string(), "10-50");
    assert!(render_mesh(&cfg.mesh).contains("vlans = \"20,30\""));
}

#[test]
fn invalid_site_vlans_have_a_line() {
    let mesh = EXAMPLE_MESH.replace("vlans = \"20,30\"", "vlans = \"20,5000\"");
    let line = line_of(&mesh, "name = \"haus2\"", "vlans");
    assert_error(
        &mesh,
        line,
        "site \"haus2\": vlans: VLAN ID 5000 is outside 1-4094",
    );
}

#[test]
fn map_target_must_be_a_vlan_of_the_site() {
    let mesh = EXAMPLE_MESH.replace(
        "name = \"haus3\"\naddress = \"10.8.0.12\"\n",
        "name = \"haus3\"\naddress = \"10.8.0.12\"\nvlans = \"30\"\n",
    );
    let line = line_of(&mesh, "local = \"native\"", "shared");
    assert_error(
        &mesh,
        line,
        "shared VLAN 20 is not one of the VLANs of site \"haus3\" (30)",
    );
}

#[test]
fn invalid_site_name() {
    let mesh = EXAMPLE_MESH
        .replace("name = \"haus3\"", "name = \"Haus_3\"")
        .replace("site = \"haus3\"", "site = \"Haus_3\"");
    let line = line_of(&mesh, "", "name = \"Haus_3\"");
    assert_error(&mesh, line, "may only contain a-z");
    let long = "averyveryverylongname";
    let mesh = EXAMPLE_MESH
        .replace("name = \"haus3\"", &format!("name = \"{long}\""))
        .replace("site = \"haus3\"", &format!("site = \"{long}\""));
    let line = line_of(&mesh, "", long);
    assert_error(&mesh, line, "longer than 12 characters");
}

#[test]
fn shared_vid_must_be_in_range() {
    let mesh = EXAMPLE_MESH.replace("shared = 30", "shared = 60");
    let line = line_of(&mesh, "", "shared = 60");
    assert_error(
        &mesh,
        line,
        "shared VLAN 60 is not one of the VLANs of site \"haus3\" (10-50)",
    );
}

#[test]
fn at_most_one_native_mapping_per_site() {
    let mesh = EXAMPLE_MESH.replace("local = 130", "local = \"native\"");
    let line = line_of(&mesh, "shared = 20", "local = \"native\"");
    assert_error(
        &mesh,
        line,
        "site \"haus3\" has more than one native mapping",
    );
}

#[test]
fn two_locals_onto_one_shared_vlan() {
    let mesh = EXAMPLE_MESH.replace("shared = 30", "shared = 20");
    let line = line_of(&mesh, "local = 130", "shared = 20");
    assert_error(&mesh, line, "maps two local VLANs onto shared VLAN 20");
}

#[test]
fn duplicate_local_vid() {
    let mesh = format!("{EXAMPLE_MESH}\n[[map]]\nsite = \"haus3\"\nlocal = 130\nshared = 40\n");
    let line = line_of(&mesh, "shared = 30", "local = 130");
    assert_error(&mesh, line, "maps local VLAN 130 twice");
}

#[test]
fn map_onto_itself() {
    let mesh = EXAMPLE_MESH.replace("local = 130", "local = 30");
    let errs = errors(&mesh, LOCAL);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("mapped onto itself"))
    );
}

#[test]
fn map_site_must_exist() {
    let mesh = EXAMPLE_MESH.replacen("site = \"haus3\"", "site = \"haus9\"", 1);
    let line = line_of(&mesh, "", "site = \"haus9\"");
    assert_error(&mesh, line, "site \"haus9\" does not exist");
}

#[test]
fn local_vid_inside_shared_range_is_a_warning() {
    let mesh = EXAMPLE_MESH.replace("local = 130", "local = 15");
    let outcome = load(&mesh, LOCAL).expect("only a warning");
    assert_eq!(outcome.warnings.len(), 1);
    let w = &outcome.warnings[0];
    assert_eq!(w.severity, Severity::Warning);
    assert_eq!(w.line, Some(line_of(&mesh, "", "local = 15")));
}

#[test]
fn vlan_list_syntax_and_errors() {
    let mesh = EXAMPLE_MESH.replace("vlans = \"10-50\"", "vlans = [10, \"20-50\"]");
    assert!(load(&mesh, LOCAL).is_ok());
    let mesh = EXAMPLE_MESH.replace("vlans = \"10-50\"", "vlans = \"10-5000\"");
    let line = line_of(&mesh, "", "vlans");
    assert_error(&mesh, line, "outside 1-4094");
    let mesh = EXAMPLE_MESH.replace("vlans = \"10-50\"", "vlans = []");
    assert_error(&mesh, line, "empty VLAN list");
}

#[test]
fn without_vlans_everything_passes() {
    for mesh in [
        EXAMPLE_MESH.replace("vlans = \"10-50\"", ""),
        EXAMPLE_MESH.replace("vlans = \"10-50\"", "vlans = \"\""),
        EXAMPLE_MESH.replace("vlans = \"10-50\"", "vlans = \"all\""),
    ] {
        let outcome = load(&mesh, LOCAL).expect("valid");
        let Loaded::Configured(cfg) = outcome.loaded else {
            panic!("configured");
        };
        assert!(cfg.mesh.vlans.is_all());
        assert!(render_mesh(&cfg.mesh).contains("vlans = \"all\""));
    }
    // An empty list of a site means the default of the network.
    let mesh = EXAMPLE_MESH.replace("vlans = \"20,30\"", "vlans = \"\"");
    let Loaded::Configured(cfg) = load(&mesh, LOCAL).unwrap().loaded else {
        panic!("configured");
    };
    assert_eq!(cfg.mesh.site("haus2").unwrap().vlans, None);
}

#[test]
fn vlan_1_is_the_native_vlan_and_no_mapping_target() {
    let mesh = EXAMPLE_MESH.replace("shared = 30", "shared = 1");
    let line = line_of(&mesh, "local = 130", "shared = 1");
    assert_error(&mesh, line, "VLAN 1 is the native VLAN");
    let mesh = EXAMPLE_MESH.replace("local = 130", "local = 1");
    let line = line_of(&mesh, "", "local = 1");
    assert_error(&mesh, line, "write local = \"native\"");
}

#[test]
fn durations_and_mss() {
    let mesh = EXAMPLE_MESH.replace("commit_timeout = \"60s\"", "commit_timeout = \"soon\"");
    let line = line_of(&mesh, "", "commit_timeout");
    assert_error(&mesh, line, "is not a duration");
    let mesh = EXAMPLE_MESH.replace("commit_timeout = \"60s\"", "commit_timeout = \"2s\"");
    assert_error(&mesh, line, "between 10s and 1h");
    let mesh = EXAMPLE_MESH.replace("# mss = 1300", "mss = 100");
    let line = line_of(&mesh, "", "mss = 100");
    assert_error(&mesh, line, "mesh.mss must be between 536 and 1460");
    let mesh = EXAMPLE_MESH.replace("# mss = 1300", "mss = 1300");
    assert!(load(&mesh, LOCAL).is_ok());
}

#[test]
fn self_must_exist() {
    let err = single_error(EXAMPLE_MESH, "self = \"haus9\"\ntrunk = \"ens19\"\n");
    assert_eq!(err.file, ConfigFile::Local);
    assert_eq!(err.line, Some(1));
    assert!(err.message.contains("self = \"haus9\" is not a site"));
    assert!(err.to_string().starts_with("local.toml:1: error: "));
}

#[test]
fn trunk_is_checked() {
    let err = single_error(EXAMPLE_MESH, "self = \"haus2\"\n");
    assert!(err.message.contains("trunk is missing"));
    let err = single_error(EXAMPLE_MESH, "self = \"haus2\"\ntrunk = \"gt-heim\"\n");
    assert_eq!(err.line, Some(2));
    assert!(err.message.contains("managed by wanderlan"));
    let err = single_error(
        EXAMPLE_MESH,
        "self = \"haus2\"\ntrunk = \"a-very-long-ifname\"\n",
    );
    assert!(err.message.contains("longer than 15"));
}

#[test]
fn local_unknown_key() {
    let errs = errors(
        EXAMPLE_MESH,
        "self = \"haus2\"\ntrunk = \"ens19\"\nwg_port = 1\n",
    );
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].line, Some(3));
}

#[test]
fn check_without_local() {
    assert!(!has_errors(&check(EXAMPLE_MESH, None)));
    let bad = EXAMPLE_MESH.replace("site = \"haus3\"", "site = \"nope\"");
    let diags = check(&bad, None);
    assert!(has_errors(&diags));
    assert!(diags[0].line.is_some());
    assert!(has_errors(&check("# nothing\n", None)));
}

#[test]
fn reports_multiple_errors_at_once() {
    let mesh = EXAMPLE_MESH
        .replace("address = \"10.8.0.12\"", "address = \"10.8.0.11\"")
        .replace("shared = 30", "shared = 60");
    assert_eq!(errors(&mesh, LOCAL).len(), 2);
}

#[test]
fn mesh_model_validation_without_text() {
    // The TUI validates the model directly; paths identify the field.
    let mut mesh: Mesh = parse_mesh(EXAMPLE_MESH).unwrap().mesh.unwrap();
    mesh.sites[0].vlans = Some(crate::config::VlanSet::new());
    let diags = crate::config::validate(&mesh, None);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].path.as_str(), "site[0].vlans");
    assert_eq!(diags[0].line, None);
}

const CLIENTS: &str = r#"
[mesh]

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
    let mesh = parse_mesh(CLIENTS).unwrap().mesh.unwrap();
    // No `uplink`, but invited by alex: a client of alex.
    assert_eq!(mesh.site("marcel").unwrap().uplink.as_deref(), Some("alex"));
    assert_eq!(mesh.site("site2").unwrap().uplink.as_deref(), Some("alex"));
    assert_eq!(mesh.site("alex").unwrap().uplink, None);
    // Written explicitly from now on; the canonical form reads back the same.
    let text = render_mesh(&mesh);
    assert_eq!(text.matches("uplink = \"alex\"").count(), 2, "{text}");
    assert_eq!(parse_mesh(&text).unwrap().mesh.unwrap(), mesh);
    // A server with a manager stays a server: `uplink = ""`.
    let mut server = mesh.clone();
    server.sites[1].uplink = None;
    let text = render_mesh(&server);
    assert!(text.contains("uplink = \"\""), "{text}");
    assert_eq!(parse_mesh(&text).unwrap().mesh.unwrap(), server);
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
    let outcome = load(EXAMPLE_MESH, local).expect("valid");
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
    let again = load(EXAMPLE_MESH, &rendered).expect("valid");
    let Loaded::Configured(again) = again.loaded else {
        panic!("configured")
    };
    assert_eq!(again.local, cfg.local);
}

#[test]
fn the_trunk_of_a_tunnel_is_checked() {
    let local = "self = \"haus2\"\ntrunk = \"ens19\"\n\n[tunnels.heim]\ntrunk = \"gt-heim\"\n";
    let err = single_error(EXAMPLE_MESH, local);
    assert_eq!(err.file, ConfigFile::Local);
    assert!(err.message.contains("managed by wanderlan"), "{err}");
    assert_eq!(err.line, Some(5));
    let local = "self = \"haus2\"\ntrunk = \"ens19\"\n\n[tunnels.heim]\nport = 1\n";
    assert_eq!(errors(EXAMPLE_MESH, local).len(), 1, "unknown key");
    // A site that is no peer: a warning, the entry is not used.
    let local = "self = \"haus2\"\ntrunk = \"ens19\"\n\n[tunnels.nowhere]\ntrunk = \"ens20\"\n";
    let outcome = load(EXAMPLE_MESH, local).expect("only a warning");
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
