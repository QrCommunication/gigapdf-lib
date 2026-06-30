//! Zero-dependency SVG **filter effects** pipeline.
//!
//! Renders the common SVG `filter` primitive set on an offscreen straight-alpha
//! RGBA raster. The flow mirrors the SVG spec: the filtered element is rasterized
//! into the *filter region* (default `-10% -10% 120% 120%` of the element bbox,
//! the `objectBoundingBox` default for `filterUnits`/`primitiveUnits`), the
//! primitive graph is evaluated over named buffers (`in`/`in2`/`result` plus the
//! implicit `SourceGraphic`/`SourceAlpha`/`BackgroundImage`), and the final
//! result is returned as an RGBA raster ready to composite back in place.
//!
//! Implemented primitives: `feGaussianBlur`, `feOffset`, `feFlood`,
//! `feColorMatrix` (`matrix`/`saturate`/`hueRotate`/`luminanceToAlpha`),
//! `feBlend` (normal/multiply/screen/darken/lighten/overlay/…), `feComposite`
//! (over/in/out/atop/xor/arithmetic), `feMerge`/`feMergeNode`, `feImage`,
//! `feTile`, `feComponentTransfer` (table/discrete/linear/gamma per channel),
//! `feDropShadow`, `feMorphology` (erode/dilate), `feDisplacementMap`, and
//! `feTurbulence` (`turbulence`/`fractalNoise`, with `stitchTiles`).
//!
//! The buffer is straight (non-premultiplied) alpha in `0.0..=1.0`. Compositing
//! and blending premultiply internally where the math requires it.

//! Integration note: this module is the complete raster filter engine. Its
//! `pub(crate)` entry points ([`render_filter`], [`collect_filters`],
//! [`filter_url`], [`apply_filter`](super::apply_filter)) are consumed by the
//! SVG walk ([`super::from_element`]) and the document image path when realizing
//! `filter="url(#…)"`: a filtered element's subtree primitives are rasterized
//! into the filter region, the `fe*` graph runs, and the resulting raster is
//! carried on [`super::SvgImage::rasters`] and emitted as an image XObject by
//! [`crate::document::Document::draw_svg_image`].

use crate::content::svg_path::Seg;
use crate::html::css::parse_color;
use crate::html::dom::{Element, Node};

use super::{Fill, Prim};

/// A straight-alpha RGBA raster: row-major, 4 channels (R,G,B,A) in `0.0..=1.0`.
#[derive(Debug, Clone)]
pub(crate) struct Raster {
    pub(crate) w: usize,
    pub(crate) h: usize,
    /// `w*h*4` floats; channel layout `[r, g, b, a]` per pixel, straight alpha.
    pub(crate) px: Vec<f32>,
}

impl Raster {
    fn new(w: usize, h: usize) -> Raster {
        Raster {
            w,
            h,
            px: vec![0.0; w * h * 4],
        }
    }

    #[inline]
    fn idx(&self, x: usize, y: usize) -> usize {
        (y * self.w + x) * 4
    }

    /// Straight-alpha sample at integer `(x, y)`; zero (transparent) out of bounds.
    #[inline]
    fn get(&self, x: i64, y: i64) -> [f32; 4] {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return [0.0; 4];
        }
        let i = self.idx(x as usize, y as usize);
        [self.px[i], self.px[i + 1], self.px[i + 2], self.px[i + 3]]
    }

    #[inline]
    fn set(&mut self, x: usize, y: usize, c: [f32; 4]) {
        let i = self.idx(x, y);
        self.px[i] = c[0];
        self.px[i + 1] = c[1];
        self.px[i + 2] = c[2];
        self.px[i + 3] = c[3];
    }
}

/// The filter region in element/user space plus the device pixel grid it maps to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Region {
    /// User-space origin (min x/y) of the region.
    x: f64,
    y: f64,
    /// User-space size of the region.
    w: f64,
    h: f64,
    /// Device pixels per user unit (x and y), i.e. the raster resolution.
    sx: f64,
    sy: f64,
    /// Raster size in device pixels.
    pw: usize,
    ph: usize,
}

impl Region {
    /// Map a user-space point into device pixel coordinates within the region.
    #[inline]
    fn dev(&self, ux: f64, uy: f64) -> (f64, f64) {
        ((ux - self.x) * self.sx, (uy - self.y) * self.sy)
    }
}

/// A single parsed filter primitive (`fe*`) with its resolved parameters.
#[derive(Debug, Clone)]
enum Fe {
    GaussianBlur {
        sx: f64,
        sy: f64,
    },
    Offset {
        dx: f64,
        dy: f64,
    },
    Flood {
        rgb: [f32; 3],
        a: f32,
    },
    ColorMatrix(ColorMatrix),
    Blend {
        mode: BlendMode,
    },
    Composite(Composite),
    Merge {
        inputs: Vec<Input>,
    },
    /// `feImage` referencing another in-document element id is approximated as a
    /// solid fill of the region (no external fetch — zero-dependency, no I/O).
    Image {
        rgb: [f32; 3],
        a: f32,
    },
    Tile,
    ComponentTransfer {
        r: Transfer,
        g: Transfer,
        b: Transfer,
        a: Transfer,
    },
    DropShadow {
        sx: f64,
        sy: f64,
        dx: f64,
        dy: f64,
        rgb: [f32; 3],
        a: f32,
    },
    Morphology {
        dilate: bool,
        rx: f64,
        ry: f64,
    },
    DisplacementMap {
        scale: f64,
        x_sel: usize,
        y_sel: usize,
    },
    Turbulence {
        base_x: f64,
        base_y: f64,
        octaves: u32,
        seed: i32,
        fractal: bool,
        stitch: bool,
    },
}

/// A primitive's `in`/`in2` input reference.
#[derive(Debug, Clone)]
enum Input {
    SourceGraphic,
    SourceAlpha,
    BackgroundImage,
    /// A named `result` buffer.
    Named(String),
    /// No explicit input: the previous primitive's result (or SourceGraphic for
    /// the first primitive), per the SVG default-input rules.
    Default,
}

