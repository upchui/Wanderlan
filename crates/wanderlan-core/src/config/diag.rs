//! Diagnostics (errors and warnings) produced while loading a configuration.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

use serde::{Deserialize, Serialize};

/// Which configuration file a diagnostic refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigFile {
    /// The shared `network.toml`.
    Network,
    /// The site-local `local.toml`.
    Local,
}

impl ConfigFile {
    /// Default file name.
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Network => "network.toml",
            Self::Local => "local.toml",
        }
    }
}

/// Severity of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The configuration is rejected.
    Error,
    /// The configuration is accepted, but something looks suspicious.
    Warning,
}

/// Location of a value in the configuration model, e.g. `site[2].hub`.
///
/// Indices are zero-based positions in the respective array of tables. The
/// TUI uses these paths to highlight the offending form field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FieldPath(String);

impl FieldPath {
    /// The whole file.
    pub fn root() -> Self {
        Self(String::new())
    }

    /// A key in the `[network]` table, or the table itself for `""`.
    pub fn network(field: &str) -> Self {
        if field.is_empty() {
            Self("network".into())
        } else {
            Self(format!("network.{field}"))
        }
    }

    /// A `[[site]]` table.
    pub fn site(index: usize) -> Self {
        Self(format!("site[{index}]"))
    }

    /// A key in a `[[site]]` table.
    pub fn site_field(index: usize, field: &str) -> Self {
        Self(format!("site[{index}].{field}"))
    }

    /// A `[[map]]` table.
    pub fn map(index: usize) -> Self {
        Self(format!("map[{index}]"))
    }

    /// A key in a `[[map]]` table.
    pub fn map_field(index: usize, field: &str) -> Self {
        Self(format!("map[{index}].{field}"))
    }

    /// A `[[tunnel]]` table.
    pub fn tunnel(index: usize) -> Self {
        Self(format!("tunnel[{index}]"))
    }

    /// A key in a `[[tunnel]]` table.
    pub fn tunnel_field(index: usize, field: &str) -> Self {
        Self(format!("tunnel[{index}].{field}"))
    }

    /// A top-level key of `local.toml`.
    pub fn local(field: &str) -> Self {
        Self(field.to_string())
    }

    /// The path as string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The enclosing path (`site[2].hub` → `site[2]` → root).
    pub fn parent(&self) -> Option<Self> {
        if self.0.is_empty() {
            return None;
        }
        match self.0.rfind(['.', '[']) {
            Some(pos) if self.0.as_bytes().get(pos) == Some(&b'.') => {
                Some(Self(self.0[..pos].to_string()))
            }
            Some(_) | None => Some(Self::root()),
        }
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One error or warning, with its position in the source file if known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Error or warning.
    pub severity: Severity,
    /// File the diagnostic refers to.
    pub file: ConfigFile,
    /// Location in the model.
    pub path: FieldPath,
    /// One-based line number in the source file, if known.
    pub line: Option<usize>,
    /// Human-readable description.
    pub message: String,
}

impl Diagnostic {
    /// Creates an error without a line number.
    pub fn error(file: ConfigFile, path: FieldPath, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            file,
            path,
            line: None,
            message: message.into(),
        }
    }

    /// Creates a warning without a line number.
    pub fn warning(file: ConfigFile, path: FieldPath, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(file, path, message)
        }
    }

    /// Returns true for errors.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// Formats the diagnostic with a custom file label (e.g. a full path).
    pub fn display_with<'a>(&'a self, label: &'a str) -> impl fmt::Display + 'a {
        DisplayWith { diag: self, label }
    }
}

struct DisplayWith<'a> {
    diag: &'a Diagnostic,
    label: &'a str,
}

impl fmt::Display for DisplayWith<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.diag.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        match self.diag.line {
            Some(line) => write!(f, "{}:{}: {kind}: {}", self.label, line, self.diag.message),
            None => write!(f, "{}: {kind}: {}", self.label, self.diag.message),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.display_with(self.file.file_name()).fmt(f)
    }
}

/// Returns true if any of the diagnostics is an error.
pub fn has_errors(diags: &[Diagnostic]) -> bool {
    diags.iter().any(Diagnostic::is_error)
}

/// Maps byte offsets in a source text to one-based line numbers.
#[derive(Debug, Clone, Default)]
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    /// Builds the index for a source text.
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self { starts }
    }

    /// One-based line containing the byte offset.
    pub fn line(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(i) => i + 1,
            Err(i) => i,
        }
    }
}

/// Source positions of the values of one parsed file.
#[derive(Debug, Clone, Default)]
pub struct SpanMap {
    lines: LineIndex,
    spans: HashMap<FieldPath, Range<usize>>,
}

impl SpanMap {
    /// Creates an empty map for the given source text.
    pub fn new(text: &str) -> Self {
        Self {
            lines: LineIndex::new(text),
            spans: HashMap::new(),
        }
    }

    /// Records the span of a value.
    pub fn insert(&mut self, path: FieldPath, span: Range<usize>) {
        self.spans.insert(path, span);
    }

    /// Line of a value; falls back to the enclosing table if the value itself
    /// is missing (e.g. for "required key missing" errors).
    pub fn line(&self, path: &FieldPath) -> Option<usize> {
        let mut current = Some(path.clone());
        while let Some(p) = current {
            if let Some(span) = self.spans.get(&p) {
                return Some(self.lines.line(span.start));
            }
            current = p.parent();
        }
        None
    }

    /// Line of a byte offset.
    pub fn line_of_offset(&self, offset: usize) -> usize {
        self.lines.line(offset)
    }

    /// Fills in missing line numbers of diagnostics for the given file.
    pub fn locate(&self, file: ConfigFile, diags: &mut [Diagnostic]) {
        for diag in diags
            .iter_mut()
            .filter(|d| d.file == file && d.line.is_none())
        {
            diag.line = self.line(&diag.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index() {
        let idx = LineIndex::new("a\nbc\n\nd");
        assert_eq!(idx.line(0), 1);
        assert_eq!(idx.line(1), 1);
        assert_eq!(idx.line(2), 2);
        assert_eq!(idx.line(5), 3);
        assert_eq!(idx.line(6), 4);
    }

    #[test]
    fn path_parents() {
        let p = FieldPath::site_field(2, "hub");
        assert_eq!(p.as_str(), "site[2].hub");
        let parent = p.parent().unwrap();
        assert_eq!(parent, FieldPath::site(2));
        assert_eq!(parent.parent().unwrap(), FieldPath::root());
        assert_eq!(FieldPath::root().parent(), None);
        assert_eq!(
            FieldPath::network("vlans").parent().unwrap(),
            FieldPath::network("")
        );
    }

    #[test]
    fn display() {
        let mut d = Diagnostic::error(ConfigFile::Network, FieldPath::root(), "boom");
        assert_eq!(d.to_string(), "network.toml: error: boom");
        d.line = Some(3);
        assert_eq!(d.to_string(), "network.toml:3: error: boom");
        assert_eq!(
            d.display_with("/etc/x.toml").to_string(),
            "/etc/x.toml:3: error: boom"
        );
    }
}
