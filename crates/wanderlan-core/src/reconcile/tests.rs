use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use super::*;
use crate::config::{Config, Loaded, load};
use crate::desired::{BRIDGE_OPTIONS, Runtime, derive};
use crate::sys::fake::FakeNet;
use crate::sys::tc::{IfTc, TcRule};
use crate::sys::{LinkKind, LinkState, NetOps, VlanFlags};

const NETWORK: &str = include_str!("../../../../packaging/examples/network.toml");

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
async fn removing_the_mapping_removes_rules_and_qdisc() {
    let net = fresh();
    run(&net, &desired("haus3")).await;
    let network = NETWORK.replace("local = 130\nshared = 30\n", "local = 131\nshared = 31\n");
    let d = derive(&config_from(&network, "haus3"), &Runtime::default());
    let changes = run(&net, &d).await;
    let text: Vec<String> = changes.iter().map(ToString::to_string).collect();
    assert!(
        text.iter()
            .any(|t| t == "On ens19: translate local VLAN 131 to shared VLAN 31 (incoming)"),
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
