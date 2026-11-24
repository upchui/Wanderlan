//! The kernel's fallback tunnel devices.
//!
//! When wanderlan creates its first GRETAP tunnel, the kernel loads the
//! module `ip_gre`, which creates the devices `gre0`, `gretap0` and
//! `erspan0` in every network namespace. They carry no traffic, but only
//! unloading the module removes them. After a reset this is done, if it is
//! safe: as root, outside containers, and only if no other GRE device uses
//! the module.

use std::path::Path;
use std::process::Command;

use wanderlan_core::api::Interface;

/// The module that owns the fallback devices.
pub const GRE_MODULE: &str = "ip_gre";
/// Device kinds served by [`GRE_MODULE`].
const GRE_KINDS: [&str; 3] = ["gre", "gretap", "erspan"];
/// The fallback devices of [`GRE_MODULE`].
const GRE_FALLBACK: [&str; 3] = ["gre0", "gretap0", "erspan0"];

/// What happened to the fallback devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cleanup {
    /// The module is not loaded; there is nothing to remove.
    NotLoaded,
    /// The module was unloaded; the devices are gone.
    Removed,
    /// Other GRE devices use the module.
    InUse(Vec<String>),
    /// This is a container: the module belongs to the host.
    Container,
    /// Unloading needs root.
    NotRoot,
    /// `modprobe -r` failed.
    Failed(String),
}

impl Cleanup {
    /// A sentence for the user, `None` if there is nothing to say.
    pub fn message(&self) -> Option<String> {
        let devices = "gre0, gretap0 and erspan0";
        match self {
            Self::NotLoaded => None,
            Self::Removed => Some(format!(
                "Unloaded the kernel module {GRE_MODULE}; its devices {devices} are gone."
            )),
            Self::InUse(names) => Some(format!(
                "The kernel's devices {devices} stay: the module {GRE_MODULE} is also used by {}.",
                names.join(", ")
            )),
            Self::Container => Some(format!(
                "The devices {devices} belong to the kernel module {GRE_MODULE} of the host (this is a container); they stay."
            )),
            Self::NotRoot => Some(format!(
                "The devices {devices} belong to the kernel module {GRE_MODULE}; as root, `modprobe -r {GRE_MODULE}` removes them."
            )),
            Self::Failed(e) => Some(format!(
                "The kernel module {GRE_MODULE} could not be unloaded ({e}); {devices} stay."
            )),
        }
    }
}

/// GRE devices that are not fallback devices.
fn foreign_gre(interfaces: &[Interface]) -> Vec<String> {
    interfaces
        .iter()
        .filter(|i| {
            GRE_KINDS.contains(&i.kind.as_str()) && !GRE_FALLBACK.contains(&i.name.as_str())
        })
        .map(|i| i.name.clone())
        .collect()
}

fn module_loaded(name: &str) -> bool {
    Path::new("/sys/module").join(name).join("refcnt").exists()
}

/// True inside a container (Docker, Podman, systemd-nspawn, LXC).
fn in_container() -> bool {
    [
        "/run/systemd/container",
        "/.dockerenv",
        "/run/.containerenv",
    ]
    .iter()
    .any(|p| Path::new(p).exists())
        || std::fs::read("/proc/1/environ")
            .is_ok_and(|env| env.split(|b| *b == 0).any(|v| v.starts_with(b"container=")))
}

/// True if the effective user is root.
fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status").is_ok_and(|status| {
        status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|ids| ids.split_whitespace().nth(1).map(|euid| euid == "0"))
            .unwrap_or(false)
    })
}

/// Removes `gre0`, `gretap0` and `erspan0` by unloading [`GRE_MODULE`] if
/// that is safe. `interfaces` is the current interface list of this host.
pub fn remove_gre_fallback(interfaces: &[Interface]) -> Cleanup {
    if !module_loaded(GRE_MODULE) {
        return Cleanup::NotLoaded;
    }
    let foreign = foreign_gre(interfaces);
    if !foreign.is_empty() {
        return Cleanup::InUse(foreign);
    }
    if in_container() {
        return Cleanup::Container;
    }
    if !is_root() {
        return Cleanup::NotRoot;
    }
    match Command::new("modprobe").args(["-r", GRE_MODULE]).output() {
        Ok(out) if out.status.success() => {
            if module_loaded(GRE_MODULE) {
                Cleanup::Failed("it is still loaded".into())
            } else {
                Cleanup::Removed
            }
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Cleanup::Failed(if err.is_empty() {
                format!("modprobe exited with {}", out.status)
            } else {
                err
            })
        }
        Err(e) => Cleanup::Failed(format!("cannot run modprobe: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, kind: &str) -> Interface {
        Interface {
            name: name.into(),
            kind: kind.into(),
            up: false,
            master: None,
            mtu: 1476,
            managed: false,
        }
    }

    #[test]
    fn only_other_gre_devices_keep_the_module() {
        let list = vec![
            iface("gre0", "gre"),
            iface("gretap0", "gretap"),
            iface("erspan0", "erspan"),
            iface("eth0", ""),
        ];
        assert!(foreign_gre(&list).is_empty());
        let mut with_vpn = list;
        with_vpn.push(iface("gre-office", "gre"));
        assert_eq!(foreign_gre(&with_vpn), vec!["gre-office".to_string()]);
    }

    #[test]
    fn every_outcome_but_not_loaded_is_explained() {
        assert_eq!(Cleanup::NotLoaded.message(), None);
        for c in [
            Cleanup::Removed,
            Cleanup::InUse(vec!["gre-x".into()]),
            Cleanup::Container,
            Cleanup::NotRoot,
            Cleanup::Failed("busy".into()),
        ] {
            assert!(c.message().is_some_and(|m| m.contains("gre0")), "{c:?}");
        }
    }
}
