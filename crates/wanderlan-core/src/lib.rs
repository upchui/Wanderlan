//! Core library of wanderlan.
//!
//! This crate contains everything that does not need a running kernel:
//! loading and validating the configuration, computing the tunnel topology
//! and deriving the desired system state. System access is abstracted behind
//! traits so that all logic can be tested without root privileges.

pub mod api;
pub mod config;
pub mod desired;
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
/// Builds from CI set `WANDERLAN_VERSION` to the package version, e.g.
/// `0.1.0+57.gc256ea5` for a commit on main; otherwise it is the version
/// from `Cargo.toml`.
pub const VERSION: &str = match option_env!("WANDERLAN_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};
