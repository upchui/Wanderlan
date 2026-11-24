//! Configuration files on disk.
//!
//! `/etc/wanderlan/{network,local}.toml` always hold the confirmed
//! configuration, except while a change made by editing the files is
//! pending. A copy of the last confirmed files is kept in the state
//! directory, together with a marker for such pending file changes, so that
//! a restart during a pending change falls back to the confirmed state.

use std::path::{Path, PathBuf};

/// The `network.toml` shipped by the package (written back on reset, so that
/// dpkg sees the conffile unchanged).
const TEMPLATE_NETWORK: &str = include_str!("../../../packaging/etc/network.toml");
/// The `local.toml` shipped by the package.
const TEMPLATE_LOCAL: &str = include_str!("../../../packaging/etc/local.toml");
/// Name of `network.toml` (and of its `[network]` table) in older versions.
const OLD_NAME: &str = "mesh";

use anyhow::{Context as _, Result};
use wanderlan_core::config::{Config, render_local, render_network};

/// File locations.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `/etc/wanderlan`.
    pub config_dir: PathBuf,
    /// `/var/lib/wanderlan`.
    pub state_dir: PathBuf,
}

impl Paths {
    /// `network.toml` in the configuration directory.
    pub fn network(&self) -> PathBuf {
        self.config_dir.join("network.toml")
    }

    /// `local.toml` in the configuration directory.
    pub fn local(&self) -> PathBuf {
        self.config_dir.join("local.toml")
    }

    fn confirmed_dir(&self) -> PathBuf {
        self.state_dir.join("confirmed")
    }

    fn marker(&self) -> PathBuf {
        self.state_dir.join("pending-files")
    }

    /// Moves the files of older versions (`mesh.toml` with a `[mesh]`
    /// table) to `network.toml`, in the configuration directory and in the
    /// confirmed copy. A `network.toml` with a configuration of its own
    /// (not just the package template or comments) is kept. Returns a line
    /// per moved file.
    pub fn migrate(&self) -> Result<Vec<String>> {
        let mut moved = Vec::new();
        for dir in [self.config_dir.clone(), self.confirmed_dir()] {
            for suffix in ["", ".rejected"] {
                let old = dir.join(format!("{OLD_NAME}.toml{suffix}"));
                if !old.exists() {
                    continue;
                }
                let new = dir.join(format!("network.toml{suffix}"));
                let text = read_optional(&old)?;
                let current = read_optional(&new)?;
                if !carries_configuration(&current) && carries_configuration(&text) {
                    write_atomic(&new, &rename_table(&text))?;
                    moved.push(format!("Moved {} to {}", old.display(), new.display()));
                }
                remove_optional(&old)?;
            }
        }
        Ok(moved)
    }

    /// Reads both files; missing files are empty.
    pub fn read(&self) -> Result<(String, String)> {
        Ok((
            read_optional(&self.network())?,
            read_optional(&self.local())?,
        ))
    }

    /// Writes a confirmed configuration to the configuration directory and
    /// keeps a copy in the state directory.
    pub fn write_confirmed(&self, cfg: &Config) -> Result<()> {
        let network = render_network(&cfg.network);
        let local = render_local(&cfg.local);
        write_atomic(&self.network(), &network)?;
        write_atomic(&self.local(), &local)?;
        self.save_copy()
    }

    /// Copies the current files into the state directory as the confirmed
    /// version (used at startup and after confirming).
    pub fn save_copy(&self) -> Result<()> {
        let dir = self.confirmed_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let (network, local) = self.read()?;
        write_atomic(&dir.join("network.toml"), &network)?;
        write_atomic(&dir.join("local.toml"), &local)
    }

    /// Restores the confirmed files after a rejected file change. The
    /// rejected files are kept as `*.rejected`.
    pub fn restore_confirmed(&self) -> Result<bool> {
        let dir = self.confirmed_dir();
        if !dir.join("network.toml").exists() {
            return Ok(false);
        }
        for name in ["network.toml", "local.toml"] {
            let current = self.config_dir.join(name);
            let saved = read_optional(&dir.join(name))?;
            let now = read_optional(&current)?;
            if now != saved {
                write_atomic(&self.config_dir.join(format!("{name}.rejected")), &now)?;
                write_atomic(&current, &saved)?;
            }
        }
        self.clear_marker();
        Ok(true)
    }