/// One parsed primitive plus where its inputs come from and where it stores its
/// result.
#[derive(Debug, Clone)]
struct Node1 {
    fe: Fe,
    in1: Input,
    in2: Input,
    result: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum BlendMode {
    Normal,
    Multiply,
    Screen,
    Darken,
    Lighten,
    Overlay,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
}

#[derive(Debug, Clone, Copy)]
enum Composite {
    Over,
    In,
    Out,
    Atop,
    Xor,
    Arithmetic { k1: f64, k2: f64, k3: f64, k4: f64 },
}

#[derive(Debug, Clone)]
enum ColorMatrix {
    /// Full 5×4 matrix (20 coefficients, row-major; columns R,G,B,A,1).
    Matrix([f32; 20]),
    Saturate(f64),
    HueRotate(f64),
    LuminanceToAlpha,
}

/// A `feComponentTransfer` per-channel transfer function.
#[derive(Debug, Clone)]
enum Transfer {
    Identity,
    Table(Vec<f64>),
    Discrete(Vec<f64>),
    Linear {
        slope: f64,
        intercept: f64,
    },
    Gamma {
        amplitude: f64,
        exponent: f64,
        offset: f64,
    },
}

/// A parsed `<filter>`: its primitive chain and region percentages.
#[derive(Debug, Clone)]
pub(crate) struct Filter {
    nodes: Vec<Node1>,
    /// Region as fractions of the bbox (objectBoundingBox default).
    fx: f64,
    fy: f64,
    fw: f64,
    fh: f64,
    /// True when `filterUnits="userSpaceOnUse"` (x/y/width/height are user units).
    user_space: bool,
}

pub(crate) type Filters = std::collections::BTreeMap<String, Filter>;

/// Walk the SVG subtree collecting every `<filter id=…>` definition.
pub(crate) fn collect_filters(nodes: &[Node], out: &mut Filters) {
    for n in nodes {
        let Node::Element(e) = n else { continue };
        if e.tag == "filter" {
            if let Some(id) = e.attr("id") {
                out.insert(id.to_string(), parse_filter(e));
            }
        }
        collect_filters(&e.children, out);
    }
}

/// The filter id of a `filter="url(#id)"` attribute (or inline `style`).
pub(crate) fn filter_url(e: &Element) -> Option<String> {
    if let Some(u) = e.attr("filter").and_then(extract_url) {
        return Some(u);
    }
    e.attr("style").and_then(|s| {
        for decl in s.split(';') {
            if let Some((k, v)) = decl.split_once(':') {
                if k.trim().eq_ignore_ascii_case("filter") {
                    if let Some(u) = extract_url(v) {
                        return Some(u);
                    }
                }
            }
        }
        None
    })
}

fn extract_url(v: &str) -> Option<String> {
    let inner = v.trim().strip_prefix("url(")?.strip_suffix(')')?;
    Some(
        inner
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .trim_start_matches('#')
            .to_string(),
    )
}

fn parse_filter(e: &Element) -> Filter {
    let user_space = e
        .attr("filterunits")
        .map(|u| u.eq_ignore_ascii_case("userSpaceOnUse"))
        .unwrap_or(false);
    // Region defaults: -10% -10% 120% 120% of the bbox.
    let frac = |name: &str, dflt: f64| e.attr(name).and_then(parse_region_coord).unwrap_or(dflt);
    let mut nodes = Vec::new();
    for c in &e.children {
        let Node::Element(p) = c else { continue };
        if let Some(n) = parse_primitive(p) {
            nodes.push(n);
        }
    }
    Filter {
        nodes,
        fx: frac("x", -0.1),
        fy: frac("y", -0.1),
        fw: frac("width", 1.2),
        fh: frac("height", 1.2),
        user_space,
    }
}

/// A region coordinate: a `%` (→ fraction) or a plain number.
fn parse_region_coord(s: &str) -> Option<f64> {
    let s = s.trim();
    match s.strip_suffix('%') {
        Some(p) => p.trim().parse::<f64>().ok().map(|v| v / 100.0),
        None => parse_num(s),
    }
}

fn parse_primitive(e: &Element) -> Option<Node1> {
    let in1 = parse_input(e.attr("in"));
    let in2 = parse_input(e.attr("in2"));
    let result = e.attr("result").map(str::to_string);
    let fe = match e.tag.as_str() {
        "fegaussianblur" => {
            let (sx, sy) = parse_xy(e.attr("stddeviation"), 0.0);
            Fe::GaussianBlur { sx, sy }
        }
        "feoffset" => Fe::Offset {
            dx: attr_num(e, "dx", 0.0),
            dy: attr_num(e, "dy", 0.0),
        },
        "feflood" => {
            let (rgb, a) = flood_paint(e);
            Fe::Flood { rgb, a }
        }
        "fecolormatrix" => Fe::ColorMatrix(parse_color_matrix(e)),
        "feblend" => Fe::Blend {
            mode: parse_blend_mode(e.attr("mode")),
        },
        "fecomposite" => Fe::Composite(parse_composite(e)),
        "femerge" => {
            let mut inputs = Vec::new();
            for c in &e.children {
                if let Node::Element(m) = c {
                    if m.tag == "femergenode" {
                        inputs.push(parse_input(m.attr("in")));
                    }
                }
            }
            Fe::Merge { inputs }
        }
        "feimage" => {
            // No external resource fetch (zero-dep, no I/O): a referenced
            // element is approximated by an opaque mid-grey fill of the region.
            Fe::Image {
                rgb: [0.5, 0.5, 0.5],
                a: 1.0,
            }
        }
        "fetile" => Fe::Tile,
        "fecomponenttransfer" => {
            let mut r = Transfer::Identity;
            let mut g = Transfer::Identity;
            let mut b = Transfer::Identity;
            let mut a = Transfer::Identity;
            for c in &e.children {
                if let Node::Element(f) = c {
                    let t = parse_transfer(f);
                    match f.tag.as_str() {
                        "fefuncr" => r = t,
                        "fefuncg" => g = t,
                        "fefuncb" => b = t,
                        "fefunca" => a = t,
                        _ => {}
                    }
                }
            }
            Fe::ComponentTransfer { r, g, b, a }
        }
        "fedropshadow" => {
            let (sx, sy) = parse_xy(e.attr("stddeviation"), 2.0);
            let (rgb, a) = flood_paint(e);
            Fe::DropShadow {
                sx,
                sy,
                dx: attr_num(e, "dx", 2.0),
                dy: attr_num(e, "dy", 2.0),
                rgb,
                a,
            }
        }
        "femorphology" => {
            let dilate = e
                .attr("operator")
                .map(|o| o.eq_ignore_ascii_case("dilate"))
                .unwrap_or(false);
            let (rx, ry) = parse_xy(e.attr("radius"), 0.0);
            Fe::Morphology { dilate, rx, ry }
        }
        "fedisplacementmap" => Fe::DisplacementMap {
            scale: attr_num(e, "scale", 0.0),
            x_sel: channel_sel(e.attr("xchannelselector"), 0),
            y_sel: channel_sel(e.attr("ychannelselector"), 1),
        },
        "feturbulence" => {
            let (base_x, base_y) = parse_xy(e.attr("basefrequency"), 0.0);
            Fe::Turbulence {
                base_x,
                base_y,
                octaves: attr_num(e, "numoctaves", 1.0).max(0.0) as u32,
                seed: attr_num(e, "seed", 0.0) as i32,
                fractal: e
                    .attr("type")
                    .map(|t| t.eq_ignore_ascii_case("fractalNoise"))
                    .unwrap_or(false),
                stitch: e
                    .attr("stitchtiles")
                    .map(|s| s.eq_ignore_ascii_case("stitch"))
                    .unwrap_or(false),
            }
        }
        _ => return None,
    };
    Some(Node1 {
        fe,
        in1,
        in2,
        result,
    })
}

fn parse_input(v: Option<&str>) -> Input {
    match v.map(str::trim) {
        None | Some("") => Input::Default,
        Some(s) if s.eq_ignore_ascii_case("SourceGraphic") => Input::SourceGraphic,
        Some(s) if s.eq_ignore_ascii_case("SourceAlpha") => Input::SourceAlpha,
        Some(s) if s.eq_ignore_ascii_case("BackgroundImage") => Input::BackgroundImage,
        Some(s) if s.eq_ignore_ascii_case("BackgroundAlpha") => Input::BackgroundImage,
        Some(s) if s.eq_ignore_ascii_case("FillPaint") || s.eq_ignore_ascii_case("StrokePaint") => {
            Input::SourceGraphic
        }
        Some(s) => Input::Named(s.to_string()),
    }
}

/// `feFlood`/`feDropShadow` colour: `flood-color` + `flood-opacity`.
fn flood_paint(e: &Element) -> ([f32; 3], f32) {
    let mut rgb = e
        .attr("flood-color")
        .and_then(parse_color)
        .map(|c| [c[0] as f32, c[1] as f32, c[2] as f32])
        .unwrap_or([0.0, 0.0, 0.0]);
    let mut a = e
        .attr("flood-opacity")
        .and_then(parse_num)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0) as f32;
    if let Some(style) = e.attr("style") {
        for decl in style.split(';') {
            if let Some((k, v)) = decl.split_once(':') {
                match k.trim().to_ascii_lowercase().as_str() {
                    "flood-color" => {
                        if let Some(c) = parse_color(v.trim()) {
                            rgb = [c[0] as f32, c[1] as f32, c[2] as f32];
                        }
                    }
                    "flood-opacity" => {
                        if let Some(o) = parse_num(v) {
                            a = o.clamp(0.0, 1.0) as f32;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    (rgb, a)
}

fn parse_color_matrix(e: &Element) -> ColorMatrix {
    let ty = e.attr("type").map(str::to_ascii_lowercase);
    match ty.as_deref() {
        Some("saturate") => ColorMatrix::Saturate(attr_num(e, "values", 1.0)),
        Some("huerotate") => ColorMatrix::HueRotate(attr_num(e, "values", 0.0)),
        Some("luminancetoalpha") => ColorMatrix::LuminanceToAlpha,
        _ => {
            let v = e.attr("values").map(parse_num_list).unwrap_or_default();
            if v.len() == 20 {
                let mut m = [0.0f32; 20];
                for (i, x) in v.iter().enumerate() {
                    m[i] = *x as f32;
                }
                ColorMatrix::Matrix(m)
            } else {
                // Identity matrix.
                let mut m = [0.0f32; 20];
                m[0] = 1.0;
                m[6] = 1.0;
                m[12] = 1.0;
                m[18] = 1.0;
                ColorMatrix::Matrix(m)
            }
        }
    }
}

fn parse_blend_mode(v: Option<&str>) -> BlendMode {
    match v.map(str::to_ascii_lowercase).as_deref() {
        Some("multiply") => BlendMode::Multiply,
        Some("screen") => BlendMode::Screen,
        Some("darken") => BlendMode::Darken,
        Some("lighten") => BlendMode::Lighten,
        Some("overlay") => BlendMode::Overlay,
        Some("color-dodge") => BlendMode::ColorDodge,
        Some("color-burn") => BlendMode::ColorBurn,
        Some("hard-light") => BlendMode::HardLight,
        Some("soft-light") => BlendMode::SoftLight,
        Some("difference") => BlendMode::Difference,
        Some("exclusion") => BlendMode::Exclusion,
        _ => BlendMode::Normal,
    }
}

fn parse_composite(e: &Element) -> Composite {
    match e.attr("operator").map(str::to_ascii_lowercase).as_deref() {
        Some("in") => Composite::In,
        Some("out") => Composite::Out,
        Some("atop") => Composite::Atop,
        Some("xor") => Composite::Xor,
        Some("arithmetic") => Composite::Arithmetic {
            k1: attr_num(e, "k1", 0.0),
            k2: attr_num(e, "k2", 0.0),
            k3: attr_num(e, "k3", 0.0),
            k4: attr_num(e, "k4", 0.0),
        },
        _ => Composite::Over,
    }
}

fn parse_transfer(e: &Element) -> Transfer {
    match e.attr("type").map(str::to_ascii_lowercase).as_deref() {
        Some("table") => Transfer::Table(
            e.attr("tablevalues")
                .map(parse_num_list)
                .unwrap_or_default(),
        ),
        Some("discrete") => Transfer::Discrete(
            e.attr("tablevalues")
                .map(parse_num_list)
                .unwrap_or_default(),
        ),
        Some("linear") => Transfer::Linear {
            slope: attr_num(e, "slope", 1.0),
            intercept: attr_num(e, "intercept", 0.0),
        },
        Some("gamma") => Transfer::Gamma {
            amplitude: attr_num(e, "amplitude", 1.0),
            exponent: attr_num(e, "exponent", 1.0),
            offset: attr_num(e, "offset", 0.0),
        },
        _ => Transfer::Identity,
    }
}

fn channel_sel(v: Option<&str>, dflt: usize) -> usize {
    match v.map(str::trim) {
        Some("R") => 0,
        Some("G") => 1,
        Some("B") => 2,
        Some("A") => 3,
        _ => dflt,
    }
}

// ── public entry point ──────────────────────────────────────────────────────────

/// Render the filter `id` applied to `prims` (already in user/viewBox space).
/// Returns the filtered RGBA raster together with the user-space rectangle it
/// covers (so a caller can composite it back in place). `None` if the filter id
/// is unknown or the geometry is degenerate.
///
/// `device_scale` is the device pixels-per-user-unit the raster is built at; a
/// caller wanting crisp output at a known placement should pass the placement
/// resolution. It is clamped so the buffer stays bounded.
pub(crate) fn render_filter(
    id: &str,
    prims: &[Prim],
    filters: &Filters,
    device_scale: f64,
) -> Option<(Raster, [f64; 4])> {
    let filter = filters.get(id)?;
    let bbox = prims_bbox(prims)?;
    let region = compute_region(filter, bbox, device_scale)?;

    // Implicit inputs.
    let source = rasterize_prims(prims, &region, false);
    let source_alpha = rasterize_prims(prims, &region, true);

    let mut named: std::collections::HashMap<String, Raster> = std::collections::HashMap::new();
    let mut last: Option<Raster> = None;

    let resolve = |inp: &Input,
                   last: &Option<Raster>,
                   named: &std::collections::HashMap<String, Raster>|
     -> Raster {
        match inp {
            Input::SourceGraphic => source.clone(),
            Input::SourceAlpha => source_alpha.clone(),
            // No backdrop is tracked: BackgroundImage is empty (transparent).
            Input::BackgroundImage => Raster::new(region.pw, region.ph),
            Input::Named(n) => named
                .get(n)
                .cloned()
                .unwrap_or_else(|| Raster::new(region.pw, region.ph)),
            Input::Default => last.clone().unwrap_or_else(|| source.clone()),
        }
    };

    for node in &filter.nodes {
        let a = resolve(&node.in1, &last, &named);
        let out = match &node.fe {
            Fe::GaussianBlur { sx, sy } => gaussian_blur(&a, *sx * region.sx, *sy * region.sy),
            Fe::Offset { dx, dy } => offset(
                &a,
                (*dx * region.sx).round() as i64,
                (*dy * region.sy).round() as i64,
            ),
            Fe::Flood { rgb, a: fa } => flood(&region, *rgb, *fa),
            Fe::ColorMatrix(cm) => color_matrix(&a, cm),
            Fe::Blend { mode } => {
                let b = resolve(&node.in2, &last, &named);
                blend(&a, &b, *mode)
            }
            Fe::Composite(op) => {
                let b = resolve(&node.in2, &last, &named);
                composite(&a, &b, *op)
            }
            Fe::Merge { inputs } => {
                let mut acc = Raster::new(region.pw, region.ph);
                for inp in inputs {
                    let layer = resolve(inp, &last, &named);
                    acc = composite(&layer, &acc, Composite::Over);
                }
                acc
            }
            Fe::Image { rgb, a: ia } => flood(&region, *rgb, *ia),
            Fe::Tile => a, // single tile spans the region already
            Fe::ComponentTransfer { r, g, b, a: ta } => component_transfer(&a, r, g, b, ta),
            Fe::DropShadow {
                sx,
                sy,
                dx,
                dy,
                rgb,
                a: sa,
            } => drop_shadow(&a, &region, *sx, *sy, *dx, *dy, *rgb, *sa),
            Fe::Morphology { dilate, rx, ry } => morphology(
                &a,
                *dilate,
                (*rx * region.sx).round().max(0.0) as usize,
                (*ry * region.sy).round().max(0.0) as usize,
            ),
            Fe::DisplacementMap {
                scale,
                x_sel,
                y_sel,
            } => {
                let b = resolve(&node.in2, &last, &named);
                displacement_map(&a, &b, *scale * region.sx, *x_sel, *y_sel)
            }
            Fe::Turbulence {
                base_x,
                base_y,
                octaves,
                seed,
                fractal,
                stitch,
            } => turbulence(
                &region, *base_x, *base_y, *octaves, *seed, *fractal, *stitch,
            ),
        };
        if let Some(name) = &node.result {
            named.insert(name.clone(), out.clone());
        }
        last = Some(out);
    }

    let result = last.unwrap_or(source);
    Some((result, [region.x, region.y, region.w, region.h]))
}

/// Compute the device-resolution filter region for `bbox` (user space).
fn compute_region(filter: &Filter, bbox: [f64; 4], device_scale: f64) -> Option<Region> {
    let [minx, miny, maxx, maxy] = bbox;
    let (bw, bh) = (maxx - minx, maxy - miny);
    if bw <= 0.0 || bh <= 0.0 {
        return None;
    }
    let (x, y, w, h) = if filter.user_space {
        (filter.fx, filter.fy, filter.fw, filter.fh)
    } else {
        (
            minx + filter.fx * bw,
            miny + filter.fy * bh,
            filter.fw * bw,
            filter.fh * bh,
        )
    };
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    // Clamp the device scale and the resulting buffer so memory stays bounded.
    let scale = device_scale.clamp(0.25, 8.0);
    let pw = ((w * scale).ceil() as usize).clamp(1, 2048);
    let ph = ((h * scale).ceil() as usize).clamp(1, 2048);
    Some(Region {
        x,
        y,
        w,
        h,
        sx: pw as f64 / w,
        sy: ph as f64 / h,
        pw,
        ph,
    })
}

fn prims_bbox(prims: &[Prim]) -> Option<[f64; 4]> {
    let (mut nx, mut ny, mut xx, mut xy) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    let mut upd = |x: f64, y: f64| {
        nx = nx.min(x);
        ny = ny.min(y);
        xx = xx.max(x);
        xy = xy.max(y);
    };
    for p in prims {
        for s in &p.segs {
            match *s {
                Seg::Move(x, y) | Seg::Line(x, y) => upd(x, y),
                Seg::Cubic(a, b, c, d, e, f) => {
                    upd(a, b);
                    upd(c, d);
                    upd(e, f);
                }
                Seg::Close => {}
            }
        }
    }
    if nx > xx {
        None
    } else {
        Some([nx, ny, xx, xy])
    }
}

// ── source rasterization (Prim → RGBA over the region) ──────────────────────────

/// Rasterize `prims` into a fresh region-sized RGBA raster. When `alpha_only`,
/// every pixel's RGB is forced to black and only the coverage drives alpha
/// (SourceAlpha). Anti-aliasing uses 4× supersampled coverage per pixel.
fn rasterize_prims(prims: &[Prim], region: &Region, alpha_only: bool) -> Raster {
    let mut out = Raster::new(region.pw, region.ph);
    for prim in prims {
        let Some((rgb, a)) = prim_color(prim) else {
            continue;
        };
        let polys = flatten_prim(prim, region);
        if polys.is_empty() {
            continue;
        }
        // Coverage accumulation per pixel for this primitive, then composite OVER.
        let cov = coverage(&polys, region.pw, region.ph);
        for y in 0..region.ph {
            for x in 0..region.pw {
                let c = cov[y * region.pw + x];
                if c <= 0.0 {
                    continue;
                }
                let src_a = a * c;
                let src_rgb = if alpha_only { [0.0, 0.0, 0.0] } else { rgb };
                let dst = out.get(x as i64, y as i64);
                out.set(
                    x,
                    y,
                    over_straight([src_rgb[0], src_rgb[1], src_rgb[2], src_a], dst),
                );
            }
        }
    }
    out
}

/// A primitive's flat fill colour and alpha; `None` for stroke-only / no fill
/// (filters operate on the painted result, but the SVG renderer's gradients are
/// approximated here by their first stop — filters are a coarse raster anyway).
fn prim_color(prim: &Prim) -> Option<([f32; 3], f32)> {
    let a = prim.fill_opacity.clamp(0.0, 1.0) as f32;
    match &prim.fill {
        Some(Fill::Solid(c)) => Some(([c[0] as f32, c[1] as f32, c[2] as f32], a)),
        Some(Fill::Gradient(g)) => {
            // Average the stops to a representative flat colour.
            if g.stops.is_empty() {
                return None;
            }
            let n = g.stops.len() as f32;
            let mut rgb = [0.0f32; 3];
            let mut sa = 0.0f32;
            for s in &g.stops {
                rgb[0] += s.rgb[0] as f32;
                rgb[1] += s.rgb[1] as f32;
                rgb[2] += s.rgb[2] as f32;
                sa += s.alpha as f32;
            }
            Some(([rgb[0] / n, rgb[1] / n, rgb[2] / n], a * (sa / n)))
        }
        None => {
            // Stroke-only: approximate with the stroke colour painting its bbox.
            prim.stroke.map(|c| {
                (
                    [c[0] as f32, c[1] as f32, c[2] as f32],
                    prim.stroke_opacity as f32,
                )
            })
        }
    }
}

/// Flatten a primitive's segments into device-space closed polygons.
fn flatten_prim(prim: &Prim, region: &Region) -> Vec<Vec<(f64, f64)>> {
    let mut polys: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut cur: Vec<(f64, f64)> = Vec::new();
    let mut last = (0.0f64, 0.0f64);
    let mut start = (0.0f64, 0.0f64);
    for s in &prim.segs {
        match *s {
            Seg::Move(x, y) => {
                if cur.len() >= 2 {
                    polys.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
                last = (x, y);
                start = (x, y);
                cur.push(region.dev(x, y));
            }
            Seg::Line(x, y) => {
                last = (x, y);
                cur.push(region.dev(x, y));
            }
            Seg::Cubic(x1, y1, x2, y2, x3, y3) => {
                flatten_cubic(last, (x1, y1), (x2, y2), (x3, y3), region, &mut cur);
                last = (x3, y3);
            }
            Seg::Close => {
                cur.push(region.dev(start.0, start.1));
                if cur.len() >= 2 {
                    polys.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
                last = start;
            }
        }
    }
    if cur.len() >= 2 {
        polys.push(cur);
    }
    polys
}

/// Adaptive (fixed-depth) cubic flattening into device-space points.
fn flatten_cubic(
    p0: (f64, f64),
    p1: (f64, f64),
    p2: (f64, f64),
    p3: (f64, f64),
    region: &Region,
    out: &mut Vec<(f64, f64)>,
) {
    const STEPS: usize = 16;
    for i in 1..=STEPS {
        let t = i as f64 / STEPS as f64;
        let mt = 1.0 - t;
        let x = mt * mt * mt * p0.0
            + 3.0 * mt * mt * t * p1.0
            + 3.0 * mt * t * t * p2.0
            + t * t * t * p3.0;
        let y = mt * mt * mt * p0.1
            + 3.0 * mt * mt * t * p1.1
            + 3.0 * mt * t * t * p2.1
            + t * t * t * p3.1;
        out.push(region.dev(x, y));
    }
}

/// 4× supersampled even-odd coverage of `polys` over a `w×h` pixel grid.
fn coverage(polys: &[Vec<(f64, f64)>], w: usize, h: usize) -> Vec<f32> {
    let mut cov = vec![0.0f32; w * h];
    if polys.is_empty() {
        return cov;
    }
    const SS: usize = 2; // 2×2 samples per pixel → 4 coverage levels
    let step = 1.0 / SS as f64;
    // Build edges (device space) once.
    let mut edges: Vec<(f64, f64, f64, f64)> = Vec::new();
    for poly in polys {
        for i in 0..poly.len() {
            let (x0, y0) = poly[i];
            let (x1, y1) = poly[(i + 1) % poly.len()];
            if (y0 - y1).abs() > f64::EPSILON {
                edges.push((x0, y0, x1, y1));
            }
        }
    }
    if edges.is_empty() {
        return cov;
    }
    for py in 0..h {
        for sy in 0..SS {
            let yc = py as f64 + (sy as f64 + 0.5) * step;
            // Find x crossings on this scanline (nonzero winding).
            let mut xs: Vec<(f64, i32)> = Vec::new();
            for &(x0, y0, x1, y1) in &edges {
                let (ymin, ymax) = if y0 < y1 { (y0, y1) } else { (y1, y0) };
                if yc < ymin || yc >= ymax {
                    continue;
                }
                let t = (yc - y0) / (y1 - y0);
                let x = x0 + t * (x1 - x0);
                let dir = if y1 > y0 { 1 } else { -1 };
                xs.push((x, dir));
            }
            if xs.is_empty() {
                continue;
            }
            xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            // Walk spans where the winding number is non-zero.
            let mut wind = 0i32;
            for i in 0..xs.len().saturating_sub(1) {
                wind += xs[i].1;
                if wind == 0 {
                    continue;
                }
                let span0 = xs[i].0;
                let span1 = xs[i + 1].0;
                for px in 0..w {
                    for sx in 0..SS {
                        let xc = px as f64 + (sx as f64 + 0.5) * step;
                        if xc >= span0 && xc < span1 {
                            cov[py * w + px] += 1.0 / (SS * SS) as f32;
                        }
                    }
                }
            }
        }
    }
    for c in &mut cov {
        *c = c.min(1.0);
    }
    cov
}

// ── compositing helpers (straight alpha) ────────────────────────────────────────

/// `src OVER dst`, both straight alpha; returns straight-alpha result.
#[inline]
fn over_straight(src: [f32; 4], dst: [f32; 4]) -> [f32; 4] {
    let sa = src[3];
    let da = dst[3];
    let oa = sa + da * (1.0 - sa);
    if oa <= 0.0 {
        return [0.0; 4];
    }
    let mut out = [0.0f32; 4];
    for c in 0..3 {
        // Premultiplied OVER, then un-premultiply.
        let s = src[c] * sa;
        let d = dst[c] * da;
        out[c] = (s + d * (1.0 - sa)) / oa;
    }
    out[3] = oa;
    out
}

// ── primitive implementations ───────────────────────────────────────────────────

/// Separable Gaussian via three successive box blurs (the standard ≈Gaussian
/// approximation also used for box-shadow). `sx`/`sy` are std-devs in *pixels*.
fn gaussian_blur(src: &Raster, sx: f64, sy: f64) -> Raster {
    let mut r = src.clone();
    if sx > 0.0 {
        let boxes = box_sizes_for_sigma(sx);
        for &b in &boxes {
            r = box_blur_h(&r, b);
        }
    }
    if sy > 0.0 {
        let boxes = box_sizes_for_sigma(sy);
        for &b in &boxes {
            r = box_blur_v(&r, b);
        }
    }
    r
}

/// Three box-blur radii whose repeated application approximates a Gaussian of
/// the given sigma (per the SVG filter spec's recommended box sizes).
fn box_sizes_for_sigma(sigma: f64) -> [usize; 3] {
    // d = floor(sigma * 3 * sqrt(2*pi)/4 + 0.5)
    let d = (sigma * 3.0 * (2.0 * std::f64::consts::PI).sqrt() / 4.0 + 0.5).floor() as i64;
    let d = d.max(1);
    if d % 2 == 1 {
        let r = (d as usize) / 2;
        [r, r, r]
    } else {
        // even: one box one wider on one side; approximate with radii d/2.
        let r = (d as usize) / 2;
        [r, r, r.max(1)]
    }
}

/// Premultiply, average over a horizontal window of half-width `r`, un-premultiply.
fn box_blur_h(src: &Raster, r: usize) -> Raster {
    if r == 0 {
        return src.clone();
    }
    let mut out = Raster::new(src.w, src.h);
    let win = (2 * r + 1) as f32;
    for y in 0..src.h {
        for x in 0..src.w {
            let mut acc = [0.0f32; 4];
            for dx in -(r as i64)..=(r as i64) {
                let s = src.get(x as i64 + dx, y as i64);
                let a = s[3];
                acc[0] += s[0] * a;
                acc[1] += s[1] * a;
                acc[2] += s[2] * a;
                acc[3] += a;
            }
            let oa = acc[3] / win;
            let out_px = if oa <= 0.0 {
                [0.0; 4]
            } else {
                [acc[0] / win / oa, acc[1] / win / oa, acc[2] / win / oa, oa]
            };
            out.set(x, y, out_px);
        }
    }
    out
}

/// Vertical counterpart of [`box_blur_h`].
fn box_blur_v(src: &Raster, r: usize) -> Raster {
    if r == 0 {
        return src.clone();
    }
    let mut out = Raster::new(src.w, src.h);
    let win = (2 * r + 1) as f32;
    for y in 0..src.h {
        for x in 0..src.w {
            let mut acc = [0.0f32; 4];
            for dy in -(r as i64)..=(r as i64) {
                let s = src.get(x as i64, y as i64 + dy);
                let a = s[3];
                acc[0] += s[0] * a;
                acc[1] += s[1] * a;
                acc[2] += s[2] * a;
                acc[3] += a;
            }
            let oa = acc[3] / win;
            let out_px = if oa <= 0.0 {
                [0.0; 4]
            } else {
                [acc[0] / win / oa, acc[1] / win / oa, acc[2] / win / oa, oa]
            };
            out.set(x, y, out_px);
        }
    }
    out
}

/// Translate the raster by `(dx, dy)` device pixels (transparent fill).
fn offset(src: &Raster, dx: i64, dy: i64) -> Raster {
    let mut out = Raster::new(src.w, src.h);
    for y in 0..src.h {
        for x in 0..src.w {
            let s = src.get(x as i64 - dx, y as i64 - dy);
            out.set(x, y, s);
        }
    }
    out
}

/// Fill the whole region with a flat colour at alpha `a`.
fn flood(region: &Region, rgb: [f32; 3], a: f32) -> Raster {
    let mut out = Raster::new(region.pw, region.ph);
    for y in 0..region.ph {
        for x in 0..region.pw {
            out.set(x, y, [rgb[0], rgb[1], rgb[2], a]);
        }
    }
    out
}

/// `feColorMatrix` — operates on premultiplied? No: SVG color-matrix is defined
/// on **non-premultiplied** RGBA. We apply per pixel on straight values.
fn color_matrix(src: &Raster, cm: &ColorMatrix) -> Raster {
    let m = match cm {
        ColorMatrix::Matrix(m) => *m,
        ColorMatrix::Saturate(s) => saturate_matrix(*s as f32),
        ColorMatrix::HueRotate(deg) => hue_rotate_matrix(*deg as f32),
        ColorMatrix::LuminanceToAlpha => {
            // RGB→0, A = 0.2125 R + 0.7154 G + 0.0721 B.
            let mut m = [0.0f32; 20];
            m[15] = 0.2125;
            m[16] = 0.7154;
            m[17] = 0.0721;
            m
        }
    };
    let mut out = Raster::new(src.w, src.h);
    for y in 0..src.h {
        for x in 0..src.w {
            let p = src.get(x as i64, y as i64);
            let (r, g, b, a) = (p[0], p[1], p[2], p[3]);
            let nr = m[0] * r + m[1] * g + m[2] * b + m[3] * a + m[4];
            let ng = m[5] * r + m[6] * g + m[7] * b + m[8] * a + m[9];
            let nb = m[10] * r + m[11] * g + m[12] * b + m[13] * a + m[14];
            let na = m[15] * r + m[16] * g + m[17] * b + m[18] * a + m[19];
            out.set(
                x,
                y,
                [
                    nr.clamp(0.0, 1.0),
                    ng.clamp(0.0, 1.0),
                    nb.clamp(0.0, 1.0),
                    na.clamp(0.0, 1.0),
                ],
            );
        }
    }
    out
}

/// The 5×4 saturate matrix for saturation `s`.
fn saturate_matrix(s: f32) -> [f32; 20] {
    [
        0.213 + 0.787 * s,
        0.715 - 0.715 * s,
        0.072 - 0.072 * s,
        0.0,
        0.0,
        0.213 - 0.213 * s,
        0.715 + 0.285 * s,
        0.072 - 0.072 * s,
        0.0,
        0.0,
        0.213 - 0.213 * s,
        0.715 - 0.715 * s,
        0.072 + 0.928 * s,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
    ]
}

/// The 5×4 hue-rotate matrix for `deg` degrees.
fn hue_rotate_matrix(deg: f32) -> [f32; 20] {
    let (s, c) = deg.to_radians().sin_cos();
    [
        0.213 + c * 0.787 - s * 0.213,
        0.715 - c * 0.715 - s * 0.715,
        0.072 - c * 0.072 + s * 0.928,
        0.0,
        0.0,
        0.213 - c * 0.213 + s * 0.143,
        0.715 + c * 0.285 + s * 0.140,
        0.072 - c * 0.072 - s * 0.283,
        0.0,
        0.0,
        0.213 - c * 0.213 - s * 0.787,
        0.715 - c * 0.715 + s * 0.715,
        0.072 + c * 0.928 + s * 0.072,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
    ]
}

/// `feBlend` — separable blend modes over premultiplied-then-restored colours.
fn blend(a: &Raster, b: &Raster, mode: BlendMode) -> Raster {
    let (w, h) = (a.w.max(b.w), a.h.max(b.h));
    let mut out = Raster::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let s = a.get(x as i64, y as i64); // top
            let d = b.get(x as i64, y as i64); // bottom
            let (sa, da) = (s[3], d[3]);
            let ra = sa + da * (1.0 - sa);
            let mut col = [0.0f32; 4];
            for c in 0..3 {
                let cs = s[c];
                let cb = d[c];
                let f = blend_channel(mode, cs, cb);
                // Spec: result_c = (1-da)*sa*cs + (1-sa)*da*cb + sa*da*B(cs,cb)
                let prem = (1.0 - da) * sa * cs + (1.0 - sa) * da * cb + sa * da * f;
                col[c] = if ra > 0.0 { prem / ra } else { 0.0 };
            }
            col[3] = ra;
            out.set(x, y, col);
        }
    }
    out
}

#[inline]
fn blend_channel(mode: BlendMode, cs: f32, cb: f32) -> f32 {
    match mode {
        BlendMode::Normal => cs,
        BlendMode::Multiply => cs * cb,
        BlendMode::Screen => cs + cb - cs * cb,
        BlendMode::Darken => cs.min(cb),
        BlendMode::Lighten => cs.max(cb),
        BlendMode::Overlay => hard_light(cb, cs),
        BlendMode::HardLight => hard_light(cs, cb),
        BlendMode::ColorDodge => {
            if cs >= 1.0 {
                1.0
            } else {
                (cb / (1.0 - cs)).min(1.0)
            }
        }
        BlendMode::ColorBurn => {
            if cs <= 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - cb) / cs).min(1.0)
            }
        }
        BlendMode::SoftLight => {
            if cs <= 0.5 {
                cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb)
            } else {
                let d = if cb <= 0.25 {
                    ((16.0 * cb - 12.0) * cb + 4.0) * cb
                } else {
                    cb.sqrt()
                };
                cb + (2.0 * cs - 1.0) * (d - cb)
            }
        }
        BlendMode::Difference => (cs - cb).abs(),
        BlendMode::Exclusion => cs + cb - 2.0 * cs * cb,
    }
}

#[inline]
fn hard_light(cs: f32, cb: f32) -> f32 {
    if cs <= 0.5 {
        2.0 * cs * cb
    } else {
        1.0 - 2.0 * (1.0 - cs) * (1.0 - cb)
    }
}

/// `feComposite` Porter-Duff + arithmetic, on premultiplied colours.
fn composite(a: &Raster, b: &Raster, op: Composite) -> Raster {
    let (w, h) = (a.w.max(b.w), a.h.max(b.h));
    let mut out = Raster::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let s = a.get(x as i64, y as i64);
            let d = b.get(x as i64, y as i64);
            let (sa, da) = (s[3], d[3]);
            let (fa, fb) = match op {
                Composite::Over => (1.0, 1.0 - sa),
                Composite::In => (da, 0.0),
                Composite::Out => (1.0 - da, 0.0),
                Composite::Atop => (da, 1.0 - sa),
                Composite::Xor => (1.0 - da, 1.0 - sa),
                Composite::Arithmetic { .. } => (0.0, 0.0), // handled below
            };
            let mut col = [0.0f32; 4];
            if let Composite::Arithmetic { k1, k2, k3, k4 } = op {
                let (k1, k2, k3, k4) = (k1 as f32, k2 as f32, k3 as f32, k4 as f32);
                // Arithmetic is defined on premultiplied values.
                let oa = (k1 * sa * da + k2 * sa + k3 * da + k4).clamp(0.0, 1.0);
                for c in 0..3 {
                    let i = s[c] * sa;
                    let j = d[c] * da;
                    let r = (k1 * i * j + k2 * i + k3 * j + k4).clamp(0.0, 1.0);
                    col[c] = if oa > 0.0 {
                        (r / oa).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                }
                col[3] = oa;
            } else {
                let oa = sa * fa + da * fb;
                for c in 0..3 {
                    let prem = s[c] * sa * fa + d[c] * da * fb;
                    col[c] = if oa > 0.0 {
                        (prem / oa).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                }
                col[3] = oa.clamp(0.0, 1.0);
            }
            out.set(x, y, col);
        }
    }
    out
}

/// `feComponentTransfer` — apply a per-channel transfer function.
fn component_transfer(
    src: &Raster,
    fr: &Transfer,
    fg: &Transfer,
    fb: &Transfer,
    fa: &Transfer,
) -> Raster {
    let mut out = Raster::new(src.w, src.h);
    for y in 0..src.h {
        for x in 0..src.w {
            let p = src.get(x as i64, y as i64);
            out.set(
                x,
                y,
                [
                    apply_transfer(fr, p[0]),
                    apply_transfer(fg, p[1]),
                    apply_transfer(fb, p[2]),
                    apply_transfer(fa, p[3]),
                ],
            );
        }
    }
    out
}

fn apply_transfer(t: &Transfer, c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0) as f64;
    let v = match t {
        Transfer::Identity => c,
        Transfer::Table(vals) => {
            if vals.is_empty() {
                c
            } else if vals.len() == 1 {
                vals[0]
            } else {
                let n = vals.len() - 1;
                let k = (c * n as f64).floor().min((n - 1) as f64) as usize;
                let t = c * n as f64 - k as f64;
                vals[k] + t * (vals[k + 1] - vals[k])
            }
        }
        Transfer::Discrete(vals) => {
            if vals.is_empty() {
                c
            } else {
                let n = vals.len();
                let k = ((c * n as f64).floor() as usize).min(n - 1);
                vals[k]
            }
        }
        Transfer::Linear { slope, intercept } => slope * c + intercept,
        Transfer::Gamma {
            amplitude,
            exponent,
            offset,
        } => amplitude * c.powf(*exponent) + offset,
    };
    v.clamp(0.0, 1.0) as f32
}

/// `feDropShadow` — `SourceAlpha` → blur → offset → flood-tint → composite the
/// shadow *under* the source graphic.
#[allow(clippy::too_many_arguments)]
fn drop_shadow(
    src: &Raster,
    region: &Region,
    sx: f64,
    sy: f64,
    dx: f64,
    dy: f64,
    rgb: [f32; 3],
    a: f32,
) -> Raster {
    // Alpha of the source.
    let mut alpha = Raster::new(src.w, src.h);
    for y in 0..src.h {
        for x in 0..src.w {
            let p = src.get(x as i64, y as i64);
            alpha.set(x, y, [0.0, 0.0, 0.0, p[3]]);
        }
    }
    let blurred = gaussian_blur(&alpha, sx * region.sx, sy * region.sy);
    let shifted = offset(
        &blurred,
        (dx * region.sx).round() as i64,
        (dy * region.sy).round() as i64,
    );
    // Tint with the shadow colour, scaling alpha by flood-opacity.
    let mut shadow = Raster::new(src.w, src.h);
    for y in 0..src.h {
        for x in 0..src.w {
            let p = shifted.get(x as i64, y as i64);
            shadow.set(x, y, [rgb[0], rgb[1], rgb[2], p[3] * a]);
        }
    }
    // Source OVER shadow.
    composite(src, &shadow, Composite::Over)
}

/// `feMorphology` erode/dilate over a `(rx, ry)` device-pixel window (min/max of
/// each channel, applied to premultiplied-equivalent straight values).
fn morphology(src: &Raster, dilate: bool, rx: usize, ry: usize) -> Raster {
    if rx == 0 && ry == 0 {
        return src.clone();
    }
    let mut out = Raster::new(src.w, src.h);
    for y in 0..src.h {
        for x in 0..src.w {
            let mut ext = if dilate { [0.0f32; 4] } else { [1.0f32; 4] };
            for dy in -(ry as i64)..=(ry as i64) {
                for dx in -(rx as i64)..=(rx as i64) {
                    let s = src.get(x as i64 + dx, y as i64 + dy);
                    for c in 0..4 {
                        ext[c] = if dilate {
                            ext[c].max(s[c])
                        } else {
                            ext[c].min(s[c])
                        };
                    }
                }
            }
            out.set(x, y, ext);
        }
    }
    out
}

/// `feDisplacementMap` — displace `src` by the selected channels of `map`,
/// scaled by `scale` device pixels.
fn displacement_map(src: &Raster, map: &Raster, scale: f64, x_sel: usize, y_sel: usize) -> Raster {
    let mut out = Raster::new(src.w, src.h);
    for y in 0..src.h {
        for x in 0..src.w {
            let m = map.get(x as i64, y as i64);
            let dx = scale * (m[x_sel] as f64 - 0.5);
            let dy = scale * (m[y_sel] as f64 - 0.5);
            let sx = (x as f64 + dx).round() as i64;
            let sy = (y as f64 + dy).round() as i64;
            out.set(x, y, src.get(sx, sy));
        }
    }
    out
}

// ── feTurbulence: SVG reference Perlin noise ────────────────────────────────────

const BSIZE: usize = 256;
const BM: i32 = 255;

/// Perlin-noise state per the SVG filter spec's reference implementation.
struct Turb {
    lattice: [i32; BSIZE + BSIZE + 2],
    grad: [[[f64; 2]; BSIZE + BSIZE + 2]; 4],
}

impl Turb {
    // The lattice/gradient init follows the SVG spec's reference pseudo-code,
    // which is index-driven (parallel `lattice`/`grad[k][i]` arrays) — keeping
    // the explicit indices mirrors the spec exactly.
    #[allow(clippy::needless_range_loop)]
    fn new(mut seed: i32) -> Turb {
        fn random(seed: i32) -> i32 {
            const RAND_A: i64 = 16807;
            const RAND_M: i64 = 2147483647;
            const RAND_Q: i64 = 127773;
            const RAND_R: i64 = 2836;
            let mut s = seed as i64;
            let result = RAND_A * (s % RAND_Q) - RAND_R * (s / RAND_Q);
            s = if result <= 0 { result + RAND_M } else { result };
            s as i32
        }
        if seed <= 0 {
            seed = -(seed % (i32::MAX - 1)) + 1;
        }
        if seed > i32::MAX - 1 {
            seed = i32::MAX - 1;
        }
        let mut lattice = [0i32; BSIZE + BSIZE + 2];
        let mut grad = [[[0.0f64; 2]; BSIZE + BSIZE + 2]; 4];
        for k in 0..4 {
            for i in 0..BSIZE {
                if k == 0 {
                    lattice[i] = i as i32;
                }
                for j in 0..2 {
                    seed = random(seed);
                    grad[k][i][j] =
                        ((seed % (BSIZE as i32 + BSIZE as i32)) - BM) as f64 / BM as f64;
                }
                let s = (grad[k][i][0] * grad[k][i][0] + grad[k][i][1] * grad[k][i][1]).sqrt();
                if s > 0.0 {
                    grad[k][i][0] /= s;
                    grad[k][i][1] /= s;
                }
            }
        }
        // Shuffle the lattice.
        let mut i = BSIZE - 1;
        while i > 0 {
            seed = random(seed);
            let j = (seed as usize) % BSIZE;
            lattice.swap(i, j);
            i -= 1;
        }
        for i in 0..BSIZE + 2 {
            lattice[BSIZE + i] = lattice[i];
            for k in 0..4 {
                grad[k][BSIZE + i] = grad[k][i];
            }
        }
        Turb { lattice, grad }
    }

