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
}

impl EngineError {
    /// Convenience constructor for a parse error.
    pub fn parse(offset: usize, message: impl Into<String>) -> Self {
        EngineError::Parse { offset, message: message.into() }
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
        }
    }
}

impl std::error::Error for EngineError {}
