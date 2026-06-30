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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_contains_area_and_intersects() {
        let b = Bounds {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 4.0,
        };
        assert!(b.contains(5.0, 2.0));
        assert!(b.contains(0.0, 0.0)); // edge counts as inside
        assert!(!b.contains(11.0, 2.0));
        assert_eq!(b.area(), 40.0);

        let touching = Bounds {
            x: 10.0,
            y: 0.0,
            width: 2.0,
            height: 2.0,
        };
        assert!(b.intersects(&touching)); // touching edges overlap
        let apart = Bounds {
            x: 100.0,
            y: 100.0,
            width: 1.0,
            height: 1.0,
        };
        assert!(!b.intersects(&apart));
    }

    #[test]
    fn builder_build_none_when_empty() {
        let bb = BoundsBuilder::new();
        assert!(bb.build().is_none());
        // Default is the same as new().
        let def = BoundsBuilder::default();
        assert!(def.build().is_none());
    }

    #[test]
    fn builder_accumulates_points_and_transforms() {
        let mut bb = BoundsBuilder::default();
        bb.add(1.0, 1.0);
        bb.add(4.0, 5.0);
        let b = bb.build().expect("non-empty");
        assert_eq!((b.x, b.y, b.width, b.height), (1.0, 1.0, 3.0, 4.0));

        // add_through applies the matrix before accumulating.
        let mut bb2 = BoundsBuilder::new();
        let m = Matrix::new(1.0, 0.0, 0.0, 1.0, 10.0, 20.0); // translate
        bb2.add_through(&m, 0.0, 0.0);
        bb2.add_through(&m, 2.0, 3.0);
        let b2 = bb2.build().unwrap();
        assert_eq!((b2.x, b2.y, b2.width, b2.height), (10.0, 20.0, 2.0, 3.0));
    }
}
