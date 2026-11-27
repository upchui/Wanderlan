use std::net::Ipv4Addr;

use super::*;
use crate::config::{LocalVid, Site, VlanMap, VlanSet};
use crate::identity::SiteId;

fn real(n: u8) -> SiteId {
    SiteId::from_bytes([n; 32])
}

fn site(name: &str, n: u8) -> Site {
    Site {
        id: real(n),
        ..Site::new(name, Ipv4Addr::new(10, 0, 0, n))
    }
}

fn vlans(text: &str) -> VlanSet {
    text.parse().unwrap()
}

/// hq, a and b, all online.
fn base() -> Network {
    let mut network = Network::new(vlans("10,20,30"));
    network.revision = 7;
    network.sites = vec![site("hq", 1), site("a", 2), site("b", 3)];
    for s in &mut network.sites {
        s.online = true;
    }
    network
}

fn map(n: u8, local: u16, shared: u16) -> VlanMap {
    VlanMap {
        site: real(n),
        local: LocalVid::Vid(local),
        shared,
    }
}

#[test]
fn changes_of_different_units_are_both_kept() {
    let base = base();
    // Here: a mapping at a, b offline, a new site.
    let mut ours = base.clone();
    ours.maps.push(map(2, 110, 10));
    ours.site_mut(real(3)).unwrap().online = false;
    ours.sites.push(site("new", 9));
    // There: another revision, a's VLANs, the MSS, hq renamed.
    let mut theirs = base.clone();
    theirs.revision = 8;
    theirs.site_mut(real(2)).unwrap().vlans = Some(vlans("10,30"));
    theirs.mss = Some(1300);
    theirs.rename_site(real(1), "main");
    let m = merge3(&base, &ours, &theirs);
    assert!(m.conflicts.is_empty(), "{:?}", m.conflicts);
    assert!(m.took_ours);
    let n = &m.network;
    assert_eq!(n.revision, 8, "the revision of theirs");
    assert_eq!(n.maps, vec![map(2, 110, 10)]);
    assert!(!n.site(real(3)).unwrap().online);
    assert_eq!(n.site(real(2)).unwrap().vlans, Some(vlans("10,30")));
    assert_eq!(n.mss, Some(1300));
    assert_eq!(n.site(real(1)).unwrap().name, "main");
    assert_eq!(n.sites.last().unwrap().name, "new", "added after theirs");
    // Nothing of ours that theirs lacks: theirs as it is.
    let same = merge3(&base, &base, &theirs);
    assert!(!same.took_ours && same.conflicts.is_empty());
    assert_eq!(same.network, theirs);
}

#[test]
fn the_same_unit_changed_differently_keeps_theirs() {
    let mut base = base();
    base.maps.push(map(2, 110, 10));
    let mut ours = base.clone();
    ours.mss = Some(1200);
    ours.maps = vec![map(2, 120, 10)];
    ours.site_mut(real(3)).unwrap().online = false;
    let mut theirs = base.clone();
    theirs.mss = Some(1300);
    theirs.maps.clear();
    // The same change on both sides is no conflict.
    theirs.site_mut(real(3)).unwrap().online = false;
    let m = merge3(&base, &ours, &theirs);
    assert_eq!(m.network.mss, Some(1300));
    assert!(m.network.maps.is_empty(), "removed there, changed here");
    assert_eq!(
        m.conflicts,
        vec![
            "the MSS".to_string(),
            "the mapping of source VLAN 10 at site \"a\"".to_string(),
        ]
    );
}

#[test]
fn removed_sites_and_entries() {
    let mut base = base();
    base.maps.push(map(3, 130, 30));
    // Here b is removed with its mapping; there a changed.
    let mut ours = base.clone();
    ours.sites.retain(|s| s.id != real(3));
    ours.maps.clear();
    let mut theirs = base.clone();
    theirs.site_mut(real(2)).unwrap().online = false;
    let m = merge3(&base, &ours, &theirs);
    assert!(m.conflicts.is_empty(), "{:?}", m.conflicts);
    assert!(m.network.site(real(3)).is_none());
    assert!(m.network.maps.is_empty());
    assert!(!m.network.site(real(2)).unwrap().online);
    // Removed here, changed there: kept, a conflict.
    let mut theirs = base.clone();
    theirs.site_mut(real(3)).unwrap().online = false;
    let m = merge3(&base, &ours, &theirs);
    assert!(m.network.site(real(3)).is_some());
    assert_eq!(
        m.conflicts,
        vec!["site \"b\" (removed here, changed there)"]
    );
}

