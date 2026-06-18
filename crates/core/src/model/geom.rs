//! Page geometry primitives for the unified editable document model.
//!
//! All measurements are in **PDF points** (`1pt = 1/72 in`), `f64`. Field names
//! mirror the HTML renderer's [`Margins`](crate::html::Margins) (`top`/`right`/
//! `bottom`/`left`) so the two never drift; this type is kept separate so the
//! `model` tree is self-contained (importers/exporters convert at the edges).

/// Per-side page margins, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Margins {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl Margins {
    /// The same margin on every side.
    pub fn uniform(m: f64) -> Self {
        Self {
            top: m,
            right: m,
            bottom: m,
            left: m,
        }
    }

    /// Vertical (`top`/`bottom`) and horizontal (`left`/`right`) margins.
    pub fn symmetric(vertical: f64, horizontal: f64) -> Self {
        Self {
            top: vertical,
            right: horizontal,
            bottom: vertical,
            left: horizontal,
        }
    }
}

impl Default for Margins {
    /// 0.5" on every side (`36pt`), matching [`html::Margins`](crate::html::Margins).
    fn default() -> Self {
        Self::uniform(36.0)
    }
}

/// A page's resolved size and margins, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageGeometry {
    pub width: f64,
    pub height: f64,
    pub margins: Margins,
}

impl PageGeometry {
    /// A4 portrait (595.27 × 841.89 pt) with default (0.5") margins.
    pub fn a4() -> Self {
        Self {
            width: 210.0 * (72.0 / 25.4),
            height: 297.0 * (72.0 / 25.4),
            margins: Margins::default(),
        }
    }
}

impl Default for PageGeometry {
    fn default() -> Self {
        Self::a4()
    }
}

/// An axis-aligned rectangle: lower-left `(x, y)` plus width/height, in points.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }
}

/// Block rotation. The four cardinal angles are first-class so that the common
/// `/Rotate` cases are exact; [`Rotation::Deg`] carries an arbitrary angle in
/// degrees (counter-clockwise) for free-form rotated boxes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Rotation {
    #[default]
    D0,
    D90,
    D180,
    D270,
    /// Arbitrary angle, **degrees** counter-clockwise.
    Deg(f64),
}

impl Rotation {
    /// The rotation as a degree value (CCW).
    pub fn degrees(self) -> f64 {
        match self {
            Rotation::D0 => 0.0,
            Rotation::D90 => 90.0,
            Rotation::D180 => 180.0,
            Rotation::D270 => 270.0,
            Rotation::Deg(d) => d,
        }
    }
}
