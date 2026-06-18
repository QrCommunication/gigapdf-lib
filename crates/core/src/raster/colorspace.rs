//! PDF colour-space resolution (ISO 32000-1 §8.6): turn an `n`-component colour
//! into device RGB. Zero dependency.
//!
//! The variants here are *resolved* — the object graph (streams, tint functions,
//! palettes) has already been read by the document into self-contained data, so
//! converting a colour needs no further object lookups except evaluating a tint
//! transform, which is delegated through the [`TintEval`] callback (the document
//! owns the PDF function evaluator and we must not duplicate it).
//!
//! Special / device-dependent spaces are mapped to their device equivalents
//! (CalRGB→RGB, CalGray→Gray, ICCBased→its `/N`-implied device space or
//! `/Alternate`). A full colour-management module (ICC profiles, white-point
//! adaptation beyond the cheap Lab→sRGB path) is intentionally out of scope: the
//! device fallback is visually faithful for the page-rasterizer use case.

use crate::object::Object;

/// A callback to evaluate a PDF tint-transform function with `inputs.len()`
/// arguments, returning the alternate-space component vector. Implemented by the
/// document (which owns the function evaluator). Keeps this module object-graph
/// free while supporting Separation/DeviceN tint transforms.
pub trait TintEval {
    /// Evaluate `func` (a PDF function object) at the given input components.
    fn eval(&self, func: &Object, inputs: &[f64]) -> Vec<f64>;
}

/// A resolved PDF colour space able to convert `n` input components to RGB.
///
/// `Separation`/`DeviceN` carry the tint-transform function object and a boxed
/// alternate space; the conversion evaluates the tint via [`TintEval`] then
/// recurses into the alternate. `Indexed` bakes its palette as raw base-space
/// component samples (`hival + 1` rows of `base.components()` values each).
#[derive(Debug, Clone)]
pub enum ColorSpace {
    /// 1 component, grey replicated to RGB.
    DeviceGray,
    /// 3 components, used directly.
    DeviceRgb,
    /// 4 components, subtractive CMYK → RGB.
    DeviceCmyk,
    /// CIE L*a*b* (`/Lab`): white point + `a`/`b` ranges. 3 inputs.
    Lab {
        /// `/WhitePoint` `[Xw Yw Zw]` (D50 default).
        white: [f64; 3],
        /// `/Range` `[amin amax bmin bmax]` (default `[-100 100 -100 100]`).
        range: [f64; 4],
    },
    /// `/ICCBased`: behaves like its `/N`-implied device space (1/3/4) or, when
    /// present, an explicit `/Alternate` space.
    Icc {
        /// Number of components (`/N`).
        n: usize,
        /// Resolved `/Alternate` space, if any (else device-by-`n`).
        alternate: Option<Box<ColorSpace>>,
    },
    /// `/Indexed [base hival lookup]`: index → palette entry in `base`.
    Indexed {
        /// The base colour space the palette entries are expressed in.
        base: Box<ColorSpace>,
        /// Highest valid index (palette has `hival + 1` entries).
        hival: usize,
        /// `hival + 1` rows of `base.components()` raw component values in `0..=1`.
        palette: Vec<f64>,
    },
    /// `/Separation` or `/DeviceN`: `names.len()` tint inputs → `alternate` via
    /// `tint`. `/Separation` is the single-colorant case (`names == 1`).
    Separation {
        /// Number of colorant tints (1 for `/Separation`).
        n: usize,
        /// `true` when the (single) colorant is `/None` — nothing is painted, so
        /// the colour is treated as white (no marks).
        none: bool,
        /// The alternate colour space the tint transform outputs into.
        alternate: Box<ColorSpace>,
        /// The tint-transform function object (evaluated via [`TintEval`]).
        tint: Object,
    },
}

impl ColorSpace {
    /// The number of colour components this space takes as input.
    pub fn components(&self) -> usize {
        match self {
            ColorSpace::DeviceGray => 1,
            ColorSpace::DeviceRgb => 3,
            ColorSpace::DeviceCmyk => 4,
            ColorSpace::Lab { .. } => 3,
            ColorSpace::Icc { n, .. } => *n,
            ColorSpace::Indexed { .. } => 1,
            ColorSpace::Separation { n, .. } => *n,
        }
    }

