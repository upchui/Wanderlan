//! Two changes of one network made at the same time, put together.
//!
//! Two sites changed the network from the same `base` (two servers within
//! a commit timeout, a client that pushes onto a view that has changed
//! since): `ours` and `theirs`. What only one of them changed is taken
//! over, unit by unit: each setting of the network, each field of each
//! site, each VLAN mapping (by site and source VLAN), each tunnel limit (by
//! its two sites). Where both changed the same unit differently, `theirs`
//! wins and the unit is named in [`Merge3::conflicts`]. Sites that got
//! another identity on one side (a claim, a box that joined again) are
//! followed on the other first.
//!
//! The result is not validated: two changes that are fine on their own can
//! contradict each other (the same mapped VLAN twice); the caller checks.

use std::collections::BTreeMap;

use crate::config::{Network, Site};
use crate::rename::rekeyed;

/// What [`merge3`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge3 {
    /// `theirs` with what only `ours` changed (the revision of `theirs`).
    pub network: Network,
    /// The units both changed differently (`theirs` kept them), in words.
    pub conflicts: Vec<String>,
    /// True if anything of `ours` was taken over.
    pub took_ours: bool,
}

/// Puts `ours` and `theirs`, both changed from `base`, together (see the
/// module doc).
pub fn merge3(base: &Network, ours: &Network, theirs: &Network) -> Merge3 {
    // The identities of both sides.
    let (base, theirs_rekeys) = rekeyed(base, theirs);
    let mut ours = ours.clone();
    for (from, to) in &theirs_rekeys {
        ours.rekey(*from, *to);
    }
    let (base, ours_rekeys) = rekeyed(&base, &ours);
    let mut out = theirs.clone();
    for (from, to) in &ours_rekeys {
        out.rekey(*from, *to);
    }
    let mut m = Merger {
        conflicts: Vec::new(),
        took: !ours_rekeys.is_empty(),
    };
    m.field(
        "the VLANs of the network",
        &base.vlans,
        &ours.vlans,
        &mut out.vlans,
    );
    m.field(
        "the control port",
        &base.control_port,
        &ours.control_port,
        &mut out.control_port,
    );
    m.field(
        "the tunnel port",
        &base.tunnel_port,
        &ours.tunnel_port,
        &mut out.tunnel_port,
    );
    m.field(
        "the path MTU check",
        &base.mtu_probe_interval,
        &ours.mtu_probe_interval,
        &mut out.mtu_probe_interval,
    );
    m.field("the MSS", &base.mss, &ours.mss, &mut out.mss);
    m.field(
        "the commit timeout",
        &base.commit_timeout,
        &ours.commit_timeout,
        &mut out.commit_timeout,
    );
    m.sites(&base, &ours, &mut out);
    let names = out.clone();
    out.maps = m.entries(
        &base.maps,
        &ours.maps,
        &out.maps,
        |v| (v.site, v.shared),
        |(site, vid)| {
            format!(
                "the mapping of source VLAN {vid} at site \"{}\"",
                names.label(*site)
            )
        },
    );
    out.tunnels = m.entries(
        &base.tunnels,
        &ours.tunnels,
        &out.tunnels,
        |t| t.sites,
        |[a, b]| {
            format!(
                "the VLANs of the tunnel between \"{}\" and \"{}\"",
                names.label(*a),
                names.label(*b)
            )
        },
    );
    out.canonicalize();
    Merge3 {
        network: out,
        conflicts: m.conflicts,
        took_ours: m.took,
    }
}

struct Merger {
    conflicts: Vec<String>,
    took: bool,
}

impl Merger {
    /// One unit: `out` holds theirs; ours where only ours changed it.
    fn field<T: PartialEq + Clone>(&mut self, what: &str, base: &T, ours: &T, out: &mut T) {
        if ours == base || ours == out {
            return;
        }
        if out == base {
            out.clone_from(ours);
            self.took = true;
        } else {
            self.conflicts.push(what.to_string());
        }
    }

