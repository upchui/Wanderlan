use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use super::*;
use crate::config::{Config, Loaded, load};
use crate::desired::{BRIDGE_OPTIONS, Runtime, derive};
use crate::sys::fake::FakeNet;
use crate::sys::tc::{IfTc, TcRule};
use crate::sys::{BridgeOptions, LinkKind, LinkState, NetOps, VlanFlags};

const NETWORK: &str = include_str!("../config/testdata/network-v1.toml");

fn config_from(network: &str, self_name: &str) -> Config {
    let local = format!("self = \"{self_name}\"\ntrunk = \"ens19\"\n");
    match load(network, &local).unwrap().loaded {
        Loaded::Configured(cfg) => cfg,
        Loaded::Unconfigured { .. } => panic!("configured"),
    }
}

fn desired(self_name: &str) -> Desired {
    derive(&config_from(NETWORK, self_name), &Runtime::default())
}

async fn run(net: &FakeNet, d: &Desired) -> Vec<Change> {
    reconcile(net, d, |_| {}).await.expect("reconcile")
}

fn fresh() -> FakeNet {
    let net = FakeNet::new();
    net.add_physical("ens19");
    net.add_physical("eth0");
    net
}

#[tokio::test]
async fn builds_a_site_from_scratch() {
    let net = fresh();
    let d = desired("heim");
    let changes = run(&net, &d).await;
    assert!(!changes.is_empty());

    let br = net.link("br0").unwrap();
    assert_eq!(br.alias.as_deref(), Some(OWNER_ALIAS));
    assert_eq!(br.kind, LinkKind::Bridge(BRIDGE_OPTIONS));
    assert!(br.admin_up);
    assert!(net.vlans("br0").is_empty());

    let trunk = net.link("ens19").unwrap();
    assert_eq!(trunk.master, Some(br.index));
    assert!(trunk.admin_up);
    assert_eq!(trunk.alias, None, "the trunk is not ours");
    assert_eq!(net.vlans("ens19").len(), 41);

    let buero = net.link("gt-buero").unwrap();
    assert_eq!(buero.master, Some(br.index));
    assert_eq!(buero.isolated, Some(true));
    assert_eq!(buero.mtu, 1500);
    assert!(buero.admin_up);
    match buero.kind {
        LinkKind::Gretap(p) => {
            assert_eq!(p.local, Ipv4Addr::new(10, 8, 0, 1));
            assert_eq!(p.remote, Ipv4Addr::new(10, 8, 0, 2));
            assert!(p.ignore_df && !p.pmtudisc);
        }
        other => panic!("unexpected kind {other:?}"),
    }
    for port in ["gt-haus2", "gt-haus3"] {
        assert_eq!(net.link(port).unwrap().isolated, Some(true), "{port}");
    }
    // haus2 only takes part in VLANs 20 and 30.
    let vlans = net.vlans("gt-haus2");
    assert_eq!(vlans.keys().copied().collect::<Vec<_>>(), vec![20, 30]);
    assert!(vlans.values().all(|f| *f == VlanFlags::TAGGED));
    let vlans = net.vlans("gt-haus3");
    assert_eq!(vlans.len(), 41);
    assert!(!vlans.contains_key(&1));

    // The untouched interface stays as it was.
    assert_eq!(net.link("eth0").unwrap().master, None);
}

#[tokio::test]
async fn second_run_changes_nothing() {
    let net = fresh();
    let d = desired("heim");
    run(&net, &d).await;
    net.take_calls();
    let changes = run(&net, &d).await;
    assert!(changes.is_empty(), "unexpected changes: {changes:?}");
    assert!(net.take_calls().is_empty());
}

#[tokio::test]
async fn native_mapping_sets_pvid_on_trunk() {
    let net = fresh();
    run(&net, &desired("haus3")).await;
    let vlans = net.vlans("ens19");
    assert_eq!(vlans.get(&20), Some(&VlanFlags::NATIVE));
    assert_eq!(vlans.get(&21), Some(&VlanFlags::TAGGED));
    // The tunnel carries VLAN 20 tagged.
    assert_eq!(net.vlans("gt-buero").get(&20), Some(&VlanFlags::TAGGED));
}

