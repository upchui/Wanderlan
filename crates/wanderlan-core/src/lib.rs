//! Core library of wanderlan.
//!
//! This crate contains everything that does not need a running kernel:
//! loading and validating the configuration, computing the tunnel topology
//! and deriving the desired system state. System access is abstracted behind
//! traits so that all logic can be tested without root privileges.

pub mod api;
pub mod config;
pub mod desired;
pub mod forwarding;
pub mod join;
pub mod loopguard;
pub mod mtu;
pub mod policy;
pub mod reconcile;
pub mod sync;
pub mod sys;
pub mod topology;
pub mod view;

/// Name of the bridge created by wanderlan.
pub const BRIDGE_NAME: &str = "br0";

/// Version of the wanderlan workspace, shared by all binaries.
///
/// Builds set `WANDERLAN_VERSION` to the version of the commit, e.g.
/// `1.0.2` (`scripts/version.sh`); otherwise it is the version from
/// `Cargo.toml`.
pub const VERSION: &str = match option_env!("WANDERLAN_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

/// The commit the binaries were built from (7 characters, `-modified` for a
/// changed working tree), if the build knew it (`WANDERLAN_COMMIT`).
pub const COMMIT: Option<&str> = match option_env!("WANDERLAN_COMMIT") {
    Some(c) if !c.is_empty() => Some(c),
    _ => None,
};

/// `1.0.2 (c463aa1)`, or `1.0.2` without a known commit.
pub fn format_version(version: &str, commit: Option<&str>) -> String {
    match commit {
        Some(c) => format!("{version} ({c})"),
        None => version.to_string(),
    }
}

/// The version with its commit, as `--version`, the log and the status show
/// it.
pub fn version_line() -> &'static str {
    static LINE: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| format_version(VERSION, COMMIT));
    &LINE
}

#[cfg(test)]
mod version_tests {
    use super::format_version;

    #[test]
    fn the_version_names_its_commit() {
        assert_eq!(format_version("1.0.2", Some("c463aa1")), "1.0.2 (c463aa1)");
        assert_eq!(format_version("1.0.2", None), "1.0.2");
    }
}
