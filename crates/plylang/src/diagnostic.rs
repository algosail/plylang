use crate::codes::{error_url, Error};
use crate::span::{line_col, Span};

/// A jump from a diagnostic to the file that caused it.
/// `plylang check` does not print it. The language server does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Related {
    pub path: String,
    pub span: Span,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    /// `PL0001`. Taken from the [`Error`] that produced [`Self::message`].
    pub code: &'static str,
    pub message: String,
    /// The file that explains this diagnostic. Absent for an error written here.
    pub related: Option<Related>,
}

impl Diagnostic {
    pub fn new(span: Span, error: Error) -> Self {
        Self {
            span,
            code: error.code(),
            message: error.message(),
            related: None,
        }
    }

    /// A diagnostic whose sentence was already formatted from an [`Error`].
    pub fn with_code(span: Span, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            span,
            code,
            message: message.into(),
            related: None,
        }
    }

    pub fn relate(mut self, related: Related) -> Self {
        self.related = Some(related);
        self
    }

    /// `PLxxxx: message` and the link to that section of `docs/Errors.md`.
    pub fn reported(&self) -> String {
        format!("{}: {}\n{}", self.code, self.message, self.url())
    }

    pub fn url(&self) -> String {
        error_url(self.code)
    }

    pub fn render(&self, filename: &str, source: &str) -> String {
        let (line, col) = line_col(source, self.span.start);
        format!("{filename}:{line}:{col}: error: {}", self.reported())
    }
}

/// An editor note. `plylang check` does not fail on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub span: Span,
    pub message: String,
    /// The file that explains this note.
    pub related: Option<Related>,
    /// The editor fades this span. A module that does not check is not unused.
    pub unnecessary: bool,
}
