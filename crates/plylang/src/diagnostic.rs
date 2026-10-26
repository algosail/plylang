use crate::codes::{error_url, Error};
use crate::span::{line_col, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    /// `PL0001`. Taken from the [`Error`] that produced [`Self::message`].
    pub code: &'static str,
    pub message: String,
}

impl Diagnostic {
    pub fn new(span: Span, error: Error) -> Self {
        Self {
            span,
            code: error.code(),
            message: error.message(),
        }
    }

    /// A diagnostic whose sentence was already formatted from an [`Error`].
    pub fn with_code(span: Span, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            span,
            code,
            message: message.into(),
        }
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
