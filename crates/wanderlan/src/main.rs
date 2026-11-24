//! `wanderlan`, terminal UI and command line of wanderlan.

mod cli;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args = cli::Args::parse();
    match cli::run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wanderlan: {e:#}");
            ExitCode::FAILURE
        }
    }
}