#[tokio::test]
async fn removed_peer_is_deleted_and_changed_address_recreated() {
    let net = fresh();
    run(&net, &desired("heim")).await;

    // haus2 leaves the network, buero gets a new address.
    let network = NETWORK
        .replace(
            "[[site]]\nname = \"haus2\"\naddress = \"10.8.0.11\"\nvlans = \"20,30\"               # haus2 only takes part in VLANs 20 and 30\n",
            "",
        )
        .replace("10.8.0.2\"", "10.8.0.22\"");
    assert!(!network.contains("haus2"));
    let d = derive(&config_from(&network, "heim"), &Runtime::default());
    let changes = run(&net, &d).await;
    let text: Vec<String> = changes.iter().map(ToString::to_string).collect();
    assert!(
        text.iter()
            .any(|t| t.contains("Deleting interface gt-haus2 (the site is no longer a peer)")),
        "{text:?}"
    );
    assert!(
        text.iter()
            .any(|t| t.contains("Deleting interface gt-buero (tunnel endpoints changed"))
    );
    assert!(net.link("gt-haus2").is_none());
    match net.link("gt-buero").unwrap().kind {
        LinkKind::Gretap(p) => assert_eq!(p.remote, Ipv4Addr::new(10, 8, 0, 22)),
        other => panic!("unexpected {other:?}"),
    }
    assert!(run(&net, &d).await.is_empty());
}

#[tokio::test]
async fn vlan_range_change_only_touches_vlans() {
    let net = fresh();
    run(&net, &desired("heim")).await;
    let network = NETWORK.replace("vlans = \"10-50\"", "vlans = \"10-40,60\"");
    let d = derive(&config_from(&network, "heim"), &Runtime::default());
    let changes = run(&net, &d).await;
    assert!(
        changes
            .iter()
            .all(|c| matches!(c, Change::AddVlans { .. } | Change::DelVlans { .. })),
        "{changes:?}"
    );
    let vlans = net.vlans("gt-buero");
    assert!(vlans.contains_key(&60) && !vlans.contains_key(&45));
    let del = changes
        .iter()
        .find(|c| matches!(c, Change::DelVlans { port, .. } if port == "ens19"))
        .unwrap();
    assert_eq!(del.to_string(), "Removing VLANs 41-50 from port ens19");
}

#[tokio::test]
async fn foreign_interfaces_are_not_touched() {
    let net = fresh();
    net.insert_link(LinkState {
        index: 0,
        name: "br0".into(),
        kind: LinkKind::Bridge(BRIDGE_OPTIONS),
        alias: None,
        master: None,
        admin_up: true,
        oper_up: true,
        mtu: 1500,
        isolated: None,
        traffic: crate::api::Traffic::default(),
    });
    let err = reconcile(&net, &desired("heim"), |_| {}).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("br0 already exists and was not created by wanderlan")
    );
    assert!(net.take_calls().is_empty());

    let net = fresh();
    net.insert_link(LinkState {
        index: 0,
        name: "gt-buero".into(),
        kind: LinkKind::Other("dummy".into()),
        alias: None,
        master: None,
        admin_up: false,
        oper_up: false,
        mtu: 1500,
        isolated: None,
        traffic: crate::api::Traffic::default(),
    });
    let err = plan(&desired("heim"), &Actual::observe(&net).await.unwrap()).unwrap_err();
    assert_eq!(err, PlanError::Foreign("gt-buero".into()));
}

#[tokio::test]
async fn trunk_problems_are_reported() {
    let net = FakeNet::new();
    let err = plan(&desired("heim"), &Actual::observe(&net).await.unwrap()).unwrap_err();
    assert_eq!(err, PlanError::TrunkMissing("ens19".into()));

    let net = fresh();
    let other = net.insert_link(LinkState {
        index: 0,
        name: "vmbr0".into(),
        kind: LinkKind::Bridge(BRIDGE_OPTIONS),
        alias: None,
        master: None,
        admin_up: true,
        oper_up: true,
        mtu: 1500,
        isolated: None,
        traffic: crate::api::Traffic::default(),
    });
    let trunk = net.link("ens19").unwrap();
    net.update_link(
        trunk.index,
        &LinkUpdate {
            master: Some(Some(other)),
            ..LinkUpdate::default()
        },
    )
    .await
    .unwrap();
    let err = plan(&desired("heim"), &Actual::observe(&net).await.unwrap()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "trunk interface ens19 is already attached to vmbr0; detach it first"
    );
}

