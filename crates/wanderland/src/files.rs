//! Configuration files on disk.
//!
//! `/etc/wanderlan/{mesh,local}.toml` always hold the confirmed
//! configuration, except while a change made by editing the files is
//! pending. A copy of the last confirmed files is kept in the state
//! directory, together with a marker for such pending file changes, so that
//! a restart during a pending change falls back to the confirmed state.

use std::path::{Path, PathBuf};

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
fn write_atomic(path: &Path, content: &str) -> Result<()> {
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
}
