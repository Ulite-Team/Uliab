//! Diagnostics produced by the lexer and parser.
//!
//! Diagnostics carry a source span, a severity, and a human-readable message.
//! They are rendered as `file:line:col: severity: message` (§11 of
//! GRAMMAR.md); the LSP maps the span and severity directly onto protocol
//! `Diagnostic` values. The lexer and parser never fail-fast: they produce a
//! partial result plus a list of diagnostics, which is the contract that lets
//! the LSP parse mid-edit source.

use crate::span::{LineIndex, Span};

/// Severity of a diagnostic. Ordering is from most to least severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    /// A hard error that makes the input invalid.
    Error,
    /// A valid-but-suspicious construct (e.g. a deprecated reference).
    Warning,
    /// An informational note that does not affect validity.
    Info,
}

impl Severity {
    /// The severity keyword used in rendered diagnostics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

/// A single message attached to a source span.
///
/// # Examples
///
/// ```
/// use ulb_lang::diagnostic::{Diagnostic, Severity};
/// use ulb_lang::span::{LineIndex, Span};
///
/// let source = "compileSdk 37";
/// let diag = Diagnostic {
///     span: Span { start: 0, end: 6 },
///     severity: Severity::Error,
///     message: "unexpected identifier".to_owned(),
/// };
/// let rendered = diag.render("build.ulb", &LineIndex::new(source));
/// assert_eq!(rendered, "build.ulb:1:1: error: unexpected identifier");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Source span the message refers to.
    pub span: Span,
    /// Severity of the message.
    pub severity: Severity,
    /// Human-readable message (no location prefix; added by [`render`]).
    ///
    /// [`render`]: Diagnostic::render
    pub message: String,
}

impl Diagnostic {
    /// Renders `file:line:col: severity: message` (§11 of GRAMMAR.md).
    ///
    /// `file` is the source filename as it should appear in the output; the
    /// line/column are derived from the precomputed line index of the
    /// source.
    #[must_use]
    pub fn render(&self, file: &str, lines: &LineIndex) -> String {
        let (line, col) = lines.line_col(self.span.start);
        format!(
            "{file}:{line}:{col}: {}: {}",
            self.severity.as_str(),
            self.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Diagnostic, Severity};
    use crate::span::{LineIndex, Span};

    #[test]
    fn severity_words() {
        assert_eq!(Severity::Error.as_str(), "error");
        assert_eq!(Severity::Warning.as_str(), "warning");
        assert_eq!(Severity::Info.as_str(), "info");
    }

    #[test]
    fn render_uses_span_line_column() {
        let source = "line one\ncompileSdk 37";
        let lines = LineIndex::new(source);
        let diag = Diagnostic {
            span: Span { start: 9, end: 15 },
            severity: Severity::Error,
            message: "missing value".to_owned(),
        };
        assert_eq!(
            diag.render("build.ulb", &lines),
            "build.ulb:2:1: error: missing value"
        );
    }
}