#[tokio::test]
async fn teardown_removes_everything_we_created() {
    let net = fresh();
    run(&net, &desired("heim")).await;
    let changes = teardown(&net, |_| {}).await.unwrap();
    assert_eq!(changes.len(), 4, "bridge and three tunnels");
    let state = net.state();
    let names: Vec<&str> = state.links.values().map(|l| l.name.as_str()).collect();
    assert_eq!(names, vec!["ens19", "eth0"]);
    let trunk = net.link("ens19").unwrap();
    assert_eq!(trunk.master, None);
    assert!(net.vlans("ens19").is_empty());
    // Nothing left to do.
    assert!(teardown(&net, |_| {}).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_failing_step_does_not_stop_the_teardown() {
    let net = fresh();
    let limited = format!(
        "{NETWORK}\n[[tunnel]]\nbetween = [\"heim\", \"haus3\"]\nvlans = \"10-39,41-50\"\n"
    );
    run(
        &net,
        &derive(&config_from(&limited, "buero"), &Runtime::default()),
    )
    .await;
    assert!(
        net.state().mss.is_some(),
        "the split horizon needs the table"
    );
    net.fail_nft_delete();
    net.take_calls();
    let err = teardown(&net, |_| {}).await.unwrap_err().to_string();
    assert!(
        err.contains("delete nftables table bridge wanderlan: Device or resource busy"),
        "{err}"
    );
    let names: Vec<String> = net.state().links.values().map(|l| l.name.clone()).collect();
    assert_eq!(names, vec!["ens19", "eth0"], "bridge and tunnels are gone");
    let deletes = net
        .take_calls()
        .iter()
        .filter(|c| *c == "delete_table")
        .count();
    assert_eq!(deletes, 2, "the failed step is tried once more");
}

#[tokio::test]
async fn the_teardown_removes_the_links_without_the_tc_state() {
    let net = fresh();
    run(&net, &desired("heim")).await;
    net.fail_tc_state();
    let err = teardown(&net, |_| {}).await.unwrap_err().to_string();
    assert!(err.contains("list tc filters"), "{err}");
    let names: Vec<String> = net.state().links.values().map(|l| l.name.clone()).collect();
    assert_eq!(names, vec!["ens19", "eth0"]);
}

#[tokio::test]
async fn blocked_tunnel_is_brought_down_and_back_up() {
    let net = fresh();
    run(&net, &desired("heim")).await;
    let runtime = Runtime {
        blocked: BTreeSet::from(["gt-haus2".to_string()]),
        ..Runtime::default()
    };
    let d = derive(&config_from(NETWORK, "heim"), &runtime);
    let changes = run(&net, &d).await;
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0].to_string(),
        "Bringing gt-haus2 down (blocked by the loop guard)"
    );
    assert!(!net.link("gt-haus2").unwrap().admin_up);
    run(&net, &desired("heim")).await;
    assert!(net.link("gt-haus2").unwrap().admin_up);
}

#[tokio::test]
async fn existing_bridge_with_default_vlan_is_corrected() {
    let net = fresh();
    let mut options = BRIDGE_OPTIONS;
    options.default_pvid = 1;
    options.stp = true;
    net.create_bridge("br0", &options).await.unwrap();
    let br = net.link("br0").unwrap();
    net.update_link(
        br.index,
        &LinkUpdate {
            alias: Some(OWNER_ALIAS.into()),
            ..LinkUpdate::default()
        },
    )
    .await
    .unwrap();
    // Attach the trunk while the default PVID is still 1: it gets VLAN 1.
    let trunk = net.link("ens19").unwrap();
    net.update_link(
        trunk.index,
        &LinkUpdate {
            master: Some(Some(br.index)),
            ..LinkUpdate::default()
        },
    )
    .await
    .unwrap();
    assert!(net.vlans("ens19").contains_key(&1));
    net.add_vlans(br.index, true, &[(1, VlanFlags::NATIVE)])
        .await
        .unwrap();

    let changes = run(&net, &desired("heim")).await;
    assert!(
        changes
            .iter()
            .any(|c| matches!(c, Change::SetBridgeOptions { .. }))
    );
    assert!(!net.vlans("ens19").contains_key(&1));
    assert!(net.vlans("br0").is_empty());
    assert_eq!(
        net.link("br0").unwrap().kind,
        LinkKind::Bridge(BRIDGE_OPTIONS)
    );
}

