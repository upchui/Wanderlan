//! Unified diff of the canonical configuration texts.

use similar::TextDiff;

/// Kind of a diff line, for colouring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// `---`/`+++` file header.
    Header,
    /// `@@ ... @@` hunk header.
    Hunk,
    /// Added line.
    Added,
    /// Removed line.
    Removed,
    /// Unchanged context line.
    Context,
    /// Remark such as "unchanged".
    Note,
}

/// One line of a rendered diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// Kind of the line.
    pub kind: DiffKind,
    /// Text including the `+`/`-`/` ` prefix.
    pub text: String,
}

impl DiffLine {
    fn new(kind: DiffKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// Unified diff of one file with three lines of context. Returns a single
/// note line if the texts are equal.
pub fn file_diff(
    name: &str,
    old: &str,
    new: &str,
    old_label: &str,
    new_label: &str,
) -> Vec<DiffLine> {
    if old == new {
        return vec![DiffLine::new(DiffKind::Note, format!("{name}: unchanged"))];
    }
    let diff = TextDiff::from_lines(old, new);
    let text = diff
        .unified_diff()
        .context_radius(3)
        .header(
            &format!("{name} ({old_label})"),
            &format!("{name} ({new_label})"),
        )
        .to_string();
    text.lines()
        .enumerate()
        .map(|(i, line)| {
            let kind = if i < 2 {
                DiffKind::Header
            } else if line.starts_with("@@") {
                DiffKind::Hunk
            } else if line.starts_with('+') {
                DiffKind::Added
            } else if line.starts_with('-') {
                DiffKind::Removed
            } else if line.starts_with('\\') {
                DiffKind::Note
            } else {
                DiffKind::Context
            };
            DiffLine::new(kind, line)
        })
        .collect()
}

/// Diff of both configuration files: `network.toml` first, then `local.toml`.
pub fn config_diff(
    old: (&str, &str),
    new: (&str, &str),
    old_label: &str,
    new_label: &str,
) -> Vec<DiffLine> {
    let mut lines = file_diff("network.toml", old.0, new.0, old_label, new_label);
    lines.push(DiffLine::new(DiffKind::Note, ""));
    lines.extend(file_diff("local.toml", old.1, new.1, old_label, new_label));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_lines() {
        let lines = file_diff("network.toml", "a\nb\nc\n", "a\nB\nc\n", "running", "draft");
        let kinds: Vec<DiffKind> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                DiffKind::Header,
                DiffKind::Header,
                DiffKind::Hunk,
                DiffKind::Context,
                DiffKind::Removed,
                DiffKind::Added,
                DiffKind::Context,
            ]
        );
        assert_eq!(lines[0].text, "--- network.toml (running)");
        assert_eq!(lines[1].text, "+++ network.toml (draft)");
        assert_eq!(lines[4].text, "-b");
        assert_eq!(lines[5].text, "+B");
    }

    #[test]
    fn equal_texts_give_a_note() {
        let lines = config_diff(("m", "l\n"), ("m", "l2\n"), "a", "b");
        assert_eq!(
            lines[0],
            DiffLine::new(DiffKind::Note, "network.toml: unchanged")
        );
        assert!(lines.iter().any(|l| l.text == "+l2"));
    }
}