    /// Convert `comps` (length should equal [`components`](Self::components),
    /// missing entries default to 0) to device RGB. `eval` evaluates tint
    /// transforms for Separation/DeviceN.
    pub fn to_rgb(&self, comps: &[f64], eval: &dyn TintEval) -> [u8; 3] {
        let f = self.to_rgb_f(comps, eval);
        [to_byte(f[0]), to_byte(f[1]), to_byte(f[2])]
    }

    /// As [`to_rgb`](Self::to_rgb) but returning unclamped `0..=1` floats, used
    /// internally so a recursive alternate space composes without rounding.
    fn to_rgb_f(&self, comps: &[f64], eval: &dyn TintEval) -> [f64; 3] {
        let g = |i: usize| comps.get(i).copied().unwrap_or(0.0);
        match self {
            ColorSpace::DeviceGray => {
                let v = g(0);
                [v, v, v]
            }
            ColorSpace::DeviceRgb => [g(0), g(1), g(2)],
            ColorSpace::DeviceCmyk => cmyk(g(0), g(1), g(2), g(3)),
            ColorSpace::Lab { white, range } => lab_to_rgb(g(0), g(1), g(2), *white, *range),
            ColorSpace::Icc { n, alternate } => match alternate {
                Some(alt) => alt.to_rgb_f(comps, eval),
                None => device_by_n(*n, comps),
            },
            ColorSpace::Indexed {
                base,
                hival,
                palette,
            } => {
                let stride = base.components().max(1);
                let idx = (g(0).round() as i64).clamp(0, *hival as i64) as usize;
                let start = idx * stride;
                let row: Vec<f64> = (0..stride)
                    .map(|c| palette.get(start + c).copied().unwrap_or(0.0))
                    .collect();
                base.to_rgb_f(&row, eval)
            }
            ColorSpace::Separation {
                none,
                alternate,
                tint,
                ..
            } => {
                if *none {
                    // `/None` colorant: produces no marks — treat as white.
                    return [1.0, 1.0, 1.0];
                }
                let alt_comps = eval.eval(tint, comps);
                alternate.to_rgb_f(&alt_comps, eval)
            }
        }
    }
}

/// Device-space-by-component-count fallback (ICCBased without `/Alternate`).
fn device_by_n(n: usize, comps: &[f64]) -> [f64; 3] {
    let g = |i: usize| comps.get(i).copied().unwrap_or(0.0);
    match n {
        1 => {
            let v = g(0);
            [v, v, v]
        }
        4 => cmyk(g(0), g(1), g(2), g(3)),
        _ => [g(0), g(1), g(2)],
    }
}

/// Subtractive CMYK → RGB (matches `render::cmyk_to_rgb`, unclamped floats).
fn cmyk(c: f64, m: f64, y: f64, k: f64) -> [f64; 3] {
    [
        (1.0 - c) * (1.0 - k),
        (1.0 - m) * (1.0 - k),
        (1.0 - y) * (1.0 - k),
    ]
}

/// CIE L*a*b* (D50 default white point) → sRGB. A compact, dependency-free
/// approximation: Lab→XYZ (CIE) then a linear XYZ→sRGB matrix with the standard
/// sRGB transfer. Good enough for faithful rendering; not a calibrated CMM.
fn lab_to_rgb(l: f64, a_in: f64, b_in: f64, white: [f64; 3], range: [f64; 4]) -> [f64; 3] {
    let a = a_in.clamp(range[0].min(range[1]), range[0].max(range[1]));
    let b = b_in.clamp(range[2].min(range[3]), range[2].max(range[3]));
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let inv = |t: f64| {
        if t > 6.0 / 29.0 {
            t * t * t
        } else {
            3.0 * (6.0f64 / 29.0).powi(2) * (t - 4.0 / 29.0)
        }
    };
    let xn = if white[0] > 0.0 { white[0] } else { 0.9642 };
    let yn = if white[1] > 0.0 { white[1] } else { 1.0 };
    let zn = if white[2] > 0.0 { white[2] } else { 0.8249 };
    let x = xn * inv(fx);
    let y = yn * inv(fy);
    let z = zn * inv(fz);
    // XYZ (D50) → linear sRGB (Bradford-adapted D50→D65 baked into the matrix).
    let rl = 3.1338561 * x - 1.6168667 * y - 0.4906146 * z;
    let gl = -0.9787684 * x + 1.9161415 * y + 0.0334540 * z;
    let bl = 0.0719453 * x - 0.2289914 * y + 1.4052427 * z;
    [gamma(rl), gamma(gl), gamma(bl)]
}