/// A bridge of an older version, without the per-VLAN counters, with the
/// trunk and its VLANs.
async fn old_bridge(net: &FakeNet) {
    run(net, &desired("heim")).await;
    let br = net.link("br0").unwrap();
    let old = BridgeOptions {
        vlan_stats: false,
        ..BRIDGE_OPTIONS
    };
    net.insert_link(LinkState {
        kind: LinkKind::Bridge(old),
        ..br.clone()
    });
    // Move every port over to the old bridge (same name, new index).
    net.delete_link(br.index).await.unwrap();
    let old_br = net.link("br0").unwrap();
    for name in ["ens19", "gt-buero", "gt-haus2"] {
        let link = net.link(name).unwrap();
        net.update_link(
            link.index,
            &LinkUpdate {
                master: Some(Some(old_br.index)),
                ..LinkUpdate::default()
            },
        )
        .await
        .unwrap();
    }
    run(net, &desired_without_counters()).await;
    assert_eq!(
        net.link("br0").unwrap().kind,
        LinkKind::Bridge(old),
        "set up without counters"
    );
    assert_eq!(net.vlans("ens19").len(), 41);
}

fn desired_without_counters() -> Desired {
    let mut d = desired("heim");
    d.bridge_options.vlan_stats = false;
    d
}

#[tokio::test]
async fn a_bridge_without_vlan_counters_is_built_anew_once() {
    let net = fresh();
    old_bridge(&net).await;
    let changes = run(&net, &desired("heim")).await;
    assert!(
        matches!(&changes[..2], [Change::DeleteLink { name, .. }, Change::CreateBridge { name: created, .. }] if name == "br0" && created == "br0"),
        "{changes:?}"
    );
    let br = net.link("br0").unwrap();
    assert_eq!(br.kind, LinkKind::Bridge(BRIDGE_OPTIONS));
    // The ports are back, with their VLANs.
    assert_eq!(net.link("ens19").unwrap().master, Some(br.index));
    assert_eq!(net.link("gt-buero").unwrap().master, Some(br.index));
    assert_eq!(net.link("gt-buero").unwrap().isolated, Some(true));
    assert_eq!(net.vlans("ens19").len(), 41);
    assert!(run(&net, &desired("heim")).await.is_empty(), "only once");
}

#[tokio::test]
async fn a_kernel_without_vlan_counters_does_not_rebuild_forever() {
    let net = fresh();
    net.without_vlan_stats();
    let changes = run(&net, &desired("heim")).await;
    assert!(!changes.is_empty());
    let br = net.link("br0").unwrap();
    assert_eq!(
        br.kind,
        LinkKind::Bridge(BridgeOptions {
            vlan_stats: false,
            ..BRIDGE_OPTIONS
        })
    );
    assert!(!net.vlan_stats_supported());
    assert!(run(&net, &desired("heim")).await.is_empty(), "stable");
}

#[tokio::test]
async fn new_vlans_reach_the_ports_only_after_the_split_horizon_covers_them() {
    let net = fresh();
    let limited = format!(
        "{NETWORK}\n[[tunnel]]\nbetween = [\"heim\", \"haus3\"]\nvlans = \"10-39,41-50\"\n"
    );
    run(
        &net,
        &derive(&config_from(&limited, "buero"), &Runtime::default()),
    )
    .await;
    // The network gets VLANs 51-60, also on heim - haus3: split horizon.
    let wider = limited
        .replacen("vlans = \"10-50\"", "vlans = \"10-60\"", 1)
        .replace("vlans = \"10-39,41-50\"", "vlans = \"10-39,41-60\"");
    assert_ne!(wider, limited);
    let d = derive(&config_from(&wider, "buero"), &Runtime::default());
    let changes = plan(&d, &Actual::observe(&net).await.unwrap()).unwrap();
    let rules = changes
        .iter()
        .position(|c| matches!(c, Change::SetNft { .. }))
        .expect("the rules grow");
    let vlans = changes
        .iter()
        .position(|c| matches!(c, Change::AddVlans { port, .. } if port.starts_with("gt-")))
        .expect("the tunnels get the VLANs");
    assert!(rules < vlans, "{changes:#?}");
}

