//! Configuration files on disk.
//!
//! `/etc/wanderlan/{mesh,local}.toml` always hold the confirmed
//! configuration, except while a change made by editing the files is
//! pending. A copy of the last confirmed files is kept in the state
//! directory, together with a marker for such pending file changes, so that
//! a restart during a pending change falls back to the confirmed state.

use std::path::{Path, PathBuf};

/// The `mesh.toml` shipped by the package (written back on reset, so that
/// dpkg sees the conffile unchanged).
const TEMPLATE_MESH: &str = include_str!("../../../packaging/etc/mesh.toml");
/// The `local.toml` shipped by the package.
const TEMPLATE_LOCAL: &str = include_str!("../../../packaging/etc/local.toml");

use anyhow::{Context as _, Result};
use wanderlan_core::config::{Config, render_local, render_mesh};

/// File locations.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `/etc/wanderlan`.
    pub config_dir: PathBuf,
    /// `/var/lib/wanderlan`.
    pub state_dir: PathBuf,
}

impl Paths {
    /// `mesh.toml` in the configuration directory.
    pub fn mesh(&self) -> PathBuf {
        self.config_dir.join("mesh.toml")
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

    /// Reads both files; missing files are empty.
    pub fn read(&self) -> Result<(String, String)> {
        Ok((read_optional(&self.mesh())?, read_optional(&self.local())?))
    }

    /// Writes a confirmed configuration to the configuration directory and
    /// keeps a copy in the state directory.
    pub fn write_confirmed(&self, cfg: &Config) -> Result<()> {
        let mesh = render_mesh(&cfg.mesh);
        let local = render_local(&cfg.local);
        write_atomic(&self.mesh(), &mesh)?;
        write_atomic(&self.local(), &local)?;
        self.save_copy()
    }

    /// Copies the current files into the state directory as the confirmed
    /// version (used at startup and after confirming).
    pub fn save_copy(&self) -> Result<()> {
        let dir = self.confirmed_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let (mesh, local) = self.read()?;
        write_atomic(&dir.join("mesh.toml"), &mesh)?;
        write_atomic(&dir.join("local.toml"), &local)
    }

    /// Restores the confirmed files after a rejected file change. The
    /// rejected files are kept as `*.rejected`.
    pub fn restore_confirmed(&self) -> Result<bool> {
        let dir = self.confirmed_dir();
        if !dir.join("mesh.toml").exists() {
            return Ok(false);
        }
        for name in ["mesh.toml", "local.toml"] {
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
        attempt(write_atomic(&self.mesh(), TEMPLATE_MESH));
        attempt(write_atomic(&self.local(), TEMPLATE_LOCAL));
        for name in ["mesh.toml.rejected", "local.toml.rejected"] {
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
        for (path, template) in [(self.mesh(), TEMPLATE_MESH), (self.local(), TEMPLATE_LOCAL)] {
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

    #[test]
    fn restore_keeps_rejected_files() {
        let base = std::env::temp_dir().join(format!("wl-files-{}", std::process::id()));
        let paths = Paths {
            config_dir: base.join("etc"),
            state_dir: base.join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.mesh(), "good").unwrap();
        std::fs::write(paths.local(), "local").unwrap();
        paths.save_copy().unwrap();
        std::fs::write(paths.mesh(), "bad").unwrap();
        paths.set_marker().unwrap();
        assert!(paths.marker_exists());
        assert!(paths.restore_confirmed().unwrap());
        assert_eq!(std::fs::read_to_string(paths.mesh()).unwrap(), "good");
        assert_eq!(
            std::fs::read_to_string(paths.config_dir.join("mesh.toml.rejected")).unwrap(),
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
        std::fs::write(paths.mesh(), "configured").unwrap();
        std::fs::write(paths.local(), "self = \"a\"").unwrap();
        std::fs::write(paths.config_dir.join("mesh.toml.rejected"), "x").unwrap();
        paths.save_copy().unwrap();
        paths.set_marker().unwrap();
        std::fs::write(paths.invites(), "[]").unwrap();
        paths.reset().unwrap();
        assert_eq!(
            std::fs::read_to_string(paths.mesh()).unwrap(),
            TEMPLATE_MESH
        );
        assert_eq!(
            std::fs::read_to_string(paths.local()).unwrap(),
            TEMPLATE_LOCAL
        );
        assert!(!paths.config_dir.join("mesh.toml.rejected").exists());
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
        std::fs::write(paths.mesh(), "configured").unwrap();
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
