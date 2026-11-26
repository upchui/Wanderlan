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
use wanderlan_core::identity::Identity;

/// The directory of the other networks of the box, in the configuration and
/// in the state directory (`networks/<name>`).
pub const NETWORKS_DIR: &str = "networks";
/// The file with the tag of a network (in its state directory).
const TAG_FILE: &str = "tag";
/// What a reset of the default network keeps in its state directory: it
/// belongs to the box (the other networks, the sysctls a breakout changed).
const BOX_STATE: &[&str] = &[NETWORKS_DIR, "breakout-sysctl"];
/// What a reset of another network keeps: its tag.
const NETWORK_STATE: &[&str] = &[TAG_FILE];
/// The form of `network.toml` the daemon writes: the sites by identity.
const CURRENT_FORMAT: u8 = 3;
/// The tags the other networks can get (`x` is the default network's).
const TAGS: &str = "abcdefghijklmnopqrstuvwyz0123456789";

/// File locations of one network.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `/etc/wanderlan` (`/etc/wanderlan/networks/<name>`).
    pub config_dir: PathBuf,
    /// `/var/lib/wanderlan` (`/var/lib/wanderlan/networks/<name>`).
    pub state_dir: PathBuf,
    /// What a reset keeps in the state directory.
    keep: &'static [&'static str],
}

/// Where the networks of the box are.
#[derive(Debug, Clone)]
pub struct Layout {
    /// `/etc/wanderlan`.
    pub config_dir: PathBuf,
    /// `/var/lib/wanderlan`.
    pub state_dir: PathBuf,
}

impl Layout {
    /// The files of network `name`.
    pub fn paths(&self, name: &str) -> Paths {
        if name == wanderlan_core::api::DEFAULT_NETWORK {
            return Paths::new(self.config_dir.clone(), self.state_dir.clone());
        }
        Paths {
            config_dir: self.config_dir.join(NETWORKS_DIR).join(name),
            state_dir: self.state_dir.join(NETWORKS_DIR).join(name),
            keep: NETWORK_STATE,
        }
    }

    /// The networks of the box with their tags: the default network first,
    /// then the others in the order they were added.
    pub fn scan(&self) -> Vec<(String, char)> {
        let mut found: Vec<(std::time::SystemTime, String, char)> = Vec::new();
        let dir = self.state_dir.join(NETWORKS_DIR);
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if wanderlan_core::api::network_name_error(&name).is_some() {
                continue;
            }
            let file = entry.path().join(TAG_FILE);
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Some(tag) = text.trim().chars().next().filter(|c| TAGS.contains(*c)) else {
                continue;
            };
            // The tag file is never written again: its time is when the
            // network was added.
            let added = std::fs::metadata(&file)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if found.iter().all(|(_, _, t)| *t != tag) {
                found.push((added, name, tag));
            }
        }
        found.sort();
        let mut out = vec![(
            wanderlan_core::api::DEFAULT_NETWORK.to_string(),
            wanderlan_core::topology::DEFAULT_TAG,
        )];
        out.extend(found.into_iter().map(|(_, name, tag)| (name, tag)));
        out
    }

    /// Adds network `name` (unconfigured) and gives it a tag.
    pub fn add(&self, name: &str) -> Result<char> {
        if let Some(e) = wanderlan_core::api::network_name_error(name) {
            anyhow::bail!("{e}");
        }
        let networks = self.scan();
        if networks.iter().any(|(n, _)| n == name) {
            anyhow::bail!("this box already has a network \"{name}\"");
        }
        let Some(tag) = TAGS.chars().find(|c| networks.iter().all(|(_, t)| t != c)) else {
            anyhow::bail!("this box has as many networks as it can have");
        };
        let paths = self.paths(name);
        for dir in [&paths.config_dir, &paths.state_dir] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        write_atomic(&paths.network(), TEMPLATE_NETWORK)?;
        write_atomic(&paths.local(), TEMPLATE_LOCAL)?;
        write_atomic(&paths.state_dir.join(TAG_FILE), &format!("{tag}\n"))?;
        Ok(tag)
    }

    /// Removes network `name` and its files.
    pub fn remove(&self, name: &str) -> Result<()> {
        if name == wanderlan_core::api::DEFAULT_NETWORK {
            anyhow::bail!("the default network cannot be removed (`wanderlan reset` empties it)");
        }
        let paths = self.paths(name);
        for dir in [&paths.config_dir, &paths.state_dir] {
            match std::fs::remove_dir_all(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot remove {}", dir.display()));
                }
            }
        }
        Ok(())
    }
}

