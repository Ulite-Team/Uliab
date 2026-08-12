//! Source positions and line/column lookup.
//!
//! All lexer tokens, AST nodes, and diagnostics carry a [`Span`] — a half-open
//! byte range into the original source text. Line/column information is not
//! stored on spans (it would go stale on every edit); it is computed on demand
//! through [`LineIndex`], which is what the CLI and the LSP use to render
//! `file:line:col` diagnostics.

/// A half-open `[start, end)` byte range into source text.
///
/// Byte offsets are used rather than character offsets so that spans map
/// directly to Rust `str` slicing; the LSP converts them to UTF-16 ranges at
/// the protocol boundary.
///
/// # Invariants
///
/// `start <= end`, and both offsets are in bounds for the source they refer
/// to. The invariant is enforced by construction everywhere a span is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    /// First byte offset (inclusive).
    pub start: u32,
    /// One past the last byte offset (exclusive).
    pub end: u32,
}

impl Span {
    /// A span covering no bytes, anchored at byte offset `pos`.
    #[must_use]
    pub fn empty(pos: u32) -> Self {
        Self {
            start: pos,
            end: pos,
        }
    }

    /// Returns a span that covers both `self` and `other`, assuming they
    /// come from the same source.
    #[must_use]
    pub fn cover(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// Byte length of the spanned region.
    #[must_use]
    pub fn len(self) -> usize {
        (self.end - self.start) as usize
    }

    /// Whether the span covers no bytes.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// Precomputed line-start offsets for a piece of source text.
///
/// Created once per source file; every `line_col` lookup is a binary search
/// over the line starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    /// Byte offset of the first byte of each line.
    line_starts: Vec<u32>,
    /// Total byte length of the indexed source.
    source_len: u32,
}

impl LineIndex {
    /// Builds the index from source text.
    ///
    /// # Examples
    ///
    /// ```
    /// use ulb_lang::span::LineIndex;
    ///
    /// let index = LineIndex::new("first line\nsecond line\n");
    /// assert_eq!(index.line_col(0), (1, 1));
    /// assert_eq!(index.line_col(12), (2, 2));
    /// assert_eq!(index.line_col(13), (2, 3));
    /// ```
    #[must_use]
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0_u32];
        for (i, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        let source_len = source.len() as u32;
        Self {
            line_starts,
            source_len,
        }
    }

    /// 1-based `(line, column)` for a byte offset, clamping offsets past the
    /// end of the source to the last position.
    ///
    /// The column is a byte column, not a character column: a multi-byte
    /// character advances the column by its byte length. The LSP layer is
    /// responsible for converting to UTF-16 columns.
    #[must_use]
    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        let offset = offset.min(self.source_len);
        let line = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .max(1);
        let line_start = self.line_starts[line - 1];
        (line as u32, offset - line_start + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{LineIndex, Span};

    #[test]
    fn span_covers_and_len() {
        let a = Span { start: 2, end: 4 };
        let b = Span { start: 8, end: 10 };
        assert_eq!(a.cover(b), Span { start: 2, end: 10 });
        assert_eq!(a.len(), 2);
        assert!(!a.is_empty());
        assert!(Span::empty(3).is_empty());
    }

    #[test]
    fn line_index_reports_line_and_byte_column() {
        let index = LineIndex::new("a\nbc\ndef");
        assert_eq!(index.line_col(0), (1, 1));
        assert_eq!(index.line_col(1), (1, 2));
        assert_eq!(index.line_col(2), (2, 1));
        assert_eq!(index.line_col(4), (2, 3));
        assert_eq!(index.line_col(6), (3, 2));
    }

    #[test]
    fn line_index_clamps_past_end_offset() {
        let index = LineIndex::new("abc");
        assert_eq!(index.line_col(10), (1, 4));
    }
}
