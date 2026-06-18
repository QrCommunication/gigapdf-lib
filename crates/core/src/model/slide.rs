//! Presentation (slide) content for the unified editable document model.
//!
//! A [`SlideBlock`] holds an ordered list of [`Slide`]s; each slide has its own
//! geometry, a set of free-floating shape [`Block`]s, semantic
//! [`Placeholder`]s (title/body/…), and optional speaker notes. This is the
//! editable counterpart of the PPTX/ODP reconstruction path.

use crate::model::geom::PageGeometry;
use crate::model::Block;

/// A block of presentation content: an ordered list of slides.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SlideBlock {
    pub slides: Vec<Slide>,
}

/// A single slide: its size, free-floating shapes, semantic placeholders, and
/// optional speaker notes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Slide {
    pub geometry: PageGeometry,
    /// Free-floating, absolutely-positioned shapes (text boxes, images, …).
    pub shapes: Vec<Block>,
    /// Layout placeholders carrying a semantic role.
    pub placeholders: Vec<Placeholder>,
    /// Speaker notes, as document blocks.
    pub notes: Option<Vec<Block>>,
}

/// A layout placeholder: a [`Block`] tagged with its semantic [`PlaceholderRole`].
#[derive(Debug, Clone, PartialEq)]
pub struct Placeholder {
    pub role: PlaceholderRole,
    pub block: Block,
}

/// The semantic role of a slide placeholder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PlaceholderRole {
    #[default]
    Title,
    Subtitle,
    Body,
    /// Any other named placeholder (footer, slide-number, custom layout slot…).
    Other(String),
}