impl Paths {
    /// The files of the default network.
    pub fn new(config_dir: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            config_dir,
            state_dir,
            keep: BOX_STATE,
        }
    }

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
    /// (not just the package template or comments) is kept. Then a
    /// `network.toml` in format 1 (`[[site]]`, `[[map]]`) is written in
    /// format 2; the old file stays as `network.toml.v1` for an older
    /// version. Returns a line per moved or converted file.
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
        for (dir, backup) in [
            (self.config_dir.clone(), true),
            (self.confirmed_dir(), false),
        ] {
            let path = dir.join("network.toml");
            let text = read_optional(&path)?;
            let Ok(parsed) = wanderlan_core::config::parse_network(&text) else {
                continue;
            };
            let Some(network) = parsed.network else {
                continue;
            };
            // Written with names (by hand, or by an older version): from now
            // on with the identities (an empty network has no sites to name).
            if parsed.format != CURRENT_FORMAT && !network.sites.is_empty() {
                if backup {
                    let copy = self.old_format_copy(parsed.format);
                    write_atomic(&copy, &text)?;
                    moved.push(format!(
                        "{} was converted to format {CURRENT_FORMAT}; the old file is kept as {}",
                        path.display(),
                        copy.display()
                    ));
                }
                write_atomic(&path, &render_network(&network))?;
            }
            // `local.toml` names the sites by their identity.
            let local_path = dir.join("local.toml");
            let local_text = read_optional(&local_path)?;
            if wanderlan_core::config::local_uses_names(&local_text)
                && let Ok(local) = wanderlan_core::config::parse_local(&local_text, Some(&network))
            {
                write_atomic(&local_path, &render_local(&local.local, Some(&network)))?;
            }
        }
        Ok(moved)
    }

    /// `network.toml` in an older format as it was before the conversion.
    fn old_format_copy(&self, format: u8) -> PathBuf {
        self.config_dir.join(format!("network.toml.v{format}"))
    }

    /// The network of the confirmed copy in the state directory, if it has
    /// one: what was in effect last (the files may have been edited since).
    pub fn confirmed_network(&self) -> Option<wanderlan_core::config::Network> {
        let text = read_optional(&self.confirmed_dir().join("network.toml")).ok()?;
        wanderlan_core::config::parse_network(&text).ok()?.network
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
        let local = render_local(&cfg.local, Some(&cfg.network));
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
            // Nothing confirmed yet (a first configuration from files): the
            // rejected files are set aside, the site is unconfigured again.
            let mut moved = false;
            for name in ["network.toml", "local.toml"] {
                let current = self.config_dir.join(name);
                if current.exists() {
                    let aside = self.config_dir.join(format!("{name}.rejected"));
                    std::fs::rename(&current, &aside).with_context(|| {
                        format!("cannot move {} to {}", current.display(), aside.display())
                    })?;
                    moved = true;
                }
            }
            self.clear_marker();
            return Ok(moved);
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

    /// The private key of this box (its identity).
    pub fn identity(&self) -> PathBuf {
        self.state_dir.join("identity")
    }

    /// Reads the identity of this box, if it has one.
    pub fn load_identity(&self) -> Result<Option<Identity>> {
        let path = self.identity();
        let text = read_optional(&path)?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        Identity::from_secret_text(&text)
            .map(Some)
            .with_context(|| format!("{} does not hold a wanderlan identity", path.display()))
    }

    /// Creates a new identity for this box and stores it (readable by root
    /// only).
    pub fn create_identity(&self) -> Result<Identity> {
        let secret = crate::random::bytes::<{ wanderlan_core::identity::KEY_LEN }>()
            .context("cannot create the identity of this box")?;
        let identity = Identity::from_secret(secret);
        write_private(&self.identity(), &identity.secret_text())?;
        Ok(identity)
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
        for name in [
            "network.toml.rejected",
            "local.toml.rejected",
            "network.toml.v0",
            "network.toml.v1",
            "network.toml.v2",
        ] {
            attempt(remove_optional(&self.config_dir.join(name)));
        }
        match std::fs::read_dir(&self.state_dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if self.keep.iter().any(|k| entry.file_name() == *k) {
                        continue;
                    }
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
                .filter(|n| !self.keep.contains(&n.as_str()))
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
    write_file(path, content, None)
}

/// [`write_atomic`] for a secret: only its owner may read it (mode 0600,
/// whatever the umask).
pub fn write_private(path: &Path, content: &str) -> Result<()> {
    write_file(path, content, Some(0o600))
}

fn write_file(path: &Path, content: &str, mode: Option<u32>) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp"));
    // On the disk before the rename, and the rename itself too: after a
    // power loss the old or the new file, never an empty one.
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let _ = std::fs::remove_file(&tmp);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        if let Some(mode) = mode {
            options.mode(mode);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        if let Some(mode) = mode {
            file.set_permissions(std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("cannot protect {}", tmp.display()))?;
        }
        file.write_all(content.as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("cannot write {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(name: &str) -> (PathBuf, Paths) {
        let base = std::env::temp_dir().join(format!("wl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = Paths::new(base.join("etc"), base.join("state"));
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        (base, paths)
    }

    const OLD: &str = "# comment\n[mesh]\nrevision = 3\nvlans = \"10-50\"\n\n[[site]]\nname = \"a\"\naddress = \"10.0.0.1\"\n";

    /// The same network as the daemon writes it.
    fn as_written(text: &str) -> String {
        let network = wanderlan_core::config::parse_network(text)
            .unwrap()
            .network
            .unwrap();
        render_network(&network)
    }

    #[test]
    fn a_network_toml_in_format_1_is_converted() {
        let (base, paths) = temp_paths("migrate-format");
        let old = "[network]\nrevision = 7\nvlans = \"10-50\"\n\n[[site]]\nname = \"hq\"\naddress = \"10.0.0.1\"\n\n[[site]]\nname = \"lab\"\naddress = \"10.0.0.2\"\nuplink = \"hq\"\n\n[[map]]\nsite = \"lab\"\nlocal = 130\nshared = 30\n";
        std::fs::write(paths.network(), old).unwrap();
        std::fs::write(paths.local(), "self = \"lab\"\ntrunk = \"ens19\"\n").unwrap();
        paths.save_copy().unwrap();
        let moved = paths.migrate().unwrap();
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert!(moved[0].contains("network.toml.v1"), "{moved:?}");
        let new = std::fs::read_to_string(paths.network()).unwrap();
        // With the identities (placeholders: no box has joined yet).
        let hq = wanderlan_core::identity::SiteId::provisional("hq");
        assert!(
            new.contains(&format!(
                "name = \"lab\"\naddress = \"10.0.0.2\"\nserver = \"{hq}\"  # hq\nmap = {{ 30 = 130 }}\n"
            )),
            "{new}"
        );
        assert!(!new.contains("[site.lab]"), "{new}");
        assert!(new.contains("revision = 7"), "{new}");
        assert_eq!(
            std::fs::read_to_string(paths.old_format_copy(1)).unwrap(),
            old
        );
        // The same network, the confirmed copy too.
        let parse = |t: &str| wanderlan_core::config::parse_network(t).unwrap().network;
        assert_eq!(parse(&new), parse(old));
        assert_eq!(
            std::fs::read_to_string(paths.confirmed_dir().join("network.toml")).unwrap(),
            new
        );
        // A second start changes nothing.
        assert!(paths.migrate().unwrap().is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }

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
        // Three files moved, then network.toml written with the identities.
        assert_eq!(moved.len(), 4, "{moved:?}");
        let new = std::fs::read_to_string(paths.network()).unwrap();
        assert_eq!(new, as_written(&OLD.replace("[mesh]", "[network]")));
        assert_eq!(
            std::fs::read_to_string(paths.old_format_copy(1)).unwrap(),
            OLD.replace("[mesh]", "[network]")
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
        assert_eq!(paths.migrate().unwrap().len(), 2);
        assert_eq!(
            std::fs::read_to_string(paths.network()).unwrap(),
            as_written(&OLD.replace("[mesh]", "[network]"))
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
        let paths = Paths::new(base.join("etc"), base.join("state"));
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
    fn the_identity_is_kept_private_and_read_back() {
        use std::os::unix::fs::PermissionsExt as _;
        let (base, paths) = temp_paths("identity");
        assert!(paths.load_identity().unwrap().is_none(), "none yet");
        let created = paths.create_identity().unwrap();
        let mode = std::fs::metadata(paths.identity())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let loaded = paths.load_identity().unwrap().unwrap();
        assert_eq!(loaded.id(), created.id());
        std::fs::write(paths.identity(), "garbage").unwrap();
        assert!(paths.load_identity().is_err());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn reset_restores_the_templates_and_clears_the_state() {
        let base = std::env::temp_dir().join(format!("wl-reset-{}", std::process::id()));
        let paths = Paths::new(base.join("etc"), base.join("state"));
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.network(), "configured").unwrap();
        std::fs::write(paths.local(), "self = \"a\"").unwrap();
        std::fs::write(paths.config_dir.join("network.toml.rejected"), "x").unwrap();
        std::fs::write(paths.old_format_copy(1), "[[site]]").unwrap();
        paths.save_copy().unwrap();
        paths.set_marker().unwrap();
        std::fs::write(paths.invites(), "[]").unwrap();
        paths.create_identity().unwrap();
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
        assert!(!paths.old_format_copy(1).exists());
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
        let paths = Paths::new(base.join("etc"), base.join("lib").join("wanderlan"));
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
