//! Interactive terminal UI (placeholder until milestone 7).

use std::path::Path;

use anyhow::Result;

/// Options of the TUI.
#[derive(Debug, Clone)]
pub struct TuiOptions {
    /// Enable mouse support.
    pub mouse: bool,
}

/// Runs the TUI until the user quits.
pub async fn run(_socket: &Path, options: TuiOptions) -> Result<()> {
    let _ = options.mouse;
    anyhow::bail!("the interactive interface is not available yet; see `wanderlan --help`")
}