#[tokio::test]
async fn without_nftables_only_the_mss_clamping_is_missing() {
    let net = fresh();
    net.without_nft();
    let err = reconcile(&net, &desired("heim"), |_| {})
        .await
        .expect_err("the error is reported");
    assert!(err.to_string().contains("nft"), "{err}");
    // Everything else is in place: the tunnels are up.
    let buero = net.link("gt-buero").expect("the tunnel exists");
    assert!(buero.admin_up);
    assert_eq!(net.vlans("ens19").len(), 41);
    // With a split horizon it is a hard stop: no loops without the rules.
    let limited = format!(
        "{NETWORK}\n[[tunnel]]\nbetween = [\"heim\", \"haus3\"]\nvlans = \"10-39,41-50\"\n"
    );
    let d = derive(&config_from(&limited, "buero"), &Runtime::default());
    assert!(!d.split.is_empty());
    let err = reconcile(&net, &d, |_| {}).await.expect_err("refused");
    assert!(
        matches!(err, ReconcileError::Plan(PlanError::Nft(_))),
        "{err}"
    );
}

#[tokio::test]
async fn the_teardown_removes_the_tunnels_before_the_split_horizon() {
    let net = fresh();
    let limited = format!(
        "{NETWORK}\n[[tunnel]]\nbetween = [\"heim\", \"haus3\"]\nvlans = \"10-39,41-50\"\n"
    );
    run(
        &net,
        &derive(&config_from(&limited, "buero"), &Runtime::default()),
    )
    .await;
    let changes = plan_teardown(&Actual::observe(&net).await.unwrap());
    let last_tunnel = changes
        .iter()
        .rposition(|c| matches!(c, Change::DeleteLink { name, .. } if name.starts_with("gt-")))
        .expect("tunnels");
    let table = changes
        .iter()
        .position(|c| matches!(c, Change::DeleteNftTable))
        .expect("the table");
    let bridge = changes
        .iter()
        .position(|c| matches!(c, Change::DeleteLink { name, .. } if name == "br0"))
        .expect("the bridge");
    assert!(last_tunnel < table && table < bridge, "{changes:#?}");
}

#[test]
fn change_descriptions_are_readable() {
    let c = Change::AddVlans {
        port: "ens19".into(),
        vlans: vec![
            (10, VlanFlags::TAGGED),
            (11, VlanFlags::TAGGED),
            (20, VlanFlags::NATIVE),
        ],
    };
    assert_eq!(
        c.to_string(),
        "Adding VLANs 10-11 (tagged), 20 (pvid untagged) to port ens19"
    );
    let c = Change::SetIsolated {
        name: "gt-buero".into(),
        isolated: true,
    };
    assert!(c.to_string().contains("split horizon"));
}

#[tokio::test]
async fn port_changes_are_detected_for_fdb_flush() {
    let net = fresh();
    let changes = run(&net, &desired("heim")).await;
    assert!(ports_changed(&changes));
    let network = NETWORK.replace("vlans = \"10-50\"", "vlans = \"10-40\"");
    let d = derive(&config_from(&network, "heim"), &Runtime::default());
    let changes = run(&net, &d).await;
    assert!(!ports_changed(&changes), "VLAN changes do not move MACs");
    flush_learned(&net, "br0").await.unwrap();
    assert!(net.take_calls().iter().any(|c| c == "flush_fdb br0"));
}

#[test]
fn forwarding_key_reflects_tunnels() {
    use crate::topology::forwarding_key;
    let a = config_from(NETWORK, "heim").network;
    let mut b = a.clone();
    assert_eq!(forwarding_key(&a), forwarding_key(&b));
    b.vlans = crate::config::VlanSet::from_range(10, 20);
    assert_eq!(
        forwarding_key(&a),
        forwarding_key(&b),
        "VLANs do not matter"
    );
    b.sites.pop();
    assert_ne!(forwarding_key(&a), forwarding_key(&b));
}

#[tokio::test]
async fn translation_rules_are_installed_once() {
    let net = fresh();
    let d = desired("haus3");
    run(&net, &d).await;
    let tc = net.tc("ens19");
    assert!(tc.clsact);
    let mut cookies: Vec<String> = tc.ours.iter().map(TcRule::cookie).collect();
    cookies.sort();
    assert_eq!(cookies, vec!["wl:e30>130", "wl:i130>30", "wl:i30x"]);
    net.take_calls();
    assert!(run(&net, &d).await.is_empty());
    assert!(net.take_calls().is_empty());
}

