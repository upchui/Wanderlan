//! What systemd knows about the daemon, for the "daemon not reachable"
//! screen: the state of the service and its last log lines.

use std::process::Command;

/// Name of the systemd service.
pub const SERVICE: &str = "wanderland";

/// State and recent log of the service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnosis {
    /// `systemctl is-active` (e.g. "active", "failed", "activating"); `None`
    /// without systemd.
    pub state: Option<String>,
    /// Last lines of the journal (empty if not readable).
    pub journal: Vec<String>,
}

fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Asks systemd about the daemon; blocking (runs two short commands).
pub fn diagnose(lines: usize) -> Diagnosis {
    let state = output("systemctl", &["is-active", SERVICE]).filter(|s| !s.is_empty());
    let count = lines.to_string();
    let journal = output(
        "journalctl",
        &["-u", SERVICE, "-n", &count, "-o", "cat", "--no-pager", "-q"],
    )
    .map(|text| text.lines().map(str::to_string).collect())
    .unwrap_or_default();
    Diagnosis { state, journal }
}

impl Diagnosis {
    /// One sentence about the service state.
    pub fn summary(&self) -> Option<String> {
        let state = self.state.as_deref()?;
        Some(match state {
            "active" => format!(
                "The service {SERVICE} is running, but its socket is missing; it creates it again within seconds (or: systemctl restart {SERVICE})."
            ),
            "activating" | "reloading" => format!("The service {SERVICE} is starting."),
            "failed" => format!(
                "The service {SERVICE} failed; see the log below or: systemctl status {SERVICE}"
            ),
            "inactive" => format!(
                "The service {SERVICE} is stopped; start it with: systemctl start {SERVICE}"
            ),
            other => format!("The service {SERVICE} is {other}."),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_says_what_to_do() {
        let d = |s: &str| Diagnosis {
            state: Some(s.into()),
            journal: vec![],
        };
        assert!(d("failed").summary().unwrap().contains("systemctl status"));
        assert!(d("inactive").summary().unwrap().contains("systemctl start"));
        assert!(d("active").summary().unwrap().contains("socket"));
        assert_eq!(Diagnosis::default().summary(), None);
    }
}