    /// The sites: added, removed, and each field of the others.
    fn sites(&mut self, base: &Network, ours: &Network, out: &mut Network) {
        // Removed here: gone, unless changed there.
        for b in &base.sites {
            if ours.site(b.id).is_some() {
                continue;
            }
            match out.site(b.id) {
                Some(t) if t == b => {
                    out.sites.retain(|s| s.id != b.id);
                    self.took = true;
                }
                Some(t) => self
                    .conflicts
                    .push(format!("site \"{}\" (removed here, changed there)", t.name)),
                None => {}
            }
        }
        for o in &ours.sites {
            match (base.site(o.id), out.sites.iter_mut().find(|s| s.id == o.id)) {
                // Added here only: added (after their sites).
                (None, None) => {
                    out.sites.push(o.clone());
                    self.took = true;
                }
                (None, Some(t)) => {
                    if t != o {
                        self.conflicts
                            .push(format!("site \"{}\" (added on both sides)", o.name));
                    }
                }
                (Some(b), None) => {
                    if o != b {
                        self.conflicts
                            .push(format!("site \"{}\" (changed here, removed there)", o.name));
                    }
                }
                (Some(b), Some(t)) => self.site(b, o, t),
            }
        }
    }

    /// Each field of a site both have.
    fn site(&mut self, base: &Site, ours: &Site, out: &mut Site) {
        let what = |field: &str| format!("{field} of site \"{}\"", base.name);
        self.field(&what("the name"), &base.name, &ours.name, &mut out.name);
        self.field(
            &what("the address"),
            &base.address,
            &ours.address,
            &mut out.address,
        );
        self.field(&what("the VLANs"), &base.vlans, &ours.vlans, &mut out.vlans);
        self.field(
            &what("the own VLANs"),
            &base.own_vlans,
            &ours.own_vlans,
            &mut out.own_vlans,
        );
        self.field(
            &what("the online state"),
            &base.online,
            &ours.online,
            &mut out.online,
        );
        self.field(
            &what("who changes"),
            &base.changes,
            &ours.changes,
            &mut out.changes,
        );
        self.field(
            &what("the server"),
            &base.uplink,
            &ours.uplink,
            &mut out.uplink,
        );
        self.field(
            &what("the breakout"),
            &base.breakout,
            &ours.breakout,
            &mut out.breakout,
        );
        self.field(&what("the ports"), &base.ports, &ours.ports, &mut out.ports);
    }

    /// Entries with a key (mappings, tunnel limits): added, removed or
    /// changed here only are taken over.
    fn entries<T: Clone + PartialEq, K: Ord + Clone>(
        &mut self,
        base: &[T],
        ours: &[T],
        theirs: &[T],
        key: impl Fn(&T) -> K,
        what: impl Fn(&K) -> String,
    ) -> Vec<T> {
        let by =
            |list: &[T]| -> BTreeMap<K, T> { list.iter().map(|x| (key(x), x.clone())).collect() };
        let (b, o) = (by(base), by(ours));
        let mut out = theirs.to_vec();
        let mut keys: Vec<&K> = b.keys().chain(o.keys()).collect();
        keys.sort();
        keys.dedup();
        for k in keys {
            let (bv, ov) = (b.get(k), o.get(k));
            let at = out.iter().position(|x| key(x) == *k);
            let tv = at.map(|i| &out[i]);
            if bv == ov || tv == ov {
                continue;
            }
            if tv != bv {
                self.conflicts.push(what(k));
                continue;
            }
            match (ov, at) {
                (Some(o), Some(i)) => out[i] = o.clone(),
                (Some(o), None) => out.push(o.clone()),
                (None, Some(i)) => {
                    out.remove(i);
                }
                (None, None) => {}
            }
            self.took = true;
        }
        out
    }
}

/// The fingerprints of the networks a site confirmed last, each with the
/// one it was made from: the bases [`merge3`] needs. Bounded; the oldest go
/// first.
#[derive(Debug, Clone, Default)]
pub struct Known {
    known: Vec<(String, Network)>,
}

impl Known {
    /// How many networks are kept.
    pub const KEEP: usize = 20;

    /// Remembers `network` under its fingerprint `fp`.
    pub fn remember(&mut self, fp: &str, network: &Network) {
        self.known.retain(|(f, _)| f != fp);
        self.known.push((fp.to_string(), network.clone()));
        if self.known.len() > Self::KEEP {
            self.known.remove(0);
        }
    }

    /// The network with the fingerprint `fp`, if known.
    pub fn get(&self, fp: &str) -> Option<&Network> {
        self.known.iter().find(|(f, _)| f == fp).map(|(_, n)| n)
    }

    /// The networks, the newest first.
    pub fn iter(&self) -> impl Iterator<Item = &Network> {
        self.known.iter().rev().map(|(_, n)| n)
    }

    /// Forgets everything (a reset).
    pub fn clear(&mut self) {
        self.known.clear();
    }
}

/// The fingerprints a network was made from, the newest first (bounded):
/// whether a network another site has already holds the changes of this
/// one, and where both started from if not. Sent with the network (see
/// `wanderland`'s sync), never part of its fingerprint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ancestry(Vec<String>);

