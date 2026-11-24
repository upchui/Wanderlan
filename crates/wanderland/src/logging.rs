//! Logging to journald (under systemd) or stderr.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Sets up logging. Uses journald when started by systemd (JOURNAL_STREAM is
/// set) and falls back to stderr otherwise. `RUST_LOG` overrides the level.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    if std::env::var_os("JOURNAL_STREAM").is_some()
        && let Ok(journald) = tracing_journald::layer()
    {
        registry
            .with(journald.with_syslog_identifier("wanderland".into()))
            .init();
        return;
    }
    registry
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();
}