    /// One octave of noise for colour channel `k` at `(x, y)`.
    fn noise2(&self, k: usize, x: f64, y: f64) -> f64 {
        fn s_curve(t: f64) -> f64 {
            t * t * (3.0 - 2.0 * t)
        }
        fn lerp(t: f64, a: f64, b: f64) -> f64 {
            a + t * (b - a)
        }
        let t = x + 4096.0;
        let bx0 = (t as i32) & BM;
        let bx1 = (bx0 + 1) & BM;
        let rx0 = t - t.floor();
        let rx1 = rx0 - 1.0;
        let t = y + 4096.0;
        let by0 = (t as i32) & BM;
        let by1 = (by0 + 1) & BM;
        let ry0 = t - t.floor();
        let ry1 = ry0 - 1.0;
        let i = self.lattice[bx0 as usize] as usize;
        let j = self.lattice[bx1 as usize] as usize;
        let b00 = self.lattice[i + by0 as usize] as usize;
        let b10 = self.lattice[j + by0 as usize] as usize;
        let b01 = self.lattice[i + by1 as usize] as usize;
        let b11 = self.lattice[j + by1 as usize] as usize;
        let sx = s_curve(rx0);
        let sy = s_curve(ry0);
        let u = rx0 * self.grad[k][b00][0] + ry0 * self.grad[k][b00][1];
        let v = rx1 * self.grad[k][b10][0] + ry0 * self.grad[k][b10][1];
        let a = lerp(sx, u, v);
        let u = rx0 * self.grad[k][b01][0] + ry1 * self.grad[k][b01][1];
        let v = rx1 * self.grad[k][b11][0] + ry1 * self.grad[k][b11][1];
        let b = lerp(sx, u, v);
        lerp(sy, a, b)
    }