#[tokio::test]
async fn translation_rules_of_an_older_version_move_into_one_priority() {
    let net = fresh();
    let d = desired("haus3");
    run(&net, &d).await;
    net.mark_tc_legacy("ens19");
    let changes = run(&net, &d).await;
    let dels = changes
        .iter()
        .filter(|c| matches!(c, Change::DelTcRule { .. }))
        .count();
    let adds = changes
        .iter()
        .filter(|c| matches!(c, Change::AddTcRule { .. }))
        .count();
    assert_eq!((dels, adds), (3, 3), "{changes:?}");
    let tc = net.tc("ens19");
    assert!(!tc.legacy);
    assert_eq!(tc.ours.len(), 3);
    assert!(run(&net, &d).await.is_empty(), "once");
}

#[tokio::test]
async fn removing_the_mapping_removes_rules_and_qdisc() {
    let net = fresh();
    run(&net, &desired("haus3")).await;
    let network = NETWORK.replace("local = 130\nshared = 30\n", "local = 131\nshared = 31\n");
    let d = derive(&config_from(&network, "haus3"), &Runtime::default());
    let changes = run(&net, &d).await;
    let text: Vec<String> = changes.iter().map(ToString::to_string).collect();
    assert!(
        text.iter()
            .any(|t| t == "On ens19: translate mapped VLAN 131 to source VLAN 31 (incoming)"),
        "{text:?}"
    );
    let mut cookies: Vec<String> = net.tc("ens19").ours.iter().map(TcRule::cookie).collect();
    cookies.sort();
    assert_eq!(cookies, vec!["wl:e31>131", "wl:i131>31", "wl:i31x"]);

    let network = NETWORK.replace("[[map]]\nsite = \"haus3\"\nlocal = 130\nshared = 30\n", "");
    let d = derive(&config_from(&network, "haus3"), &Runtime::default());
    run(&net, &d).await;
    assert_eq!(net.tc("ens19"), IfTc::default());
}

#[tokio::test]
async fn foreign_filters_are_kept() {
    let net = fresh();
    net.add_foreign_filter("ens19");
    run(&net, &desired("haus3")).await;
    let tc = net.tc("ens19");
    assert_eq!(tc.foreign, 1);
    assert_eq!(tc.ours.len(), 3);
    teardown(&net, |_| {}).await.unwrap();
    let tc = net.tc("ens19");
    assert!(tc.clsact, "the clsact qdisc is still used by someone else");
    assert!(tc.ours.is_empty());
    assert_eq!(tc.foreign, 1);
}

#[tokio::test]
async fn teardown_removes_translation_rules() {
    let net = fresh();
    run(&net, &desired("haus3")).await;
    teardown(&net, |_| {}).await.unwrap();
    assert_eq!(net.tc("ens19"), IfTc::default());
    assert!(net.link("br0").is_none());
}

#[tokio::test]
async fn mss_rules_follow_measurements() {
    let net = fresh();
    run(&net, &desired("heim")).await;
    assert_eq!(net.state().mss, None, "no measurement, no table");
    let mut runtime = Runtime::default();
    runtime.pmtu.insert("buero".into(), 1400);
    let d = derive(&config_from(NETWORK, "heim"), &runtime);
    let changes = run(&net, &d).await;
    assert_eq!(changes.len(), 1);
    assert!(
        changes[0]
            .to_string()
            .starts_with("Setting TCP MSS clamping")
    );
    assert_eq!(net.state().mss.unwrap().len(), 12);
    assert!(run(&net, &d).await.is_empty());
    teardown(&net, |_| {}).await.unwrap();
    assert_eq!(net.state().mss, None);
}

fn config_with_local(local: &str) -> Config {
    match load(NETWORK, local).unwrap().loaded {
        Loaded::Configured(cfg) => cfg,
        Loaded::Unconfigured { .. } => panic!("configured"),
    }
}