#[test]
fn a_claimed_identity_is_followed() {
    // b has its provisional identity in the base; it claimed its own there.
    let mut base = base();
    base.sites[2].id = SiteId::provisional("b");
    base.maps.push(VlanMap {
        site: SiteId::provisional("b"),
        ..map(3, 130, 30)
    });
    let mut theirs = base.clone();
    theirs.rekey(SiteId::provisional("b"), real(3));
    theirs.revision = 8;
    // Here b goes offline.
    let mut ours = base.clone();
    ours.site_mut(SiteId::provisional("b")).unwrap().online = false;
    let m = merge3(&base, &ours, &theirs);
    assert!(m.conflicts.is_empty(), "{:?}", m.conflicts);
    assert_eq!(m.network.sites.len(), 3);
    assert!(!m.network.site(real(3)).unwrap().online);
    assert_eq!(m.network.maps, vec![map(3, 130, 30)]);
}

#[test]
fn history_is_bounded() {
    let mut h = Known::default();
    for i in 0..30u64 {
        let mut n = base();
        n.revision = i;
        h.remember(&format!("fp{i}"), &n);
    }
    assert!(h.get("fp0").is_none());
    assert_eq!(h.get("fp29").map(|n| n.revision), Some(29));
    assert_eq!(h.get("fp10").map(|n| n.revision), Some(10));
}

fn fp(n: &Network) -> String {
    crate::config::fingerprint(n)
}

#[test]
fn ancestry_is_bounded_and_read_back() {
    let a = Ancestry::parse(&format!("{} junk {}", "a".repeat(64), "b".repeat(64)));
    assert_eq!(a.render(), format!("{} {}", "a".repeat(64), "b".repeat(64)));
    let mut long = Ancestry::default();
    for i in 0..40u8 {
        long = long.child(&format!("{:064x}", i));
    }
    assert_eq!(long.render().split(' ').count(), Ancestry::KEEP);
    assert!(long.contains(&format!("{:064x}", 39)), "the newest first");
    assert!(!long.contains(&format!("{:064x}", 0)));
}

#[test]
fn a_newer_network_is_merged_only_where_both_changed() {
    let base = base();
    let mut history = Known::default();
    history.remember(&fp(&base), &base);
    let origin = Ancestry::default();
    // Ours: b offline, made here from the base.
    let mut ours = base.clone();
    ours.revision = 8;
    ours.site_mut(real(3)).unwrap().online = false;
    let ours_anc = origin.child(&fp(&base));
    // Theirs: the MSS, made elsewhere from the same base.
    let mut theirs = base.clone();
    theirs.revision = 8;
    theirs.mss = Some(1300);
    let theirs_anc = origin.child(&fp(&base));
    let Replace::Merged(m) = on_replace((&ours, &ours_anc), true, &history, (&theirs, &theirs_anc))
    else {
        panic!("not merged");
    };
    assert_eq!(m.network.revision, 9, "above both");
    assert_eq!(m.network.mss, Some(1300));
    assert!(!m.network.site(real(3)).unwrap().online);
    // Not a change of this site, or theirs was made from ours: taken.
    let take = |own, anc: &Ancestry| on_replace((&ours, &ours_anc), own, &history, (&theirs, anc));
    assert_eq!(take(false, &theirs_anc), Replace::Take);
    assert_eq!(take(true, &theirs_anc.child(&fp(&ours))), Replace::Take);
    // Where both started is not known: ours is lost.
    assert_eq!(take(true, &Ancestry::default()), Replace::Loses);
    let unknown = Ancestry::default().child(&"f".repeat(64));
    assert_eq!(take(true, &unknown), Replace::Loses);
}

#[test]
fn a_view_changed_from_an_older_one_keeps_the_newer_changes() {
    // b is a client of hq.
    let mut full = base();
    full.site_mut(real(3)).unwrap().uplink = Some(real(1));
    let mut history = Known::default();
    history.remember(&fp(&full), &full);
    let old_view = crate::view::view_for(&full, real(3));
    // Meanwhile at hq: the MSS; b sends a change made on the old view.
    let mut now = full.clone();
    now.mss = Some(1300);
    now.revision = 8;
    let mut pushed = old_view.clone();
    pushed.site_mut(real(3)).unwrap().vlans = Some(vlans("10"));
    pushed.revision = 8;
    let anc = Ancestry::default().child(&fp(&old_view));
    let m = rebase_view(&history, &now, real(3), &pushed, &anc).unwrap();
    assert_eq!(m.network.mss, Some(1300), "the change of hq stays");
    assert_eq!(m.network.site(real(3)).unwrap().vlans, Some(vlans("10")));
    assert_eq!(m.network.revision, 9, "above the client's: it takes it");
    // Made from the current view, or an unknown one: as it is.
    let current = Ancestry::default().child(&fp(&crate::view::view_for(&now, real(3))));
    assert!(rebase_view(&history, &now, real(3), &pushed, &current).is_none());
    assert!(rebase_view(&Known::default(), &now, real(3), &pushed, &anc).is_none());
}
