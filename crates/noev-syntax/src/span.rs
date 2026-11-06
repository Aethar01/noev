//! Byte ranges in source files.

/// A half-open byte range `start..end` in a source file.
///
/// Offsets are `u32`, which limits source files to 4 GiB and keeps tokens and syntax-tree
/// nodes small.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    /// Offset of the first byte.
    pub start: u32,
    /// Offset one past the last byte.
    pub end: u32,
}

impl Span {
    /// Creates a span from `start` to `end`.
    pub const fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }

    /// The smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        Span::new(self.start.min(other.start), self.end.max(other.end))
    }

    /// The span's length in bytes.
    pub const fn len(self) -> u32 {
        self.end - self.start
    }

    /// Whether the span covers no bytes.
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// The source text the span covers.
    pub fn text(self, source: &str) -> &str {
        &source[self.start as usize..self.end as usize]
    }
}