#[tokio::test]
async fn a_tunnel_with_its_own_trunk_is_a_segment_of_its_own() {
    let net = fresh();
    net.add_physical("ens20");
    let local = "self = \"heim\"\ntrunk = \"ens19\"\n\n[tunnels.haus2]\ntrunk = \"ens20\"\n";
    let d = derive(&config_with_local(local), &Runtime::default());
    assert_eq!(d.bridges().collect::<Vec<_>>(), ["br0", "wl-ens20"]);
    run(&net, &d).await;

    let br0 = net.link("br0").unwrap().index;
    let segment = net.link("wl-ens20").unwrap();
    assert_eq!(segment.alias.as_deref(), Some(OWNER_ALIAS));
    assert_eq!(segment.kind, LinkKind::Bridge(BRIDGE_OPTIONS));
    assert_eq!(net.link("ens20").unwrap().master, Some(segment.index));
    assert_eq!(net.link("gt-haus2").unwrap().master, Some(segment.index));
    assert_eq!(net.link("gt-buero").unwrap().master, Some(br0));
    assert_eq!(net.link("ens19").unwrap().master, Some(br0));
    assert_eq!(
        net.vlans("ens20"),
        net.vlans("ens19"),
        "every trunk carries the same VLANs"
    );
    assert_eq!(
        net.vlans("gt-haus2").keys().copied().collect::<Vec<_>>(),
        vec![20, 30]
    );
    assert!(
        run(&net, &d).await.is_empty(),
        "nothing to do the second time"
    );

    // Back to the default trunk: the segment disappears, the tunnel moves.
    run(&net, &desired("heim")).await;
    assert!(net.link("wl-ens20").is_none());
    assert_eq!(net.link("gt-haus2").unwrap().master, Some(br0));
    assert_eq!(net.link("ens20").unwrap().master, None);
    assert_eq!(
        net.vlans("gt-haus2").keys().copied().collect::<Vec<_>>(),
        vec![20, 30]
    );

    // Teardown removes every bridge of wanderlan.
    run(&net, &d).await;
    teardown(&net, |_| {}).await.unwrap();
    assert!(net.link("br0").is_none() && net.link("wl-ens20").is_none());
    assert_eq!(net.link("ens20").unwrap().master, None);
}

#[tokio::test]
async fn a_trunk_in_a_foreign_bridge_is_refused() {
    let net = fresh();
    let ens20 = net.add_physical("ens20");
    let vmbr = net.insert_link(LinkState {
        index: 0,
        name: "vmbr1".into(),
        kind: LinkKind::Bridge(BRIDGE_OPTIONS),
        alias: None,
        master: None,
        admin_up: true,
        oper_up: true,
        mtu: 1500,
        isolated: None,
        traffic: crate::api::Traffic::default(),
    });
    net.update_link(
        ens20,
        &LinkUpdate {
            master: Some(Some(vmbr)),
            ..LinkUpdate::default()
        },
    )
    .await
    .unwrap();
    let local = "self = \"heim\"\ntrunk = \"ens19\"\n\n[tunnels.haus2]\ntrunk = \"ens20\"\n";
    let d = derive(&config_with_local(local), &Runtime::default());
    let err = plan(&d, &Actual::observe(&net).await.unwrap()).unwrap_err();
    assert!(
        err.to_string()
            .contains("ens20 is already attached to vmbr1"),
        "{err}"
    );
}

#[tokio::test]
async fn the_split_horizon_moves_to_nftables_before_isolated_goes() {
    let net = fresh();
    run(&net, &desired("buero")).await;
    // VLAN 40 only on heim - buero and buero - haus3 (haus2 does not have
    // it): buero is the only way between heim and haus3.
    let limited = format!(
        "{NETWORK}\n[[tunnel]]\nbetween = [\"heim\", \"haus3\"]\nvlans = \"10-39,41-50\"\n"
    );
    let d = derive(&config_from(&limited, "buero"), &Runtime::default());
    assert!(
        !d.split.is_empty(),
        "the other VLANs keep the split horizon"
    );
    assert!(d.tunnels.iter().all(|t| !t.isolated), "{:?}", d.tunnels);
    let changes = run(&net, &d).await;
    let text: Vec<String> = changes.iter().map(ToString::to_string).collect();
    let rules = changes
        .iter()
        .position(|c| matches!(c, Change::SetNft { .. }))
        .expect("nftables rules");
    let unisolate = changes
        .iter()
        .position(|c| {
            matches!(
                c,
                Change::SetIsolated {
                    isolated: false,
                    ..
                }
            )
        })
        .expect("isolated goes");
    assert!(rules < unisolate, "{text:#?}");
    assert!(text[rules].contains("split horizon"), "{}", text[rules]);
    assert_eq!(net.state().split, d.split);
    assert!(run(&net, &d).await.is_empty(), "stable");
    // Back: isolated first, then the rules go.
    let back = run(&net, &desired("buero")).await;
    let isolate = back
        .iter()
        .position(|c| matches!(c, Change::SetIsolated { isolated: true, .. }))
        .expect("isolated again");
    let gone = back
        .iter()
        .position(|c| matches!(c, Change::DeleteNftTable))
        .expect("the table goes");
    assert!(isolate < gone);
    assert!(net.state().split.is_empty());
}

