//! Non-interactive subcommands for scripts.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use wanderlan_core::config::{self, ConfigFile, Severity};

const EXAMPLE_MESH: &str = include_str!("../../../packaging/examples/mesh.toml");
const EXAMPLE_LOCAL: &str = include_str!("../../../packaging/examples/local.toml");

/// Command line of `wanderlan`.
#[derive(Debug, Parser)]
#[command(name = "wanderlan", version = wanderlan_core::VERSION, about)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check a mesh.toml (and optionally a local.toml) for errors.
    Validate {
        /// The mesh.toml to check.
        file: PathBuf,
        /// A local.toml to check together with the mesh.
        #[arg(long)]
        local: Option<PathBuf>,
    },
    /// Write an example mesh.toml and local.toml.
    Init {
        /// Directory to write the files to; prints them if omitted.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
    },
}

/// Runs the selected subcommand.
pub fn run(args: Args) -> Result<ExitCode> {
    match args.command {
        None => bail!("the interactive interface is not available yet; see `wanderlan --help`"),
        Some(Command::Validate { file, local }) => validate(&file, local.as_deref()),
        Some(Command::Init { dir, force }) => init(dir.as_deref(), force),
    }
}

fn validate(file: &Path, local: Option<&Path>) -> Result<ExitCode> {
    let mesh_text =
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    let local_text = local
        .map(|p| std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display())))
        .transpose()?;
    let diags = config::check(&mesh_text, local_text.as_deref());
    for d in &diags {
        let label = match (d.file, local) {
            (ConfigFile::Local, Some(p)) => p.display().to_string(),
            _ => file.display().to_string(),
        };
        eprintln!("{}", d.display_with(&label));
    }
    let errors = diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    if errors == 0 {
        println!("{}: configuration is valid", file.display());
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("{errors} error(s) found");
        Ok(ExitCode::FAILURE)
    }
}

fn init(dir: Option<&Path>, force: bool) -> Result<ExitCode> {
    let Some(dir) = dir else {
        println!("# ---- mesh.toml ----\n{EXAMPLE_MESH}\n# ---- local.toml ----\n{EXAMPLE_LOCAL}");
        return Ok(ExitCode::SUCCESS);
    };
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    for (name, content) in [("mesh.toml", EXAMPLE_MESH), ("local.toml", EXAMPLE_LOCAL)] {
        let path = dir.join(name);
        if path.exists() && !force {
            bail!(
                "{} already exists (use --force to overwrite)",
                path.display()
            );
        }
        std::fs::write(&path, content)
            .with_context(|| format!("cannot write {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}
