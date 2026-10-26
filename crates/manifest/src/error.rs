//! A rejection that names the field it is about.

/// A Manifest that cannot be normalized.
///
/// `path` is a dotted path into the document (`resources.web.kind`,
/// `resources.web.ports[1].hostname`), or the empty string when the problem is
/// the document as a whole. The API surfaces it as `path` on the problem body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError {
    pub path: String,
    pub message: String,
}

impl ManifestError {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for ManifestError {}