/// Linear → sRGB transfer (companding).
fn gamma(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Clamp `0..=1` and scale to a byte.
fn to_byte(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoTint;
    impl TintEval for NoTint {
        fn eval(&self, _func: &Object, _inputs: &[f64]) -> Vec<f64> {
            Vec::new()
        }
    }

    /// A tint that maps a single tint `t` to CMYK `[0, 0, 0, t]` (a `/Separation`
    /// "Black"-like colorant), independent of the function object.
    struct BlackTint;
    impl TintEval for BlackTint {
        fn eval(&self, _func: &Object, inputs: &[f64]) -> Vec<f64> {
            let t = inputs.first().copied().unwrap_or(0.0);
            vec![0.0, 0.0, 0.0, t]
        }
    }

    #[test]
    fn device_gray_and_rgb_and_cmyk() {
        assert_eq!(
            ColorSpace::DeviceGray.to_rgb(&[0.5], &NoTint),
            [128, 128, 128]
        );
        assert_eq!(
            ColorSpace::DeviceRgb.to_rgb(&[1.0, 0.0, 0.0], &NoTint),
            [255, 0, 0]
        );
        // CMYK pure cyan → RGB (0,255,255).
        assert_eq!(
            ColorSpace::DeviceCmyk.to_rgb(&[1.0, 0.0, 0.0, 0.0], &NoTint),
            [0, 255, 255]
        );
    }

    #[test]
    fn icc_n3_acts_like_rgb() {
        let cs = ColorSpace::Icc {
            n: 3,
            alternate: None,
        };
        assert_eq!(cs.to_rgb(&[0.0, 1.0, 0.0], &NoTint), [0, 255, 0]);
    }

    #[test]
    fn indexed_maps_into_palette() {
        // 2-entry RGB palette: index 0 = red, index 1 = blue.
        let cs = ColorSpace::Indexed {
            base: Box::new(ColorSpace::DeviceRgb),
            hival: 1,
            palette: vec![1.0, 0.0, 0.0, 0.0, 0.0, 1.0],
        };
        assert_eq!(cs.to_rgb(&[0.0], &NoTint), [255, 0, 0]);
        assert_eq!(cs.to_rgb(&[1.0], &NoTint), [0, 0, 255]);
        // Out-of-range index is clamped to hival.
        assert_eq!(cs.to_rgb(&[5.0], &NoTint), [0, 0, 255]);
    }

    #[test]
    fn separation_tint_to_cmyk_alternate() {
        let cs = ColorSpace::Separation {
            n: 1,
            none: false,
            alternate: Box::new(ColorSpace::DeviceCmyk),
            tint: Object::Null,
        };
        // Full tint (1.0) → CMYK k=1 → black.
        assert_eq!(cs.to_rgb(&[1.0], &BlackTint), [0, 0, 0]);
        // Zero tint → CMYK all zero → white.
        assert_eq!(cs.to_rgb(&[0.0], &BlackTint), [255, 255, 255]);
    }

    #[test]
    fn separation_none_is_white() {
        let cs = ColorSpace::Separation {
            n: 1,
            none: true,
            alternate: Box::new(ColorSpace::DeviceCmyk),
            tint: Object::Null,
        };
        assert_eq!(cs.to_rgb(&[1.0], &NoTint), [255, 255, 255]);
    }
}
