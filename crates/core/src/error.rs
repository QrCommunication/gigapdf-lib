//! Engine error type — hand-written (no `thiserror`), zero dependencies.

use std::fmt;

/// Result alias for all fallible engine operations.
pub type Result<T> = std::result::Result<T, EngineError>;

/// Everything that can go wrong while parsing or editing a PDF.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineError {
    /// Lexing/parsing failure at a byte offset, with a human-readable reason.
    Parse { offset: usize, message: String },
    /// A required key, object, or structure was missing.
    Missing(String),
    /// A feature that is valid PDF but not (yet) implemented.
    Unsupported(String),
    /// A stream filter (e.g. FlateDecode) failed to decode.
    Filter(String),
    /// A content stream could not be parsed or re-encoded.
    Content(String),
    /// A 1-based page number that does not exist was requested.
    PageNotFound(u32),
    /// A content-object index that does not exist on the page was requested.
    RunNotFound { index: usize, page: u32 },
    /// A caller-supplied argument was malformed or out of range (e.g. a
    /// degenerate page-box rectangle, or an unknown enum discriminant).
    InvalidArgument(String),
}

impl EngineError {
    /// Convenience constructor for a parse error.
    pub fn parse(offset: usize, message: impl Into<String>) -> Self {
        EngineError::Parse {
            offset,
            message: message.into(),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Parse { offset, message } => {
                write!(f, "parse error at byte {offset}: {message}")
            }
            EngineError::Missing(what) => write!(f, "missing: {what}"),
            EngineError::Unsupported(what) => write!(f, "unsupported: {what}"),
            EngineError::Filter(why) => write!(f, "filter error: {why}"),
            EngineError::Content(why) => write!(f, "content stream error: {why}"),
            EngineError::PageNotFound(page) => write!(f, "page {page} not found"),
            EngineError::RunNotFound { index, page } => {
                write!(f, "content object #{index} not found on page {page}")
            }
            EngineError::InvalidArgument(why) => write!(f, "invalid argument: {why}"),
        }
    }
}

impl std::error::Error for EngineError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_constructor_builds_parse_variant() {
        let e = EngineError::parse(42, "bad token");
        assert_eq!(
            e,
            EngineError::Parse {
                offset: 42,
                message: "bad token".to_string(),
            }
        );
    }

    #[test]
    fn display_covers_every_variant() {
        assert_eq!(
            EngineError::parse(7, "oops").to_string(),
            "parse error at byte 7: oops"
        );
        assert_eq!(
            EngineError::Missing("Root".into()).to_string(),
            "missing: Root"
        );
        assert_eq!(
            EngineError::Unsupported("JBIG2".into()).to_string(),
            "unsupported: JBIG2"
        );
        assert_eq!(
            EngineError::Filter("flate".into()).to_string(),
            "filter error: flate"
        );
        assert_eq!(
            EngineError::Content("ops".into()).to_string(),
            "content stream error: ops"
        );
        assert_eq!(EngineError::PageNotFound(3).to_string(), "page 3 not found");
        assert_eq!(
            EngineError::RunNotFound { index: 5, page: 2 }.to_string(),
            "content object #5 not found on page 2"
        );
        assert_eq!(
            EngineError::InvalidArgument("rect".into()).to_string(),
            "invalid argument: rect"
        );
    }

    #[test]
    fn implements_std_error_and_is_usable_as_trait_object() {
        let e = EngineError::Missing("x".into());
        let dyn_err: &dyn std::error::Error = &e;
        assert_eq!(dyn_err.to_string(), "missing: x");
        // Result alias round-trips an Err of this type.
        let r: Result<()> = Err(EngineError::PageNotFound(1));
        assert!(r.is_err());
    }
}