    /// Fractal sum over `octaves` for channel `k`; `fractal` selects fractalNoise
    /// (signed → biased to `0..1`) vs turbulence (|noise|).
    #[allow(clippy::too_many_arguments)]
    fn turbulence(
        &self,
        k: usize,
        x: f64,
        y: f64,
        base_x: f64,
        base_y: f64,
        octaves: u32,
        fractal: bool,
    ) -> f64 {
        let mut sum = 0.0;
        let mut fx = x * base_x;
        let mut fy = y * base_y;
        let mut ratio = 1.0;
        for _ in 0..octaves.max(1) {
            let n = self.noise2(k, fx, fy);
            sum += (if fractal { n } else { n.abs() }) / ratio;
            fx *= 2.0;
            fy *= 2.0;
            ratio *= 2.0;
        }
        if fractal {
            (sum + 1.0) / 2.0
        } else {
            sum
        }
    }
}

/// `feTurbulence`/`feImage`-free noise generator. `stitch` is accepted; the
/// reference stitching adjusts frequencies/wrap to tile seamlessly across the
/// region — we apply the frequency rounding part (the visible stitch effect) so
/// the option is honoured, while the noise itself stays the reference fractal.
fn turbulence(
    region: &Region,
    base_x: f64,
    base_y: f64,
    octaves: u32,
    seed: i32,
    fractal: bool,
    stitch: bool,
) -> Raster {
    let mut out = Raster::new(region.pw, region.ph);
    let t = Turb::new(seed);
    // Stitch: round base frequencies so an integer number of periods fits the
    // region (the spec's stitch pre-step), keeping tiles seamless.
    let (bx, by) = if stitch {
        let round_freq = |b: f64, len: f64| {
            if b <= 0.0 {
                return b;
            }
            let lo = (len * b).floor().max(1.0) / len;
            let hi = (len * b).ceil() / len;
            if b / lo < hi / b {
                lo
            } else {
                hi
            }
        };
        (round_freq(base_x, region.w), round_freq(base_y, region.h))
    } else {
        (base_x, base_y)
    };
    for py in 0..region.ph {
        for px in 0..region.pw {
            // Noise is sampled in user space (so frequency is per user unit).
            let ux = region.x + (px as f64 + 0.5) / region.sx;
            let uy = region.y + (py as f64 + 0.5) / region.sy;
            let mut col = [0.0f32; 4];
            for (k, ch) in col.iter_mut().enumerate() {
                let n = t.turbulence(k, ux, uy, bx, by, octaves, fractal);
                *ch = (n.clamp(0.0, 1.0)) as f32;
            }
            out.set(px, py, col);
        }
    }
    out
}

// ── small numeric parsers (local to filters) ────────────────────────────────────

fn parse_num(s: &str) -> Option<f64> {
    let s = s.trim();
    let end = s
        .find(|c: char| !matches!(c, '0'..='9' | '.' | '-' | '+' | 'e' | 'E'))
        .unwrap_or(s.len());
    s[..end].parse().ok()
}

fn parse_num_list(s: &str) -> Vec<f64> {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect()
}

fn attr_num(e: &Element, name: &str, dflt: f64) -> f64 {
    e.attr(name).and_then(parse_num).unwrap_or(dflt)
}

/// Parse an `x[ y]` pair (e.g. `stdDeviation`, `radius`, `baseFrequency`); a
/// single value applies to both axes.
fn parse_xy(v: Option<&str>, dflt: f64) -> (f64, f64) {
    let list = v.map(parse_num_list).unwrap_or_default();
    match list.len() {
        0 => (dflt, dflt),
        1 => (list[0], list[0]),
        _ => (list[0], list[1]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::svg_path::Seg;
    use crate::svg::{Fill, Prim};

    /// A solid-filled rectangle primitive spanning `(x,y)..(x+w,y+h)`.
    fn rect_prim(x: f64, y: f64, w: f64, h: f64, rgb: [f64; 3]) -> Prim {
        Prim {
            segs: vec![
                Seg::Move(x, y),
                Seg::Line(x + w, y),
                Seg::Line(x + w, y + h),
                Seg::Line(x, y + h),
                Seg::Close,
            ],
            fill: Some(Fill::Solid(rgb)),
            stroke: None,
            stroke_w: 0.0,
            fill_opacity: 1.0,
            stroke_opacity: 1.0,
        }
    }

    fn filters_from(svg_defs: &str) -> Filters {
        let nodes = crate::html::dom::parse(svg_defs);
        let mut f = Filters::new();
        collect_filters(&nodes, &mut f);
        f
    }

    #[test]
    fn gaussian_blur_softens_edges_with_alpha_falloff() {
        let filters = filters_from(
            r##"<svg><filter id="b"><feGaussianBlur stdDeviation="3"/></filter></svg>"##,
        );
        let prims = vec![rect_prim(10.0, 10.0, 40.0, 40.0, [1.0, 0.0, 0.0])];
        let (raster, _) = render_filter("b", &prims, &filters, 1.0).expect("filter renders");
        // Center of the rect stays (near) opaque; well outside the rect alpha→0;
        // and there is a partially-transparent halo at the original edge.
        let cx = raster.w / 2;
        let cy = raster.h / 2;
        let center = raster.get(cx as i64, cy as i64);
        assert!(
            center[3] > 0.8,
            "blurred center stays mostly opaque (a={})",
            center[3]
        );
        let corner = raster.get(0, 0);
        assert!(
            corner[3] < 0.2,
            "far corner is (near) transparent (a={})",
            corner[3]
        );
        // A soft halo: some pixel has partial alpha (edge falloff).
        let any_partial = (0..raster.w * raster.h)
            .map(|p| raster.px[p * 4 + 3])
            .any(|a| a > 0.05 && a < 0.95);
        assert!(
            any_partial,
            "blur produces partial-alpha falloff at the edge"
        );
    }

    #[test]
    fn flood_then_composite_in_tints_the_shape() {
        // feFlood blue, then composite(in) with SourceGraphic → blue inside the
        // shape, transparent outside.
        let filters = filters_from(
            r##"<svg><filter id="t">
                <feFlood flood-color="#0000ff" result="f"/>
                <feComposite in="f" in2="SourceGraphic" operator="in"/>
            </filter></svg>"##,
        );
        let prims = vec![rect_prim(8.0, 8.0, 30.0, 30.0, [1.0, 0.0, 0.0])];
        let (raster, _) = render_filter("t", &prims, &filters, 1.0).expect("renders");
        let cx = raster.w / 2;
        let cy = raster.h / 2;
        let center = raster.get(cx as i64, cy as i64);
        assert!(
            center[2] > 0.8 && center[0] < 0.2 && center[3] > 0.8,
            "inside the shape is now blue (rgba={center:?})"
        );
        let corner = raster.get(0, 0);
        assert!(
            corner[3] < 0.2,
            "outside the shape stays transparent (a={})",
            corner[3]
        );
    }

    #[test]
    fn color_matrix_saturate_zero_greyscales() {
        let filters = filters_from(
            r##"<svg><filter id="g"><feColorMatrix type="saturate" values="0"/></filter></svg>"##,
        );
        // A saturated red rect: saturate(0) → grey (r==g==b).
        let prims = vec![rect_prim(5.0, 5.0, 30.0, 30.0, [1.0, 0.0, 0.0])];
        let (raster, _) = render_filter("g", &prims, &filters, 1.0).expect("renders");
        let cx = raster.w / 2;
        let cy = raster.h / 2;
        let p = raster.get(cx as i64, cy as i64);
        assert!(p[3] > 0.8, "still opaque where the shape was");
        assert!(
            (p[0] - p[1]).abs() < 0.02 && (p[1] - p[2]).abs() < 0.02,
            "saturate(0) yields grey (r=g=b), got {p:?}"
        );
        // Pure red's luma ≈ 0.2126 → mid-dark grey, definitely not still-red.
        assert!(
            p[0] < 0.6,
            "red channel dropped after desaturation (r={})",
            p[0]
        );
    }

    #[test]
    fn merge_stacks_two_layers() {
        // Flood red (result a), flood-and-clip blue square via composite(in)
        // (result b), then merge a then b → the top layer (b) wins where present.
        let filters = filters_from(
            r##"<svg><filter id="m">
                <feFlood flood-color="#ff0000" flood-opacity="1" result="red"/>
                <feMerge>
                    <feMergeNode in="red"/>
                    <feMergeNode in="SourceGraphic"/>
                </feMerge>
            </filter></svg>"##,
        );
        let prims = vec![rect_prim(10.0, 10.0, 20.0, 20.0, [0.0, 1.0, 0.0])];
        let (raster, _) = render_filter("m", &prims, &filters, 1.0).expect("renders");
        // Outside the green square: only the red flood shows.
        let edge = raster.get(0, 0);
        assert!(
            edge[0] > 0.8 && edge[1] < 0.2 && edge[3] > 0.8,
            "flood-red fills the background (rgba={edge:?})"
        );
        // Center: the green SourceGraphic is merged on top.
        let cx = raster.w / 2;
        let cy = raster.h / 2;
        let center = raster.get(cx as i64, cy as i64);
        assert!(
            center[1] > 0.8 && center[0] < 0.2,
            "green source stacks on top in the middle (rgba={center:?})"
        );
    }

    #[test]
    fn drop_shadow_adds_offset_blurred_copy() {
        // A generous region (the customary drop-shadow region) so the offset,
        // blurred copy lands inside the filter region rather than being clipped.
        let filters = filters_from(
            r##"<svg><filter id="d" x="-50%" y="-50%" width="200%" height="200%">
                <feDropShadow dx="8" dy="8" stdDeviation="2" flood-color="#000000"/>
            </filter></svg>"##,
        );
        let prims = vec![rect_prim(10.0, 10.0, 20.0, 20.0, [1.0, 0.0, 0.0])];
        let (raster, region) = render_filter("d", &prims, &filters, 1.0).expect("renders");
        let at = |ux: f64, uy: f64| {
            let dx = ((ux - region[0]) / region[2] * raster.w as f64) as i64;
            let dy = ((uy - region[1]) / region[3] * raster.h as f64) as i64;
            raster.get(dx, dy)
        };
        // The source rect occupies (10..30,10..30); the shadow sits down-right.
        // Sample just past the source's bottom-right corner (≈ 35,35 user) where
        // ONLY the offset+blurred dark shadow should be present.
        let shadow = at(35.0, 35.0);
        assert!(
            shadow[3] > 0.05 && shadow[0] < 0.4 && shadow[1] < 0.4 && shadow[2] < 0.4,
            "an offset, blurred dark shadow appears past the shape (rgba={shadow:?})"
        );
        // The source itself is still present & red at its center.
        let src = at(20.0, 20.0);
        assert!(
            src[0] > 0.7 && src[3] > 0.7,
            "the source graphic is preserved (rgba={src:?})"
        );
    }

    #[test]
    fn turbulence_produces_nonuniform_noise() {
        let filters = filters_from(
            r##"<svg><filter id="n">
                <feTurbulence type="fractalNoise" baseFrequency="0.2" numOctaves="3" seed="7"/>
            </filter></svg>"##,
        );
        // Geometry only sets the region; turbulence ignores the source content.
        let prims = vec![rect_prim(0.0, 0.0, 40.0, 40.0, [0.0, 0.0, 0.0])];
        let (raster, _) = render_filter("n", &prims, &filters, 1.0).expect("renders");
        // Collect alpha samples; fractalNoise yields a spread of values (variance
        // well above zero), not a flat field.
        let vals: Vec<f32> = (0..raster.w * raster.h)
            .map(|p| raster.px[p * 4 + 3])
            .collect();
        let mean = vals.iter().sum::<f32>() / vals.len() as f32;
        let var = vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / vals.len() as f32;
        assert!(var > 0.001, "turbulence is non-uniform (variance={var})");
        // Distinct min and max prove spatial variation.
        let min = vals.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = vals.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!(max - min > 0.1, "noise spans a range (min={min} max={max})");
    }

    #[test]
    fn offset_translates_the_graphic() {
        // Explicit wide region so the shifted copy stays inside the filter region.
        let filters = filters_from(
            r##"<svg><filter id="o" x="-50%" y="-50%" width="200%" height="200%"><feOffset dx="10" dy="0"/></filter></svg>"##,
        );
        let prims = vec![rect_prim(0.0, 10.0, 10.0, 20.0, [1.0, 0.0, 0.0])];
        let (raster, region) = render_filter("o", &prims, &filters, 1.0).expect("renders");
        // The left edge of the rect was at user x=0; after dx=+10 the painted
        // pixels start ~10 user units to the right. Column at user x=2 is empty,
        // column at user x=12 is painted.
        let col_at = |ux: f64| -> f32 {
            let dx = ((ux - region[0]) / region[2] * raster.w as f64) as i64;
            let mut maxa = 0.0f32;
            for y in 0..raster.h {
                maxa = maxa.max(raster.get(dx, y as i64)[3]);
            }
            maxa
        };
        assert!(col_at(2.0) < 0.2, "original location cleared by the offset");
        assert!(col_at(13.0) > 0.5, "graphic shifted right by dx");
    }

    #[test]
    fn morphology_dilate_grows_coverage() {
        let dilate = filters_from(
            r##"<svg><filter id="d"><feMorphology operator="dilate" radius="2"/></filter></svg>"##,
        );
        let prims = vec![rect_prim(15.0, 15.0, 10.0, 10.0, [1.0, 0.0, 0.0])];
        let (plain, _) = render_filter(
            "id",
            &prims,
            &filters_from(r##"<svg><filter id="id"><feOffset dx="0" dy="0"/></filter></svg>"##),
            1.0,
        )
        .expect("plain");
        let (grown, _) = render_filter("d", &prims, &dilate, 1.0).expect("dilated");
        let covered = |r: &Raster| (0..r.w * r.h).filter(|&p| r.px[p * 4 + 3] > 0.5).count();
        assert!(
            covered(&grown) > covered(&plain),
            "dilation increases covered pixels ({} > {})",
            covered(&grown),
            covered(&plain)
        );
    }

    #[test]
    fn component_transfer_linear_scales_alpha() {
        let filters = filters_from(
            r##"<svg><filter id="c">
                <feComponentTransfer><feFuncA type="linear" slope="0.5" intercept="0"/></feComponentTransfer>
            </filter></svg>"##,
        );
        let prims = vec![rect_prim(10.0, 10.0, 20.0, 20.0, [1.0, 0.0, 0.0])];
        let (raster, _) = render_filter("c", &prims, &filters, 1.0).expect("renders");
        let cx = raster.w / 2;
        let cy = raster.h / 2;
        let p = raster.get(cx as i64, cy as i64);
        assert!(
            (p[3] - 0.5).abs() < 0.05,
            "alpha halved by the linear transfer (a={})",
            p[3]
        );
    }

    // ── blend modes, composite operators, morphology erode, more transfers ─────

    fn one_rect(rgb: [f64; 3]) -> Vec<Prim> {
        vec![rect_prim(8.0, 8.0, 30.0, 30.0, rgb)]
    }

    /// Each feBlend mode runs end-to-end and yields an opaque blended centre.
    #[test]
    fn blend_modes_all_run() {
        for mode in [
            "multiply",
            "screen",
            "darken",
            "lighten",
            "overlay",
            "hard-light",
            "color-dodge",
            "color-burn",
            "soft-light",
            "difference",
            "exclusion",
        ] {
            let svg = format!(
                r##"<svg><filter id="b">
                    <feFlood flood-color="#3366cc" result="bg"/>
                    <feBlend in="SourceGraphic" in2="bg" mode="{mode}"/>
                </filter></svg>"##
            );
            let filters = filters_from(&svg);
            let (raster, _) = render_filter("b", &one_rect([0.8, 0.4, 0.2]), &filters, 1.0)
                .unwrap_or_else(|| panic!("blend {mode} renders"));
            let c = raster.get((raster.w / 2) as i64, (raster.h / 2) as i64);
            assert!(c[3] > 0.5, "blend {mode}: centre is covered (a={})", c[3]);
        }
    }

    /// blend_channel + hard_light reference values for the tricky branches.
    #[test]
    fn blend_channel_reference_values() {
        // Multiply/Screen/Darken/Lighten/Difference/Exclusion are exact.
        assert!((blend_channel(BlendMode::Multiply, 0.5, 0.4) - 0.2).abs() < 1e-6);
        assert!((blend_channel(BlendMode::Screen, 0.5, 0.4) - 0.7).abs() < 1e-6);
        assert!((blend_channel(BlendMode::Darken, 0.5, 0.4) - 0.4).abs() < 1e-6);
        assert!((blend_channel(BlendMode::Lighten, 0.5, 0.4) - 0.5).abs() < 1e-6);
        assert!((blend_channel(BlendMode::Difference, 0.5, 0.4) - 0.1).abs() < 1e-6);
        // ColorDodge: cs>=1 → 1.0 ; else cb/(1-cs) clamped.
        assert_eq!(blend_channel(BlendMode::ColorDodge, 1.0, 0.4), 1.0);
        assert!((blend_channel(BlendMode::ColorDodge, 0.5, 0.4) - 0.8).abs() < 1e-6);
        // ColorBurn: cs<=0 → 0.0 ; else 1-((1-cb)/cs) clamped.
        assert_eq!(blend_channel(BlendMode::ColorBurn, 0.0, 0.4), 0.0);
        // hard_light both branches (cs<=0.5 and cs>0.5).
        assert!((hard_light(0.25, 0.6) - (2.0 * 0.25 * 0.6)).abs() < 1e-6);
        assert!((hard_light(0.75, 0.6) - (1.0 - 2.0 * 0.25 * 0.4)).abs() < 1e-6);
        // SoftLight exercises both sub-branches.
        let _ = blend_channel(BlendMode::SoftLight, 0.3, 0.1);
        let _ = blend_channel(BlendMode::SoftLight, 0.8, 0.5);
    }

    /// Composite operators (out / atop / xor / arithmetic) each run.
    #[test]
    fn composite_operators_all_run() {
        for op in ["out", "atop", "xor", "over"] {
            let svg = format!(
                r##"<svg><filter id="c">
                    <feFlood flood-color="#00ff00" result="g"/>
                    <feComposite in="g" in2="SourceGraphic" operator="{op}"/>
                </filter></svg>"##
            );
            let filters = filters_from(&svg);
            assert!(
                render_filter("c", &one_rect([1.0, 0.0, 0.0]), &filters, 1.0).is_some(),
                "composite {op} renders"
            );
        }
        // Arithmetic with explicit k coefficients.
        let filters = filters_from(
            r##"<svg><filter id="a">
                <feFlood flood-color="#0000ff" result="b"/>
                <feComposite in="b" in2="SourceGraphic" operator="arithmetic"
                    k1="0" k2="0.5" k3="0.5" k4="0"/>
            </filter></svg>"##,
        );
        let (raster, _) =
            render_filter("a", &one_rect([1.0, 0.0, 0.0]), &filters, 1.0).expect("arithmetic");
        let c = raster.get((raster.w / 2) as i64, (raster.h / 2) as i64);
        assert!(c[3] > 0.0, "arithmetic produced output (a={})", c[3]);
    }

    /// feMorphology erode shrinks coverage (the opposite of the dilate test).
    #[test]
    fn morphology_erode_shrinks_coverage() {
        let erode = filters_from(
            r##"<svg><filter id="e"><feMorphology operator="erode" radius="2"/></filter></svg>"##,
        );
        let prims = vec![rect_prim(15.0, 15.0, 12.0, 12.0, [1.0, 0.0, 0.0])];
        let (plain, _) = render_filter(
            "id",
            &prims,
            &filters_from(r##"<svg><filter id="id"><feOffset dx="0" dy="0"/></filter></svg>"##),
            1.0,
        )
        .expect("plain");
        let (shrunk, _) = render_filter("e", &prims, &erode, 1.0).expect("eroded");
        let covered = |r: &Raster| (0..r.w * r.h).filter(|&p| r.px[p * 4 + 3] > 0.5).count();
        assert!(
            covered(&shrunk) < covered(&plain),
            "erosion reduces covered pixels ({} < {})",
            covered(&shrunk),
            covered(&plain)
        );
    }

    /// feColorMatrix hueRotate, luminanceToAlpha, and a full 20-value matrix.
    #[test]
    fn color_matrix_hue_luminance_and_full_matrix() {
        // hueRotate(180) on red → not still pure red.
        let hue = filters_from(
            r##"<svg><filter id="h"><feColorMatrix type="hueRotate" values="180"/></filter></svg>"##,
        );
        let (r, _) = render_filter("h", &one_rect([1.0, 0.0, 0.0]), &hue, 1.0).expect("hue");
        let c = r.get((r.w / 2) as i64, (r.h / 2) as i64);
        assert!(c[3] > 0.5, "hueRotate keeps the shape opaque");

        // luminanceToAlpha: alpha becomes the luminance; rgb zeroed.
        let lum = filters_from(
            r##"<svg><filter id="l"><feColorMatrix type="luminanceToAlpha"/></filter></svg>"##,
        );
        let (r, _) = render_filter("l", &one_rect([1.0, 1.0, 1.0]), &lum, 1.0).expect("lum");
        let c = r.get((r.w / 2) as i64, (r.h / 2) as i64);
        assert!(
            c[3] > 0.5,
            "white maps to high luminance alpha (a={})",
            c[3]
        );

        // Full matrix: identity-ish that swaps to blue via the constant column.
        let m = filters_from(
            r##"<svg><filter id="m"><feColorMatrix type="matrix"
                values="0 0 0 0 0  0 0 0 0 0  0 0 0 0 1  0 0 0 1 0"/></filter></svg>"##,
        );
        let (r, _) = render_filter("m", &one_rect([1.0, 0.0, 0.0]), &m, 1.0).expect("matrix");
        let c = r.get((r.w / 2) as i64, (r.h / 2) as i64);
        assert!(c[2] > 0.8 && c[0] < 0.2, "matrix forced blue (rgba={c:?})");
    }

    /// feComponentTransfer table / discrete / gamma / identity branches.
    #[test]
    fn component_transfer_table_discrete_gamma_identity() {
        for func in [
            r#"<feFuncA type="table" tableValues="0 1"/>"#,
            r#"<feFuncA type="discrete" tableValues="0 0.5 1"/>"#,
            r#"<feFuncA type="gamma" amplitude="1" exponent="2" offset="0"/>"#,
            r#"<feFuncR type="identity"/>"#,
        ] {
            let svg = format!(
                r##"<svg><filter id="c"><feComponentTransfer>{func}</feComponentTransfer></filter></svg>"##
            );
            let filters = filters_from(&svg);
            assert!(
                render_filter("c", &one_rect([0.6, 0.6, 0.6]), &filters, 1.0).is_some(),
                "transfer {func} renders"
            );
        }
    }

    /// apply_transfer reference values for each transfer kind.
    #[test]
    fn apply_transfer_reference_values() {
        assert_eq!(apply_transfer(&Transfer::Identity, 0.4), 0.4);
        // Empty table / single value.
        assert_eq!(apply_transfer(&Transfer::Table(vec![]), 0.4), 0.4);
        assert_eq!(apply_transfer(&Transfer::Table(vec![0.7]), 0.4), 0.7);
        // Two-entry table interpolates linearly (c=0.5 → 0.5).
        assert!((apply_transfer(&Transfer::Table(vec![0.0, 1.0]), 0.5) - 0.5).abs() < 1e-5);
        // Discrete picks the bucket.
        assert_eq!(
            apply_transfer(&Transfer::Discrete(vec![0.0, 0.5, 1.0]), 0.5),
            0.5
        );
        assert_eq!(apply_transfer(&Transfer::Discrete(vec![]), 0.3), 0.3);
        // Linear & gamma.
        assert!(
            (apply_transfer(
                &Transfer::Linear {
                    slope: 0.5,
                    intercept: 0.25
                },
                1.0
            ) - 0.75)
                .abs()
                < 1e-5
        );
        assert!(
            (apply_transfer(
                &Transfer::Gamma {
                    amplitude: 1.0,
                    exponent: 2.0,
                    offset: 0.0
                },
                0.5
            ) - 0.25)
                .abs()
                < 1e-5
        );
    }

    /// feTurbulence type="turbulence" (the non-fractal branch).
    #[test]
    fn turbulence_plain_type_runs() {
        let filters = filters_from(
            r##"<svg><filter id="n">
                <feTurbulence type="turbulence" baseFrequency="0.15" numOctaves="2" seed="3"/>
            </filter></svg>"##,
        );
        let prims = vec![rect_prim(0.0, 0.0, 32.0, 32.0, [0.0, 0.0, 0.0])];
        let (raster, _) = render_filter("n", &prims, &filters, 1.0).expect("turbulence");
        let any_ink = (0..raster.w * raster.h).any(|p| raster.px[p * 4 + 3] > 0.01);
        assert!(any_ink, "plain turbulence produced some output");
    }

    /// feDisplacementMap perturbs the source using a second input's channels.
    #[test]
    fn displacement_map_runs() {
        let filters = filters_from(
            r##"<svg><filter id="dm">
                <feTurbulence type="turbulence" baseFrequency="0.2" numOctaves="2" result="noise"/>
                <feDisplacementMap in="SourceGraphic" in2="noise" scale="10"
                    xChannelSelector="R" yChannelSelector="G"/>
            </filter></svg>"##,
        );
        let prims = vec![rect_prim(10.0, 10.0, 30.0, 30.0, [1.0, 0.0, 0.0])];
        assert!(
            render_filter("dm", &prims, &filters, 1.0).is_some(),
            "displacement map renders"
        );
    }

    // ── pure parser/helper units ──────────────────────────────────────────────

    #[test]
    fn parse_num_and_lists() {
        assert_eq!(parse_num("12.5px"), Some(12.5));
        assert_eq!(parse_num("  -3e2 "), Some(-300.0));
        assert_eq!(parse_num("abc"), None);
        assert_eq!(parse_num_list("1, 2  3,,4"), vec![1.0, 2.0, 3.0, 4.0]);
        assert!(parse_num_list("").is_empty());
    }

    #[test]
    fn parse_xy_one_and_two_values() {
        assert_eq!(parse_xy(Some("5"), 1.0), (5.0, 5.0));
        assert_eq!(parse_xy(Some("3 7"), 1.0), (3.0, 7.0));
        assert_eq!(parse_xy(None, 2.0), (2.0, 2.0));
    }

    #[test]
    fn channel_sel_maps_letters_and_default() {
        assert_eq!(channel_sel(Some("R"), 9), 0);
        assert_eq!(channel_sel(Some("G"), 9), 1);
        assert_eq!(channel_sel(Some("B"), 9), 2);
        assert_eq!(channel_sel(Some("A"), 9), 3);
        assert_eq!(channel_sel(Some("Z"), 7), 7); // unknown → default
        assert_eq!(channel_sel(None, 5), 5);
    }

    #[test]
    fn parse_blend_mode_known_and_unknown() {
        assert!(matches!(
            parse_blend_mode(Some("multiply")),
            BlendMode::Multiply
        ));
        assert!(matches!(
            parse_blend_mode(Some("screen")),
            BlendMode::Screen
        ));
        // Unknown / missing → Normal.
        assert!(matches!(parse_blend_mode(Some("bogus")), BlendMode::Normal));
        assert!(matches!(parse_blend_mode(None), BlendMode::Normal));
    }

    #[test]
    fn saturate_and_hue_matrices_have_expected_shape() {
        // saturate(1) is (close to) the identity for the diagonal-ish luma terms.
        let s = saturate_matrix(1.0);
        assert_eq!(s.len(), 20);
        // hue_rotate(0) ≈ identity on the RGB block diagonal.
        let h = hue_rotate_matrix(0.0);
        assert_eq!(h.len(), 20);
        assert!((h[0] - 1.0).abs() < 1e-3, "hueRotate(0)[0,0] ≈ 1");
    }

    #[test]
    fn box_sizes_and_over_straight() {
        // box_sizes_for_sigma returns three radii summing to a blur of ~sigma.
        let sizes = box_sizes_for_sigma(3.0);
        assert_eq!(sizes.len(), 3);
        assert!(
            sizes.iter().all(|&s| s > 0),
            "non-zero box radii: {sizes:?}"
        );
        // over_straight: opaque src over anything → src.
        let src = [0.2, 0.4, 0.6, 1.0];
        let dst = [0.9, 0.9, 0.9, 1.0];
        assert_eq!(over_straight(src, dst), src);
        // Transparent src over dst → dst unchanged.
        let clear = [0.0, 0.0, 0.0, 0.0];
        let out = over_straight(clear, dst);
        for c in 0..4 {
            assert!((out[c] - dst[c]).abs() < 1e-6, "clear over dst = dst");
        }
    }
}