fn config_with(local: &str) -> Config {
    match load(NETWORK, local).unwrap().loaded {
        Loaded::Configured(cfg) => cfg,
        Loaded::Unconfigured { .. } => panic!("configured"),
    }
}

#[tokio::test]
async fn several_trunk_ports_join_the_bridge_and_leave_it_again() {
    let net = fresh();
    net.add_physical("ens20");
    let two = derive(
        &config_with("self = \"heim\"\ntrunk = [\"ens19\", \"ens20\"]\n"),
        &Runtime::default(),
    );
    let specs: Vec<(&str, &str, bool)> = two
        .trunks
        .iter()
        .map(|t| (t.name.as_str(), t.bridge.as_str(), t.required))
        .collect();
    assert_eq!(specs, [("ens19", "br0", true), ("ens20", "br0", false)]);
    run(&net, &two).await;
    let br = net.link("br0").unwrap().index;
    assert_eq!(net.link("ens20").unwrap().master, Some(br));
    assert_eq!(net.vlans("ens20"), net.vlans("ens19"), "the same VLANs");
    // Back to one port: ens20 leaves the bridge.
    let changes = run(&net, &desired("heim")).await;
    assert!(
        changes
            .iter()
            .any(|c| matches!(c, Change::Detach { name } if name == "ens20")),
        "{changes:?}"
    );
    assert_eq!(net.link("ens20").unwrap().master, None);
    assert!(run(&net, &desired("heim")).await.is_empty(), "stable");
}

#[tokio::test]
async fn a_missing_further_port_is_left_out_the_main_port_is_not() {
    let net = fresh();
    let d = derive(
        &config_with("self = \"heim\"\ntrunk = [\"ens19\", \"usb0\"]\n"),
        &Runtime::default(),
    );
    run(&net, &d).await;
    assert!(net.link("br0").is_some(), "the site runs without usb0");
    let main_missing = derive(
        &config_with("self = \"heim\"\ntrunk = [\"usb0\", \"ens19\"]\n"),
        &Runtime::default(),
    );
    assert!(reconcile(&net, &main_missing, |_| {}).await.is_err());
}

#[tokio::test]
async fn a_blocked_trunk_port_goes_down() {
    let net = fresh();
    net.add_physical("ens20");
    let cfg = config_with("self = \"heim\"\ntrunk = [\"ens19\", \"ens20\"]\n");
    run(&net, &derive(&cfg, &Runtime::default())).await;
    let mut runtime = Runtime::default();
    runtime.blocked.insert("ens20".into());
    run(&net, &derive(&cfg, &runtime)).await;
    assert!(!net.link("ens20").unwrap().admin_up);
    assert!(net.link("ens19").unwrap().admin_up, "never the main port");
}

#[test]
fn a_tunnel_with_several_ports_has_a_segment_of_its_own() {
    let d = derive(
        &config_with(
            "self = \"heim\"\ntrunk = \"ens19\"\n\n[tunnels.buero]\ntrunk = [\"ens21\", \"ens22\"]\n",
        ),
        &Runtime::default(),
    );
    let specs: Vec<(&str, &str)> = d
        .trunks
        .iter()
        .map(|t| (t.name.as_str(), t.bridge.as_str()))
        .collect();
    assert_eq!(
        specs,
        [
            ("ens19", "br0"),
            ("ens21", "wl-ens21"),
            ("ens22", "wl-ens21")
        ]
    );
    let buero = d.tunnels.iter().find(|t| t.peer == "buero").unwrap();
    assert_eq!(buero.bridge, "wl-ens21");
}
