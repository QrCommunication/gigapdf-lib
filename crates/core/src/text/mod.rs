//! Text analysis utilities that don't belong to the content stream itself.
//!
//! Currently: Unicode reading-direction and script/language detection
//! ([`direction`]). Pure `std`, no third-party library.

pub mod direction;

pub use direction::{
    direction_str, document_language, run_direction, script_str, Direction, DocLanguage, Script,
};