    /// Marks that the files contain an unconfirmed change.
    pub fn set_marker(&self) -> Result<()> {
        std::fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("cannot create {}", self.state_dir.display()))?;
        std::fs::write(self.marker(), b"pending\n")
            .with_context(|| format!("cannot write {}", self.marker().display()))
    }

    /// Removes the marker.
    pub fn clear_marker(&self) {
        let _ = std::fs::remove_file(self.marker());
    }

    /// True if the files contain an unconfirmed change.
    pub fn marker_exists(&self) -> bool {
        self.marker().exists()
    }

    /// Open invitations of this site.
    pub fn invites(&self) -> PathBuf {
        self.state_dir.join("invites.json")
    }

    /// Back to the state after installation: the configuration files become
    /// the package templates again; the contents of the state directory
    /// (confirmed copies, marker, invitations) and rejected files are
    /// removed. The state directory itself stays: under systemd it is the
    /// unit's `StateDirectory` and its parent (`/var/lib`) is read-only.
    pub fn reset(&self) -> Result<()> {
        let mut errors: Vec<String> = Vec::new();
        let mut attempt = |result: Result<()>| {
            if let Err(e) = result {
                errors.push(format!("{e:#}"));
            }
        };
        attempt(write_atomic(&self.network(), TEMPLATE_NETWORK));
        attempt(write_atomic(&self.local(), TEMPLATE_LOCAL));
        for name in ["network.toml.rejected", "local.toml.rejected"] {
            attempt(remove_optional(&self.config_dir.join(name)));
        }
        match std::fs::read_dir(&self.state_dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let removed = if path.is_dir() && !path.is_symlink() {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                    attempt(removed.with_context(|| format!("cannot remove {}", path.display())));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                attempt(Err(e).with_context(|| format!("cannot read {}", self.state_dir.display())))
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("{}", errors.join("; "))
        }
    }

    /// What a reset should have removed but is still there.
    pub fn leftovers(&self) -> Vec<String> {
        let mut left = Vec::new();
        for (path, template) in [
            (self.network(), TEMPLATE_NETWORK),
            (self.local(), TEMPLATE_LOCAL),
        ] {
            if std::fs::read_to_string(&path).ok().as_deref() != Some(template) {
                left.push(format!("{} is not the package template", path.display()));
            }
        }
        if let Ok(entries) = std::fs::read_dir(&self.state_dir) {
            let names: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            if !names.is_empty() {
                left.push(format!(
                    "{} still contains {}",
                    self.state_dir.display(),
                    names.join(", ")
                ));
            }
        }
        left
    }
}

/// False for the unconfigured package template of older versions (it is
/// replaced by the current one).
fn carries_configuration(text: &str) -> bool {
    match wanderlan_core::config::parse_network(text) {
        Ok(parsed) => parsed.network.is_some(),
        // Not valid, but written by someone: keep it for them to fix.
        Err(_) => !text.trim().is_empty(),
    }
}