impl Ancestry {
    /// How many fingerprints are kept.
    pub const KEEP: usize = 16;

    /// Reads the fingerprints separated by spaces (anything else is left
    /// out).
    pub fn parse(words: &str) -> Self {
        let is_fp = |w: &&str| w.len() == 64 && w.bytes().all(|b| b.is_ascii_hexdigit());
        let mut out = Self(Vec::new());
        for w in words.split_whitespace().filter(is_fp) {
            out.push(w);
        }
        out
    }

    /// The fingerprints separated by spaces.
    pub fn render(&self) -> String {
        self.0.join(" ")
    }

    /// True if there are none (a network of an older version).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// True if `fp` is one of them.
    pub fn contains(&self, fp: &str) -> bool {
        self.0.iter().any(|f| f == fp)
    }

    /// The ancestry of a network made from the one with fingerprint `fp`
    /// and this ancestry.
    pub fn child(&self, fp: &str) -> Self {
        let mut out = Self(vec![fp.to_string()]);
        for f in &self.0 {
            out.push(f);
        }
        out
    }

    /// The ancestry of a network put together from two: both, each with
    /// its own.
    pub fn joined(a: (&str, &Self), b: (&str, &Self)) -> Self {
        let mut out = a.1.child(a.0);
        out.push(b.0);
        for f in &b.1.0 {
            out.push(f);
        }
        out
    }

    fn push(&mut self, fp: &str) {
        if self.0.len() < Self::KEEP && !self.contains(fp) {
            self.0.push(fp.to_string());
        }
    }
}

/// A view the client `client` changed from an older view than the one it
/// gets from `full` now: its changes put onto the current view (where both
/// changed the same, the current view's). `None` if it was made from the
/// current view, or from one not in `history` (the networks confirmed
/// here).
pub fn rebase_view(
    history: &Known,
    full: &Network,
    client: crate::identity::SiteId,
    pushed: &Network,
    ancestry: &Ancestry,
) -> Option<Merge3> {
    use crate::config::fingerprint;
    let current = crate::view::view_for(full, client);
    let current_fp = fingerprint(&current);
    if ancestry.is_empty() || ancestry.contains(&current_fp) || fingerprint(pushed) == current_fp {
        return None;
    }
    let views: Vec<(String, Network)> = history
        .iter()
        .map(|n| {
            let v = crate::view::view_for(n, client);
            (fingerprint(&v), v)
        })
        .collect();
    let base = ancestry
        .0
        .iter()
        .find_map(|f| views.iter().find(|(vf, _)| vf == f).map(|(_, v)| v))?;
    let mut m = merge3(base, pushed, &current);
    m.network.revision = m.network.revision.max(pushed.revision);
    // Not what the client has: above its revision, else it keeps its own
    // (it takes only a higher one from its uplink).
    if fingerprint(&m.network) != fingerprint(pushed) {
        m.network.revision = crate::config::next_revision(m.network.revision);
    }
    Some(m)
}

/// What becomes of the confirmed network `ours` when a newer `theirs`
/// arrives that replaces it (from a server: see [`on_replace`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replace {
    /// It holds everything of ours (or ours has nothing of its own).
    Take,
    /// Both changed it: put together, their revision counted on.
    Merged(Merge3),
    /// Ours holds a change that theirs lacks, and where both started from is
    /// not known (any more): ours is lost.
    Loses,
}

/// Decides how `theirs` (with its ancestry) replaces `ours`: `own` tells
/// whether ours holds a change made at this site that may not have reached
/// others; `history` holds the networks confirmed here, the bases.
pub fn on_replace(
    ours: (&Network, &Ancestry),
    own: bool,
    history: &Known,
    theirs: (&Network, &Ancestry),
) -> Replace {
    let ours_fp = crate::config::fingerprint(ours.0);
    if !own || theirs.1.contains(&ours_fp) {
        return Replace::Take;
    }
    // The newest network both were made from.
    let Some(base) = theirs
        .1
        .0
        .iter()
        .filter(|f| ours.1.contains(f))
        .find_map(|f| history.get(f))
    else {
        return Replace::Loses;
    };
    let mut m = merge3(base, ours.0, theirs.0);
    if !m.took_ours {
        return Replace::Take;
    }
    m.network.revision = crate::config::next_revision(ours.0.revision.max(theirs.0.revision));
    Replace::Merged(m)
}

#[cfg(test)]
mod tests;
