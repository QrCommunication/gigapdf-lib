//! A small content-stream interpreter: tracks the CTM and text matrices so each
//! element gets a bounding box in page user space. That box is what lets a UI
//! map a click to an element (hit-testing).
//!
//! Text width uses an average-advance approximation (real glyph metrics come
//! with the font-aware path); it is accurate enough to pick the element under a
//! pointer.

/// A 2-D affine matrix `[a b c d e f]` in PDF's row-vector convention:
/// a transformed point is `[x y 1] · M`.
#[derive(Clone, Copy, Debug)]
pub struct Matrix(pub [f64; 6]);

impl Matrix {
    pub const IDENTITY: Matrix = Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    pub fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Matrix {
        Matrix([a, b, c, d, e, f])
    }

    pub fn translate(tx: f64, ty: f64) -> Matrix {
        Matrix([1.0, 0.0, 0.0, 1.0, tx, ty])
    }

    /// Compose: `self` applied first, then `other` (row vectors) — `self · other`.
    pub fn then(&self, other: &Matrix) -> Matrix {
        let [a1, b1, c1, d1, e1, f1] = self.0;
        let [a2, b2, c2, d2, e2, f2] = other.0;
        Matrix([
            a1 * a2 + b1 * c2,
            a1 * b2 + b1 * d2,
            c1 * a2 + d1 * c2,
            c1 * b2 + d1 * d2,
            e1 * a2 + f1 * c2 + e2,
            e1 * b2 + f1 * d2 + f2,
        ])
    }

    /// Transform a point.
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
    }
}

/// Axis-aligned bounding box in page user space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Bounds {
    /// Whether a point lies inside the box.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }

    /// Area, used to prefer the smallest element when several overlap.
    pub fn area(&self) -> f64 {
        self.width * self.height
    }

    /// Whether this box overlaps `other` (touching edges count as overlap).
    pub fn intersects(&self, other: &Bounds) -> bool {
        self.x <= other.x + other.width
            && other.x <= self.x + self.width
            && self.y <= other.y + other.height
            && other.y <= self.y + self.height
    }
}

/// Accumulates transformed points into an axis-aligned box.
#[derive(Debug)]
pub struct BoundsBuilder {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    any: bool,
}

impl BoundsBuilder {
    pub fn new() -> Self {
        Self {
            min_x: f64::INFINITY,
            min_y: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            max_y: f64::NEG_INFINITY,
            any: false,
        }
    }

    pub fn add(&mut self, x: f64, y: f64) {
        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
        self.any = true;
    }

    /// Add a point after transforming it through `m`.
    pub fn add_through(&mut self, m: &Matrix, x: f64, y: f64) {
        let (px, py) = m.apply(x, y);
        self.add(px, py);
    }

    pub fn build(&self) -> Option<Bounds> {
        if !self.any || !self.min_x.is_finite() {
            return None;
        }
        Some(Bounds {
            x: self.min_x,
            y: self.min_y,
            width: self.max_x - self.min_x,
            height: self.max_y - self.min_y,
        })
    }
}

impl Default for BoundsBuilder {
    fn default() -> Self {
        Self::new()
    }
}
