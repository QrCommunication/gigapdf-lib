//! PDF colour-space resolution (ISO 32000-1 §8.6): turn an `n`-component colour
//! into device RGB. Zero dependency.
//!
//! The variants here are *resolved* — the object graph (streams, tint functions,
//! palettes) has already been read by the document into self-contained data, so
//! converting a colour needs no further object lookups except evaluating a tint
//! transform, which is delegated through the [`TintEval`] callback (the document
//! owns the PDF function evaluator and we must not duplicate it).
//!
//! CIE-calibrated spaces apply what is cheap and dependency-free: `CalGray`
//! decodes its `/Gamma`, `CalRGB` applies per-channel `/Gamma` then the
//! `/Matrix` + white-point-adapted XYZ→sRGB, and `Lab` runs the Lab→sRGB path.
//! `ICCBased` is *not* parsed — it behaves as its `/N`-implied device space
//! (1/3/4) or its explicit `/Alternate`. A full colour-management module (ICC
//! profile parsing, a calibrated CMM) is intentionally out of scope: these
//! approximations are visually faithful for the page-rasterizer use case.

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
    /// `/CalGray` (ISO 32000-1 §8.6.5.2): a single component decoded by the
    /// `/Gamma` transfer (`A^gamma`) to an achromatic luminance. 1 input.
    CalGray {
        /// `/Gamma` exponent (default `1.0`).
        gamma: f64,
    },
    /// `/CalRGB` (ISO 32000-1 §8.6.5.3): per-channel `/Gamma` then the `/Matrix`
    /// (CIE `XYZ = M · [A^GA, B^GB, C^GC]`) and white-point-adapted XYZ→sRGB.
    /// 3 inputs.
    CalRgb {
        /// `/Gamma` `[GA GB GC]` per-channel exponents (default `[1,1,1]`).
        gamma: [f64; 3],
        /// `/Matrix` (column-major `[XA YA ZA XB YB ZB XC YC ZC]`) mapping the
        /// gamma-decoded components to CIE XYZ (default identity).
        matrix: [f64; 9],
        /// `/WhitePoint` `[Xw Yw Zw]` of the calibration white (D65 default).
        white: [f64; 3],
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
            ColorSpace::CalGray { .. } => 1,
            ColorSpace::CalRgb { .. } => 3,
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
    ///
    /// Exposed crate-wide so the vector-path extractor
    /// ([`content::vector`](crate::content::vector)) can drive an editor's shape
    /// layer with the *same* exact colour the rasterizer paints — Separation /
    /// DeviceN tints, ICCBased `/N`, Indexed palettes — instead of a naive
    /// arity-guess. Returns `0..=1` floats so the caller stores `VectorPath.fill`
    /// without an extra round-trip through bytes.
    pub(crate) fn to_rgb_f(&self, comps: &[f64], eval: &dyn TintEval) -> [f64; 3] {
        let g = |i: usize| comps.get(i).copied().unwrap_or(0.0);
        match self {
            ColorSpace::DeviceGray => {
                let v = g(0);
                [v, v, v]
            }
            ColorSpace::DeviceRgb => [g(0), g(1), g(2)],
            ColorSpace::DeviceCmyk => cmyk(g(0), g(1), g(2), g(3)),
            ColorSpace::Lab { white, range } => lab_to_rgb(g(0), g(1), g(2), *white, *range),
            ColorSpace::CalGray { gamma } => cal_gray_to_rgb(g(0), *gamma),
            ColorSpace::CalRgb {
                gamma,
                matrix,
                white,
            } => cal_rgb_to_rgb(g(0), g(1), g(2), *gamma, *matrix, *white),
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

/// `/CalGray` (ISO 32000-1 §8.6.5.2) → device grey: decode the single component
/// through the `/Gamma` transfer (`A^gamma`) into an achromatic luminance, used
/// directly as the grey value (a calibrated CMM is out of scope — applying the
/// gamma is the faithful, dependency-free improvement over the old `A`-as-grey
/// mapping). `gamma <= 0` is treated as `1.0` (identity).
fn cal_gray_to_rgb(a: f64, gamma: f64) -> [f64; 3] {
    let a = a.clamp(0.0, 1.0);
    let g = if gamma > 0.0 { gamma } else { 1.0 };
    let l = a.powf(g);
    [l, l, l]
}

/// `/CalRGB` (ISO 32000-1 §8.6.5.3) → sRGB: decode each component through its
/// `/Gamma`, map the result to CIE XYZ via `/Matrix` (`XYZ = M · [A',B',C']`,
/// column-major), Bradford-adapt the calibration white point to D65, then apply
/// the linear-XYZ→sRGB matrix + transfer. When `/Matrix` is the identity (its
/// default) the gamma-decoded components are already treated as linear sRGB, so
/// only the sRGB companding is applied — the visually-faithful fast path.
fn cal_rgb_to_rgb(
    a: f64,
    b: f64,
    c: f64,
    gammas: [f64; 3],
    matrix: [f64; 9],
    white: [f64; 3],
) -> [f64; 3] {
    let dec = |v: f64, g: f64| {
        let v = v.clamp(0.0, 1.0);
        if g > 0.0 {
            v.powf(g)
        } else {
            v
        }
    };
    let (ad, bd, cd) = (dec(a, gammas[0]), dec(b, gammas[1]), dec(c, gammas[2]));
    if matrix == IDENTITY_MATRIX {
        // Identity matrix: the decoded components are linear sRGB already.
        return [gamma(ad), gamma(bd), gamma(cd)];
    }
    // XYZ = M · [A', B', C'] with M stored column-major [XA YA ZA XB YB ZB XC YC ZC].
    let x = matrix[0] * ad + matrix[3] * bd + matrix[6] * cd;
    let y = matrix[1] * ad + matrix[4] * bd + matrix[7] * cd;
    let z = matrix[2] * ad + matrix[5] * bd + matrix[8] * cd;
    let [xa, ya, za] = bradford_adapt_to_d65(x, y, z, white);
    // XYZ (D65) → linear sRGB (IEC 61966-2.1 matrix).
    let rl = 3.2406255 * xa - 1.5372080 * ya - 0.4986286 * za;
    let gl = -0.9689307 * xa + 1.8757561 * ya + 0.0415175 * za;
    let bl = 0.0557101 * xa - 0.2040211 * ya + 1.0569959 * za;
    [gamma(rl), gamma(gl), gamma(bl)]
}

/// The default (`/Matrix` absent) `/CalRGB` matrix: identity, column-major.
const IDENTITY_MATRIX: [f64; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

/// Bradford chromatic adaptation of an XYZ triple from a source white point to
/// D65. Cone responses are scaled by the white-point ratio in LMS space, the
/// standard cheap CMM-free adaptation; a near-D65 source is effectively a
/// no-op. A degenerate (non-positive `Yw`) white point is left unadapted.
fn bradford_adapt_to_d65(x: f64, y: f64, z: f64, white: [f64; 3]) -> [f64; 3] {
    const D65: [f64; 3] = [0.95047, 1.0, 1.08883];
    if white[1] <= 0.0 {
        return [x, y, z];
    }
    // Bradford forward matrix (XYZ → LMS).
    let to_lms = |x: f64, y: f64, z: f64| {
        [
            0.8951 * x + 0.2664 * y - 0.1614 * z,
            -0.7502 * x + 1.7135 * y + 0.0367 * z,
            0.0389 * x - 0.0685 * y + 1.0296 * z,
        ]
    };
    let src = to_lms(white[0], white[1], white[2]);
    let dst = to_lms(D65[0], D65[1], D65[2]);
    let [l, m, s] = to_lms(x, y, z);
    let ratio = |d: f64, s: f64| if s != 0.0 { d / s } else { 1.0 };
    let (lp, mp, sp) = (
        l * ratio(dst[0], src[0]),
        m * ratio(dst[1], src[1]),
        s * ratio(dst[2], src[2]),
    );
    // Bradford inverse matrix (LMS → XYZ).
    [
        0.9869929 * lp - 0.1470543 * mp + 0.1599627 * sp,
        0.4323053 * lp + 0.5183603 * mp + 0.0492912 * sp,
        -0.0085287 * lp + 0.0400428 * mp + 0.9684867 * sp,
    ]
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
    fn cal_gray_applies_gamma() {
        // Gamma 1.0 is the identity: 0.5 → grey 128.
        let id = ColorSpace::CalGray { gamma: 1.0 };
        assert_eq!(id.to_rgb(&[0.5], &NoTint), [128, 128, 128]);
        // Gamma 2.2 darkens the midtone: 0.5^2.2 ≈ 0.2176 → ~55.
        let g22 = ColorSpace::CalGray { gamma: 2.2 };
        let v = g22.to_rgb(&[0.5], &NoTint)[0];
        assert!(
            (50..=60).contains(&v),
            "0.5^2.2 grey should be ~55, got {v}"
        );
        // Endpoints are fixed points of any positive gamma.
        assert_eq!(g22.to_rgb(&[0.0], &NoTint), [0, 0, 0]);
        assert_eq!(g22.to_rgb(&[1.0], &NoTint), [255, 255, 255]);
    }

    #[test]
    fn cal_rgb_identity_matrix_is_srgb_with_gamma() {
        // Identity matrix + gamma 1: components are linear sRGB, so the sRGB
        // transfer expands 0.5 to ~188 (matches the Lab/CMYK companding path).
        let cs = ColorSpace::CalRgb {
            gamma: [1.0, 1.0, 1.0],
            matrix: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            white: [0.95047, 1.0, 1.08883],
        };
        let mid = cs.to_rgb(&[0.5, 0.5, 0.5], &NoTint);
        assert!(
            (185..=192).contains(&mid[0]) && mid[0] == mid[1] && mid[1] == mid[2],
            "linear 0.5 → sRGB ~188 grey, got {mid:?}"
        );
        // Pure primaries stay saturated and ordered (red channel dominates red).
        let red = cs.to_rgb(&[1.0, 0.0, 0.0], &NoTint);
        assert_eq!(red, [255, 0, 0]);
    }

    #[test]
    fn cal_rgb_gamma_darkens_before_transfer() {
        // Per-channel gamma 2.2 on the green channel decodes 0.5→0.2176 (linear)
        // before sRGB companding, so green ends up darker than the identity case.
        let plain = ColorSpace::CalRgb {
            gamma: [1.0, 1.0, 1.0],
            matrix: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            white: [0.95047, 1.0, 1.08883],
        };
        let gamma = ColorSpace::CalRgb {
            gamma: [2.2, 2.2, 2.2],
            matrix: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            white: [0.95047, 1.0, 1.08883],
        };
        let g_plain = plain.to_rgb(&[0.5, 0.5, 0.5], &NoTint)[1];
        let g_gamma = gamma.to_rgb(&[0.5, 0.5, 0.5], &NoTint)[1];
        assert!(
            g_gamma < g_plain,
            "gamma 2.2 must darken 0.5 ({g_gamma}) vs identity ({g_plain})"
        );
    }

    #[test]
    fn cal_rgb_matrix_maps_through_xyz() {
        // A `/Matrix` mapping the green component onto the D65 white point: input
        // (0,1,0) → XYZ = D65 → near-white sRGB (matrix path, not the identity
        // fast path). Confirms the XYZ→sRGB pipeline runs and stays achromatic.
        let cs = ColorSpace::CalRgb {
            gamma: [1.0, 1.0, 1.0],
            matrix: [
                0.0, 0.0, 0.0, // A column
                0.95047, 1.0, 1.08883, // B column = D65 white
                0.0, 0.0, 0.0, // C column
            ],
            white: [0.95047, 1.0, 1.08883],
        };
        let [r, g, b] = cs.to_rgb(&[0.0, 1.0, 0.0], &NoTint);
        assert!(
            r > 240 && g > 240 && b > 240,
            "B→D65 white must render near-white, got [{r}, {g}, {b}]"
        );
        // The component spread is tiny (achromatic), proving adaptation balanced.
        let spread = r.max(g).max(b) - r.min(g).min(b);
        assert!(
            spread <= 6,
            "white point should be near-neutral, spread {spread}"
        );
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
