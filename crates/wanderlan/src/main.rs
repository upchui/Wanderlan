//! `wanderlan`, terminal UI and command line of wanderlan.

mod cli;
mod client;
mod tui;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args = cli::Args::parse();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("wanderlan: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(cli::run(args)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wanderlan: {e:#}");
            ExitCode::FAILURE
        }
    }
}
