//! Core library of wanderlan.
//!
//! This crate contains everything that does not need a running kernel:
//! loading and validating the configuration, computing the tunnel topology
//! and deriving the desired system state. System access is abstracted behind
//! traits so that all logic can be tested without root privileges.

pub mod api;
pub mod config;
pub mod desired;
pub mod mtu;
pub mod reconcile;
pub mod sys;
pub mod topology;

/// Name of the bridge created by wanderlan.
pub const BRIDGE_NAME: &str = "br0";

/// Version of the wanderlan workspace, shared by all binaries.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