/// `[mesh]` becomes `[network]`; everything else stays as it is.
fn rename_table(text: &str) -> String {
    let old = format!("[{OLD_NAME}]");
    let mut out: String = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with(&old) {
                line.replacen(&old, "[network]", 1)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn remove_optional(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot remove {}", path.display())),
    }
}

/// Reads a file, treating a missing file as empty.
pub fn read_optional(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Writes a file via a temporary file and rename.
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, content).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(name: &str) -> (PathBuf, Paths) {
        let base = std::env::temp_dir().join(format!("wl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = Paths {
            config_dir: base.join("etc"),
            state_dir: base.join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        (base, paths)
    }

    const OLD: &str = "# comment\n[mesh]\nrevision = 3\nvlans = \"10-50\"\n\n[[site]]\nname = \"a\"\naddress = \"10.0.0.1\"\n";

    #[test]
    fn the_files_of_older_versions_are_moved() {
        let (base, paths) = temp_paths("migrate");
        // The package installed the new template next to the old file.
        std::fs::write(paths.network(), TEMPLATE_NETWORK).unwrap();
        std::fs::write(paths.config_dir.join("mesh.toml"), OLD).unwrap();
        std::fs::write(paths.config_dir.join("mesh.toml.rejected"), OLD).unwrap();
        std::fs::create_dir_all(paths.confirmed_dir()).unwrap();
        std::fs::write(paths.confirmed_dir().join("mesh.toml"), OLD).unwrap();
        let moved = paths.migrate().unwrap();
        assert_eq!(moved.len(), 3, "{moved:?}");
        let new = std::fs::read_to_string(paths.network()).unwrap();
        assert_eq!(new, OLD.replace("[mesh]", "[network]"));
        assert!(
            wanderlan_core::config::parse_network(&new)
                .unwrap()
                .network
                .is_some()
        );
        for dir in [&paths.config_dir, &paths.confirmed_dir()] {
            assert!(!dir.join("mesh.toml").exists());
        }
        assert!(paths.confirmed_dir().join("network.toml").exists());
        assert!(paths.config_dir.join("network.toml.rejected").exists());
        // A second start changes nothing.
        assert!(paths.migrate().unwrap().is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_network_toml_with_comments_only_is_replaced() {
        let (base, paths) = temp_paths("migrate-comments");
        std::fs::write(paths.network(), "# not configured\n").unwrap();
        std::fs::write(paths.config_dir.join("mesh.toml"), OLD).unwrap();
        assert_eq!(paths.migrate().unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(paths.network()).unwrap(),
            OLD.replace("[mesh]", "[network]")
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_network_toml_with_a_configuration_is_kept() {
        let (base, paths) = temp_paths("migrate-keep");
        std::fs::write(paths.network(), "[network]\nvlans = \"20\"\n").unwrap();
        std::fs::write(paths.config_dir.join("mesh.toml"), OLD).unwrap();
        assert!(paths.migrate().unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(paths.network()).unwrap(),
            "[network]\nvlans = \"20\"\n"
        );
        assert!(!paths.config_dir.join("mesh.toml").exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn an_unconfigured_old_template_is_not_taken_over() {
        let (base, paths) = temp_paths("migrate-template");
        std::fs::write(paths.network(), TEMPLATE_NETWORK).unwrap();
        std::fs::write(
            paths.config_dir.join("mesh.toml"),
            "# This site is not configured yet.\n",
        )
        .unwrap();
        assert!(paths.migrate().unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(paths.network()).unwrap(),
            TEMPLATE_NETWORK
        );
        assert!(!paths.config_dir.join("mesh.toml").exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn restore_keeps_rejected_files() {
        let base = std::env::temp_dir().join(format!("wl-files-{}", std::process::id()));
        let paths = Paths {
            config_dir: base.join("etc"),
            state_dir: base.join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.network(), "good").unwrap();
        std::fs::write(paths.local(), "local").unwrap();
        paths.save_copy().unwrap();
        std::fs::write(paths.network(), "bad").unwrap();
        paths.set_marker().unwrap();
        assert!(paths.marker_exists());
        assert!(paths.restore_confirmed().unwrap());
        assert_eq!(std::fs::read_to_string(paths.network()).unwrap(), "good");
        assert_eq!(
            std::fs::read_to_string(paths.config_dir.join("network.toml.rejected")).unwrap(),
            "bad"
        );
        assert!(!paths.marker_exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn reset_restores_the_templates_and_clears_the_state() {
        let base = std::env::temp_dir().join(format!("wl-reset-{}", std::process::id()));
        let paths = Paths {
            config_dir: base.join("etc"),
            state_dir: base.join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.network(), "configured").unwrap();
        std::fs::write(paths.local(), "self = \"a\"").unwrap();
        std::fs::write(paths.config_dir.join("network.toml.rejected"), "x").unwrap();
        paths.save_copy().unwrap();
        paths.set_marker().unwrap();
        std::fs::write(paths.invites(), "[]").unwrap();
        paths.reset().unwrap();
        assert_eq!(
            std::fs::read_to_string(paths.network()).unwrap(),
            TEMPLATE_NETWORK
        );
        assert_eq!(
            std::fs::read_to_string(paths.local()).unwrap(),
            TEMPLATE_LOCAL
        );
        assert!(!paths.config_dir.join("network.toml.rejected").exists());
        assert!(paths.state_dir.exists(), "the state directory itself stays");
        assert_eq!(std::fs::read_dir(&paths.state_dir).unwrap().count(), 0);
        assert_eq!(paths.leftovers(), Vec::<String>::new());
        paths.reset().unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn reset_works_when_the_parent_of_the_state_directory_is_read_only() {
        use std::os::unix::fs::PermissionsExt as _;
        // Like systemd's StateDirectory under ProtectSystem=strict: the state
        // directory is writable, /var/lib above it is not.
        let base = std::env::temp_dir().join(format!("wl-reset-ro-{}", std::process::id()));
        let paths = Paths {
            config_dir: base.join("etc"),
            state_dir: base.join("lib").join("wanderlan"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.network(), "configured").unwrap();
        paths.save_copy().unwrap();
        std::fs::write(paths.invites(), "[]").unwrap();
        let parent = base.join("lib");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = paths.reset();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        result.unwrap();
        assert!(paths.state_dir.exists());
        assert_eq!(std::fs::read_dir(&paths.state_dir).unwrap().count(), 0);
        std::fs::remove_dir_all(base).unwrap();
    }
}
