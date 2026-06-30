//! Colour-font tables: **COLR v0** (layered colour glyphs) + **CPAL** (palettes).
//!
//! A COLRv0 colour glyph (e.g. an emoji) is a stack of ordinary monochrome
//! glyphs, each filled with a palette colour. We expose those layers so the
//! renderer can draw them as **native vector paths** (reusing
//! [`super::truetype::TrueTypeFont::glyph_polygons`]), which renders the emoji in
//! colour regardless of viewer support and rasterizes for free.
//!
//! COLRv1 (gradients/transforms) is not interpreted; its v0 base/layer records
//! still parse, so simple layered glyphs in a v1 font render in flat colour.

fn be16(d: &[u8], o: usize) -> u16 {
    if o + 2 <= d.len() {
        ((d[o] as u16) << 8) | d[o + 1] as u16
    } else {
        0
    }
}

fn be32(d: &[u8], o: usize) -> u32 {
    if o + 4 <= d.len() {
        ((d[o] as u32) << 24)
            | ((d[o + 1] as u32) << 16)
            | ((d[o + 2] as u32) << 8)
            | d[o + 3] as u32
    } else {
        0
    }
}

/// One layer of a colour glyph: a glyph id to fill with `rgb` at `alpha`.
/// `use_foreground` marks the special palette index `0xFFFF` (use the text
/// colour); the renderer substitutes the run's colour then.
#[derive(Debug, Clone, Copy)]
pub struct Layer {
    pub gid: u16,
    pub rgb: [f64; 3],
    pub alpha: f64,
    pub use_foreground: bool,
}

/// Parsed COLR/CPAL tables: base-glyph → layer ranges, layer records, palette 0.
#[derive(Debug, Clone)]
pub struct ColorGlyphs {
    /// `(base_gid, first_layer_index, num_layers)`, sorted by `base_gid`.
    bases: Vec<(u16, u16, u16)>,
    /// `(layer_gid, palette_index)` records.
    layers: Vec<(u16, u16)>,
    /// Palette 0 entries as RGBA in `0..=1`.
    palette: Vec<[f64; 4]>,
}

impl ColorGlyphs {
    /// Parse the raw `COLR` and `CPAL` table bytes. Returns `None` if neither a
    /// base glyph nor a palette can be read.
    pub fn parse(colr: &[u8], cpal: &[u8]) -> Option<ColorGlyphs> {
        // COLR header (the v0 fields sit at the same offsets in v1).
        let num_base = be16(colr, 2) as usize;
        let base_off = be32(colr, 4) as usize;
        let layer_off = be32(colr, 8) as usize;
        let num_layers = be16(colr, 12) as usize;

        let mut bases = Vec::with_capacity(num_base);
        for i in 0..num_base {
            let o = base_off + i * 6;
            if o + 6 > colr.len() {
                break;
            }
            bases.push((be16(colr, o), be16(colr, o + 2), be16(colr, o + 4)));
        }
        let mut layers = Vec::with_capacity(num_layers);
        for i in 0..num_layers {
            let o = layer_off + i * 4;
            if o + 4 > colr.len() {
                break;
            }
            layers.push((be16(colr, o), be16(colr, o + 2)));
        }
        bases.sort_by_key(|b| b.0); // guarantee sorted for binary search

        // CPAL — read palette 0's entries (colorRecordIndices[0] + entry index).
        let palette = parse_cpal_palette(cpal);

        if bases.is_empty() || palette.is_empty() {
            return None;
        }
        Some(ColorGlyphs {
            bases,
            layers,
            palette,
        })
    }

    /// The colour layers of `base_gid`, or `None` if it isn't a colour glyph.
    pub fn layers(&self, base_gid: u16) -> Option<Vec<Layer>> {
        let idx = self.bases.binary_search_by_key(&base_gid, |b| b.0).ok()?;
        let (_, first, num) = self.bases[idx];
        let mut out = Vec::with_capacity(num as usize);
        for i in 0..num as usize {
            let (gid, pi) = *self.layers.get(first as usize + i)?;
            if pi == 0xFFFF {
                out.push(Layer {
                    gid,
                    rgb: [0.0, 0.0, 0.0],
                    alpha: 1.0,
                    use_foreground: true,
                });
            } else {
                let c = self
                    .palette
                    .get(pi as usize)
                    .copied()
                    .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                out.push(Layer {
                    gid,
                    rgb: [c[0], c[1], c[2]],
                    alpha: c[3],
                    use_foreground: false,
                });
            }
        }
        Some(out)
    }
}

fn bei16(d: &[u8], o: usize) -> i16 {
    be16(d, o) as i16
}

/// Read palette 0 of a `CPAL` table as RGBA entries in `0..=1` (records are
/// stored BGRA). Shared by COLR v0 and v1. Empty if the table is malformed.
fn parse_cpal_palette(cpal: &[u8]) -> Vec<[f64; 4]> {
    let num_entries = be16(cpal, 2) as usize;
    let num_palettes = be16(cpal, 4) as usize;
    let num_records = be16(cpal, 6) as usize;
    let records_off = be32(cpal, 8) as usize;
    let mut palette = Vec::new();
    if num_palettes == 0 {
        return palette;
    }
    let first = be16(cpal, 12) as usize; // colorRecordIndices[0]
    for p in 0..num_entries {
        let ri = first + p;
        if ri >= num_records {
            break;
        }
        let o = records_off + ri * 4;
        if o + 4 > cpal.len() {
            break;
        }
        palette.push([
            cpal[o + 2] as f64 / 255.0,
            cpal[o + 1] as f64 / 255.0,
            cpal[o] as f64 / 255.0,
            cpal[o + 3] as f64 / 255.0,
        ]);
    }
    palette
}

/// One glyph bitmap from the `sbix` table (Apple colour emoji): the encoded
/// image bytes plus the strike's pixels-per-em and the glyph's origin offset.
#[derive(Debug, Clone)]
pub struct SbixGlyph {
    pub png: Vec<u8>,
    pub ppem: f64,
    pub origin_x: f64,
    pub origin_y: f64,
}

/// The `sbix` table resolved to its best (largest) PNG strike.
#[derive(Debug, Clone)]
pub struct Sbix {
    data: Vec<u8>,
    strike_off: usize,
    ppem: f64,
    num_glyphs: u16,
}

impl Sbix {
    /// Parse the `sbix` table, choosing the strike with the largest ppem.
    pub fn parse(sbix: &[u8], num_glyphs: u16) -> Option<Sbix> {
        let num_strikes = be32(sbix, 4) as usize;
        let mut best: Option<(usize, u16)> = None;
        for i in 0..num_strikes {
            let off = be32(sbix, 8 + i * 4) as usize;
            if off + 4 > sbix.len() {
                continue;
            }
            let ppem = be16(sbix, off);
            if best.map(|(_, p)| ppem > p).unwrap_or(true) {
                best = Some((off, ppem));
            }
        }
        let (strike_off, ppem) = best?;
        Some(Sbix {
            data: sbix.to_vec(),
            strike_off,
            ppem: ppem as f64,
            num_glyphs,
        })
    }

    /// The PNG bitmap for `gid` in the chosen strike, or `None` if the glyph has
    /// no bitmap (or a non-PNG `graphicType`).
    pub fn glyph(&self, gid: u16) -> Option<SbixGlyph> {
        if gid >= self.num_glyphs {
            return None;
        }
        // glyphDataOffsets[] follows the strike's `ppem`(u16) + `ppi`(u16).
        let table = self.strike_off + 4;
        let g = gid as usize;
        let start = be32(&self.data, table + g * 4) as usize;
        let end = be32(&self.data, table + (g + 1) * 4) as usize;
        if end <= start {
            return None; // empty glyph in this strike
        }
        let rec = self.strike_off + start;
        let rec_end = self.strike_off + end;
        if rec + 8 > self.data.len() || rec_end > self.data.len() {
            return None;
        }
        if &self.data[rec + 4..rec + 8] != b"png " {
            return None; // only PNG bitmaps are placed
        }
        Some(SbixGlyph {
            png: self.data[rec + 8..rec_end].to_vec(),
            ppem: self.ppem,
            origin_x: bei16(&self.data, rec) as f64,
            origin_y: bei16(&self.data, rec + 2) as f64,
        })
    }
}

// ---------------------------------------------------------------------------
// COLR version 1 — the paint graph (gradients + affine transforms).
// ---------------------------------------------------------------------------
//
// A v1 colour glyph is a tree of `Paint` records reached from the
// `BaseGlyphList`. We flatten that tree into a back-to-front list of
// [`Colrv1Layer`]s — each is one outline glyph filled by a [`PaintFill`]
// (a solid CPAL colour, or an axial/radial gradient) under an accumulated
// 2×3 affine. The renderer walks the list and emits one fill per layer,
// reusing the engine's outline + shading machinery.

/// A 2×3 affine (`x' = a·x + c·y + e`, `y' = b·x + d·y + f`) in font units.
#[derive(Debug, Clone, Copy)]
pub struct Affine {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Affine {
    /// The identity transform.
    pub fn identity() -> Affine {
        Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    /// `self ∘ other` — apply `other` first, then `self`.
    fn then(self, o: Affine) -> Affine {
        Affine {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }
}

/// A colour stop on a COLRv1 gradient: position plus the resolved CPAL colour.
#[derive(Debug, Clone, Copy)]
pub struct Colrv1Stop {
    pub offset: f64,
    pub rgb: [f64; 3],
    pub alpha: f64,
    /// `true` when this stop used the special palette index `0xFFFF` (the text
    /// foreground colour); the renderer substitutes the run's colour.
    pub use_foreground: bool,
}

/// How a COLRv1 layer glyph is filled.
#[derive(Debug, Clone)]
pub enum PaintFill {
    /// A flat CPAL colour at `alpha` (`use_foreground` ⇒ the run's text colour).
    Solid {
        rgb: [f64; 3],
        alpha: f64,
        use_foreground: bool,
    },
    /// `PaintLinearGradient`: line `(x0,y0)→(x1,y1)`; `(x2,y2)` is the rotation
    /// reference (used to skew the gradient line) — we project it for the angle.
    Linear {
        p0: (f64, f64),
        p1: (f64, f64),
        p2: (f64, f64),
        stops: Vec<Colrv1Stop>,
    },
    /// `PaintRadialGradient`: two circles `(c0,r0)`/`(c1,r1)`.
    Radial {
        c0: (f64, f64),
        r0: f64,
        c1: (f64, f64),
        r1: f64,
        stops: Vec<Colrv1Stop>,
    },
    /// `PaintSweepGradient`: a conic sweep about `center` from `start_angle` to
    /// `end_angle` (radians, CCW from +X — already converted from the table's
    /// F2Dot14 ×180° encoding). Rendered as a fan of flat-coloured sectors.
    Sweep {
        center: (f64, f64),
        start_angle: f64,
        end_angle: f64,
        stops: Vec<Colrv1Stop>,
    },
}

/// One flattened COLRv1 layer: fill the outline of `gid` with `fill` under
/// `transform` (font units), composited onto the layers below with `blend`.
/// Emitted back-to-front (first = bottom).
#[derive(Debug, Clone)]
pub struct Colrv1Layer {
    pub gid: u16,
    pub fill: PaintFill,
    pub transform: Affine,
    /// The PDF `/BM` blend mode for this layer (`Normal` = plain source-over).
    /// Set by an enclosing `PaintComposite` for its source sub-tree.
    pub blend: BlendName,
}

/// A PDF separable blend-mode name (the `/BM` entry of an ExtGState), mapped
/// from a COLRv1 `CompositeMode`. Non-separable / unsupported modes fall back to
/// `Normal` (plain source-over).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendName {
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
}

impl BlendName {
    /// The PDF `/BM` name token (without the leading slash).
    pub fn pdf_name(self) -> &'static [u8] {
        match self {
            BlendName::Normal => b"Normal",
            BlendName::Multiply => b"Multiply",
            BlendName::Screen => b"Screen",
            BlendName::Overlay => b"Overlay",
            BlendName::Darken => b"Darken",
            BlendName::Lighten => b"Lighten",
            BlendName::ColorDodge => b"ColorDodge",
            BlendName::ColorBurn => b"ColorBurn",
            BlendName::HardLight => b"HardLight",
            BlendName::SoftLight => b"SoftLight",
            BlendName::Difference => b"Difference",
            BlendName::Exclusion => b"Exclusion",
        }
    }

    /// Map a COLRv1 `CompositeMode` (OpenType §COLR, table 8) to a PDF blend
    /// mode. The Porter-Duff alpha modes (CLEAR/SRC/DEST/…) and the
    /// non-separable HSL modes have no separable `/BM` equivalent and fall back
    /// to `Normal` (the common `SRC_OVER` case).
    fn from_composite_mode(mode: u8) -> BlendName {
        match mode {
            // 0..=8 are the Porter-Duff alpha compositing modes; SRC_OVER (3)
            // and the rest composite normally for our purposes.
            9 => BlendName::Screen,
            10 => BlendName::Overlay,
            11 => BlendName::Darken,
            12 => BlendName::Lighten,
            13 => BlendName::ColorDodge,
            14 => BlendName::ColorBurn,
            15 => BlendName::HardLight,
            16 => BlendName::SoftLight,
            17 => BlendName::Difference,
            18 => BlendName::Exclusion,
            19 => BlendName::Multiply,
            _ => BlendName::Normal, // CLEAR/SRC/.../SRC_OVER + HSL modes
        }
    }
}

/// The COLR **v1** paint graph (present only when `COLR` is version 1).
#[derive(Debug, Clone)]
pub struct Colrv1 {
    colr: Vec<u8>,
    /// `(base_gid, paint_offset)` from the `BaseGlyphList`, sorted by gid.
    base_paints: Vec<(u16, u32)>,
    /// Absolute offset of the `LayerList` (or 0 if absent).
    layer_list_off: u32,
    /// Palette 0 entries as RGBA in `0..=1` (shared with COLRv0's CPAL parse).
    palette: Vec<[f64; 4]>,
    /// Variation deltas for `PaintVar*` formats: the `ItemVariationStore` plus
    /// the `DeltaSetIndexMap` (variable colour fonts). `None` for static fonts.
    var: Option<VarStore>,
    /// Normalized variation coordinates (one per font axis, `-1.0..=1.0`) of the
    /// instance being rendered. Empty ⇒ the default instance (all deltas zero).
    coords: Vec<f64>,
}

impl Colrv1 {
    /// Parse the v1 paint graph from the `COLR`+`CPAL` bytes. `None` unless the
    /// table is COLR version 1 with a non-empty `BaseGlyphList`. The default
    /// (non-variable) instance is rendered; see [`with_coords`](Self::with_coords).
    pub fn parse(colr: &[u8], cpal: &[u8]) -> Option<Colrv1> {
        if be16(colr, 0) != 1 {
            return None; // not a v1 table
        }
        // v1 header extends the v0 one: at offset 14 sit the 32-bit offsets to
        // BaseGlyphList, LayerList, ClipList, then VarIdxMap + ItemVariationStore.
        let base_list_off = be32(colr, 14) as usize;
        let layer_list_off = be32(colr, 18);
        let var_idx_map_off = be32(colr, 26) as usize;
        let var_store_off = be32(colr, 30) as usize;

        let mut base_paints = Vec::new();
        if base_list_off != 0 && base_list_off + 4 <= colr.len() {
            let n = be32(colr, base_list_off) as usize;
            for i in 0..n {
                // BaseGlyphPaintRecord: glyphID(u16) + paintOffset(Offset24→
                // actually Offset32 from the BaseGlyphList start).
                let rec = base_list_off + 4 + i * 6;
                if rec + 6 > colr.len() {
                    break;
                }
                let gid = be16(colr, rec);
                let off = be32(colr, rec + 2);
                if off != 0 {
                    base_paints.push((gid, base_list_off as u32 + off));
                }
            }
            base_paints.sort_by_key(|p| p.0);
        }
        if base_paints.is_empty() {
            return None;
        }

        // Palette 0 (BGRA → RGBA). v1 fonts need not carry v0 base records, so
        // read CPAL directly rather than via `ColorGlyphs::parse`.
        let palette = parse_cpal_palette(cpal);

        // Variation store + index map (variable colour fonts). Absent ⇒ static.
        let var = (var_store_off != 0)
            .then(|| VarStore::parse(colr, var_store_off, var_idx_map_off))
            .flatten();

        Some(Colrv1 {
            colr: colr.to_vec(),
            base_paints,
            layer_list_off,
            palette,
            var,
            coords: Vec::new(),
        })
    }

    /// Select a variation instance by its **normalized** axis coordinates (one
    /// per font axis, each `-1.0..=1.0`). Affects only `PaintVar*` formats; for
    /// a static font (no `ItemVariationStore`) this is a no-op.
    pub fn with_coords(mut self, coords: Vec<f64>) -> Colrv1 {
        self.coords = coords;
        self
    }

    /// The variation delta for `var_index_base + field` (or 0 when the font is
    /// static, the instance is the default, or the index is out of range).
    fn delta(&self, var_index_base: u32, field: u32) -> f64 {
        if var_index_base == 0xFFFF_FFFF || self.coords.is_empty() {
            return 0.0;
        }
        match &self.var {
            Some(v) => v.delta(var_index_base + field, &self.coords),
            None => 0.0,
        }
    }

    /// Resolve a CPAL `(palette_index, alpha_factor)` to `(rgb, alpha, fg?)`.
    fn cpal_color(&self, index: u16, alpha_factor: f64) -> ([f64; 3], f64, bool) {
        if index == 0xFFFF {
            return ([0.0, 0.0, 0.0], alpha_factor, true);
        }
        let c = self
            .palette
            .get(index as usize)
            .copied()
            .unwrap_or([0.0, 0.0, 0.0, 1.0]);
        ([c[0], c[1], c[2]], c[3] * alpha_factor, false)
    }

    /// Read a `ColorLine` (or `VarColorLine`) at `off`: `extend(u8)` +
    /// `numStops(u16)` + `ColorStop[]`. A plain `ColorStop` is 6 bytes
    /// (stopOffset F2Dot14, paletteIndex u16, alpha F2Dot14); a `VarColorStop`
    /// (when `variable`) adds a `varIndexBase(u32)` (10 bytes) whose deltas
    /// adjust the stop's offset (+0) and alpha (+1).
    fn color_line(&self, off: usize, variable: bool) -> Vec<Colrv1Stop> {
        let d = &self.colr;
        let mut stops = Vec::new();
        if off + 3 > d.len() {
            return stops;
        }
        let num = be16(d, off + 1) as usize;
        let rec = if variable { 10 } else { 6 };
        for i in 0..num {
            let s = off + 3 + i * rec;
            if s + rec > d.len() {
                break;
            }
            let mut stop_off = f2dot14(d, s);
            let pi = be16(d, s + 2);
            let mut alpha = f2dot14(d, s + 4);
            if variable {
                let vib = be32(d, s + 6);
                stop_off += self.delta(vib, 0) / 16384.0; // F2Dot14 delta units
                alpha += self.delta(vib, 1) / 16384.0;
            }
            let (rgb, a, fg) = self.cpal_color(pi, alpha.clamp(0.0, 1.0));
            stops.push(Colrv1Stop {
                offset: stop_off,
                rgb,
                alpha: a,
                use_foreground: fg,
            });
        }
        stops
    }

    /// Flatten the paint graph of `base_gid` into back-to-front layers, or
    /// `None` if it isn't a v1 base glyph.
    pub fn layers(&self, base_gid: u16) -> Option<Vec<Colrv1Layer>> {
        let idx = self
            .base_paints
            .binary_search_by_key(&base_gid, |p| p.0)
            .ok()?;
        let root = self.base_paints[idx].1 as usize;
        let mut out = Vec::new();
        self.walk_paint(
            root,
            Affine::identity(),
            None,
            BlendName::Normal,
            0,
            &mut out,
        );
        Some(out)
    }

    /// Recursively walk a `Paint` subtree. `glyph` is the outline gid set by the
    /// nearest enclosing `PaintGlyph` (the shape a fill paints into); `blend` is
    /// the `/BM` mode inherited from an enclosing `PaintComposite` source.
    fn walk_paint(
        &self,
        off: usize,
        ctm: Affine,
        glyph: Option<u16>,
        blend: BlendName,
        depth: u8,
        out: &mut Vec<Colrv1Layer>,
    ) {
        let d = &self.colr;
        if depth > 64 || off == 0 || off >= d.len() {
            return; // cycle guard / out of range
        }
        let format = d[off];
        match format {
            // PaintColrLayers: a run of children in the LayerList.
            1 => {
                let num = d.get(off + 1).copied().unwrap_or(0) as usize;
                let first = be32(d, off + 2) as usize;
                let ll = self.layer_list_off as usize;
                if ll == 0 || ll + 4 > d.len() {
                    return;
                }
                let total = be32(d, ll) as usize;
                for i in 0..num {
                    let li = first + i;
                    if li >= total {
                        break;
                    }
                    let po = ll + 4 + li * 4;
                    if po + 4 > d.len() {
                        break;
                    }
                    let child = ll + be32(d, po) as usize;
                    self.walk_paint(child, ctm, glyph, blend, depth + 1, out);
                }
            }
            // PaintSolid (2) / PaintVarSolid (3): fill `glyph` with a CPAL colour.
            // The Var form carries a trailing `varIndexBase` whose +0 delta
            // adjusts the alpha.
            2 | 3 => {
                if let Some(g) = glyph {
                    let pi = be16(d, off + 1);
                    let mut alpha = f2dot14(d, off + 3);
                    if format == 3 {
                        alpha += self.delta(be32(d, off + 5), 0) / 16384.0;
                    }
                    let (rgb, a, fg) = self.cpal_color(pi, alpha.clamp(0.0, 1.0));
                    out.push(Colrv1Layer {
                        gid: g,
                        fill: PaintFill::Solid {
                            rgb,
                            alpha: a,
                            use_foreground: fg,
                        },
                        transform: ctm,
                        blend,
                    });
                }
            }
            // PaintLinearGradient (4) / Var (5). Var deltas +0..+5 adjust
            // x0,y0,x1,y1,x2,y2 (the `varIndexBase` follows the 6 coords).
            4 | 5 => {
                if let Some(g) = glyph {
                    let cl = off + be24(d, off + 1) as usize;
                    let mut p = [
                        bei16(d, off + 4) as f64,
                        bei16(d, off + 6) as f64,
                        bei16(d, off + 8) as f64,
                        bei16(d, off + 10) as f64,
                        bei16(d, off + 12) as f64,
                        bei16(d, off + 14) as f64,
                    ];
                    if format == 5 {
                        let vib = be32(d, off + 16);
                        for (k, v) in p.iter_mut().enumerate() {
                            *v += self.delta(vib, k as u32);
                        }
                    }
                    let stops = self.color_line(cl, format == 5);
                    if !stops.is_empty() {
                        out.push(Colrv1Layer {
                            gid: g,
                            fill: PaintFill::Linear {
                                p0: (p[0], p[1]),
                                p1: (p[2], p[3]),
                                p2: (p[4], p[5]),
                                stops,
                            },
                            transform: ctm,
                            blend,
                        });
                    }
                }
            }
            // PaintRadialGradient (6) / Var (7). Var deltas +0..+5 adjust
            // x0,y0,r0,x1,y1,r1.
            6 | 7 => {
                if let Some(g) = glyph {
                    let cl = off + be24(d, off + 1) as usize;
                    let mut v = [
                        bei16(d, off + 4) as f64,
                        bei16(d, off + 6) as f64,
                        be16(d, off + 8) as f64,
                        bei16(d, off + 10) as f64,
                        bei16(d, off + 12) as f64,
                        be16(d, off + 14) as f64,
                    ];
                    if format == 7 {
                        let vib = be32(d, off + 16);
                        for (k, val) in v.iter_mut().enumerate() {
                            *val += self.delta(vib, k as u32);
                        }
                    }
                    let stops = self.color_line(cl, format == 7);
                    if !stops.is_empty() {
                        out.push(Colrv1Layer {
                            gid: g,
                            fill: PaintFill::Radial {
                                c0: (v[0], v[1]),
                                r0: v[2].max(0.0),
                                c1: (v[3], v[4]),
                                r1: v[5].max(0.0),
                                stops,
                            },
                            transform: ctm,
                            blend,
                        });
                    }
                }
            }
            // PaintSweepGradient (8) / Var (9): a conic sweep. Rendered as a fan
            // of flat-coloured sectors (PDF has no native conic shading), the
            // same approach as CSS `conic-gradient`. Var deltas +0..+3 adjust
            // centerX,centerY,startAngle,endAngle.
            8 | 9 => {
                if let Some(g) = glyph {
                    let cl = off + be24(d, off + 1) as usize;
                    let mut cx = bei16(d, off + 4) as f64;
                    let mut cy = bei16(d, off + 6) as f64;
                    // Angles are F2Dot14 in counter-clockwise ×180° units.
                    let mut start = f2dot14(d, off + 8);
                    let mut end = f2dot14(d, off + 10);
                    if format == 9 {
                        let vib = be32(d, off + 12);
                        cx += self.delta(vib, 0);
                        cy += self.delta(vib, 1);
                        start += self.delta(vib, 2) / 16384.0;
                        end += self.delta(vib, 3) / 16384.0;
                    }
                    let stops = self.color_line(cl, format == 9);
                    if !stops.is_empty() {
                        out.push(Colrv1Layer {
                            gid: g,
                            fill: PaintFill::Sweep {
                                center: (cx, cy),
                                start_angle: start * std::f64::consts::PI,
                                end_angle: end * std::f64::consts::PI,
                                stops,
                            },
                            transform: ctm,
                            blend,
                        });
                    }
                }
            }
            // PaintGlyph (10): set the clip glyph, recurse into the sub-paint.
            10 => {
                let sub = off + be24(d, off + 1) as usize;
                let gid = be16(d, off + 4);
                self.walk_paint(sub, ctm, Some(gid), blend, depth + 1, out);
            }
            // PaintColrGlyph (11): paint another base glyph's graph in place.
            11 => {
                let gid = be16(d, off + 1);
                if let Ok(i) = self.base_paints.binary_search_by_key(&gid, |p| p.0) {
                    let child = self.base_paints[i].1 as usize;
                    self.walk_paint(child, ctm, glyph, blend, depth + 1, out);
                }
            }
            // PaintTransform (12) / Var (13): child paint + an Affine2x3 record.
            // The Var affine carries a trailing `varIndexBase` whose +0..+5
            // deltas adjust the six matrix entries.
            12 | 13 => {
                let sub = off + be24(d, off + 1) as usize;
                let aff_off = off + be24(d, off + 4) as usize;
                let mut t = read_affine(d, aff_off);
                if format == 13 {
                    let vib = be32(d, aff_off + 24);
                    t.a += self.delta(vib, 0);
                    t.b += self.delta(vib, 1);
                    t.c += self.delta(vib, 2);
                    t.d += self.delta(vib, 3);
                    t.e += self.delta(vib, 4);
                    t.f += self.delta(vib, 5);
                }
                self.walk_paint(sub, ctm.then(t), glyph, blend, depth + 1, out);
            }
            // PaintTranslate (14) / Var (15).
            14 | 15 => {
                let sub = off + be24(d, off + 1) as usize;
                let mut dx = bei16(d, off + 4) as f64;
                let mut dy = bei16(d, off + 6) as f64;
                if format == 15 {
                    let vib = be32(d, off + 8);
                    dx += self.delta(vib, 0);
                    dy += self.delta(vib, 1);
                }
                let t = Affine {
                    e: dx,
                    f: dy,
                    ..Affine::identity()
                };
                self.walk_paint(sub, ctm.then(t), glyph, blend, depth + 1, out);
            }
            // PaintScale (16/17) and PaintScaleAroundCenter (18/19).
            16..=19 => {
                let sub = off + be24(d, off + 1) as usize;
                let mut sx = f2dot14(d, off + 4);
                let mut sy = f2dot14(d, off + 6);
                let around_center = format == 18 || format == 19;
                let (mut cx, mut cy) = if around_center {
                    (bei16(d, off + 8) as f64, bei16(d, off + 10) as f64)
                } else {
                    (0.0, 0.0)
                };
                if format == 17 || format == 19 {
                    let vib = be32(d, off + if around_center { 12 } else { 8 });
                    sx += self.delta(vib, 0) / 16384.0;
                    sy += self.delta(vib, 1) / 16384.0;
                    if around_center {
                        cx += self.delta(vib, 2);
                        cy += self.delta(vib, 3);
                    }
                }
                let t = scale_around(sx, sy, cx, cy);
                self.walk_paint(sub, ctm.then(t), glyph, blend, depth + 1, out);
            }
            // PaintScaleUniform (20/21) / PaintScaleUniformAroundCenter (22/23).
            20..=23 => {
                let sub = off + be24(d, off + 1) as usize;
                let mut s = f2dot14(d, off + 4);
                let around_center = format == 22 || format == 23;
                let (mut cx, mut cy) = if around_center {
                    (bei16(d, off + 6) as f64, bei16(d, off + 8) as f64)
                } else {
                    (0.0, 0.0)
                };
                if format == 21 || format == 23 {
                    let vib = be32(d, off + if around_center { 10 } else { 6 });
                    s += self.delta(vib, 0) / 16384.0;
                    if around_center {
                        cx += self.delta(vib, 1);
                        cy += self.delta(vib, 2);
                    }
                }
                let t = scale_around(s, s, cx, cy);
                self.walk_paint(sub, ctm.then(t), glyph, blend, depth + 1, out);
            }
            // PaintRotate (24/25) / PaintRotateAroundCenter (26/27). Angle in
            // F2Dot14 ×180° → radians = value × π.
            24..=27 => {
                let sub = off + be24(d, off + 1) as usize;
                let mut ang = f2dot14(d, off + 4);
                let around_center = format == 26 || format == 27;
                let (mut cx, mut cy) = if around_center {
                    (bei16(d, off + 6) as f64, bei16(d, off + 8) as f64)
                } else {
                    (0.0, 0.0)
                };
                if format == 25 || format == 27 {
                    let vib = be32(d, off + if around_center { 10 } else { 6 });
                    ang += self.delta(vib, 0) / 16384.0;
                    if around_center {
                        cx += self.delta(vib, 1);
                        cy += self.delta(vib, 2);
                    }
                }
                let t = rotate_around(ang * std::f64::consts::PI, cx, cy);
                self.walk_paint(sub, ctm.then(t), glyph, blend, depth + 1, out);
            }
            // PaintSkew (28/29) / PaintSkewAroundCenter (30/31). Skew angles in
            // F2Dot14 ×180°.
            28..=31 => {
                let sub = off + be24(d, off + 1) as usize;
                let mut sx_ang = f2dot14(d, off + 4);
                let mut sy_ang = f2dot14(d, off + 6);
                let around_center = format == 30 || format == 31;
                let (mut cx, mut cy) = if around_center {
                    (bei16(d, off + 8) as f64, bei16(d, off + 10) as f64)
                } else {
                    (0.0, 0.0)
                };
                if format == 29 || format == 31 {
                    let vib = be32(d, off + if around_center { 12 } else { 8 });
                    sx_ang += self.delta(vib, 0) / 16384.0;
                    sy_ang += self.delta(vib, 1) / 16384.0;
                    if around_center {
                        cx += self.delta(vib, 2);
                        cy += self.delta(vib, 3);
                    }
                }
                let kx = (sx_ang * std::f64::consts::PI).tan();
                let ky = (sy_ang * std::f64::consts::PI).tan();
                // Skew about centre: T(c)·skew·T(-c).
                let skew = Affine {
                    a: 1.0,
                    b: ky,
                    c: -kx,
                    d: 1.0,
                    e: 0.0,
                    f: 0.0,
                };
                let t = around(skew, cx, cy);
                self.walk_paint(sub, ctm.then(t), glyph, blend, depth + 1, out);
            }
            // PaintComposite (32): draw backdrop, then source with the composite
            // mode mapped to a PDF `/BM` blend mode applied to the source.
            32 => {
                let src = off + be24(d, off + 1) as usize;
                let mode = d.get(off + 4).copied().unwrap_or(3);
                let backdrop = off + be24(d, off + 5) as usize;
                self.walk_paint(backdrop, ctm, glyph, blend, depth + 1, out);
                let src_blend = BlendName::from_composite_mode(mode);
                self.walk_paint(src, ctm, glyph, src_blend, depth + 1, out);
            }
            _ => {} // unknown / unsupported paint format: skip
        }
    }
}

/// Read a signed F2Dot14 fixed-point value (1.14) as f64.
fn f2dot14(d: &[u8], o: usize) -> f64 {
    bei16(d, o) as f64 / 16384.0
}

/// Read a big-endian 24-bit unsigned offset.
fn be24(d: &[u8], o: usize) -> u32 {
    if o + 3 <= d.len() {
        ((d[o] as u32) << 16) | ((d[o + 1] as u32) << 8) | d[o + 2] as u32
    } else {
        0
    }
}

/// Read an `Affine2x3` record (six F16Dot16 fixed-point values, 16.16).
fn read_affine(d: &[u8], o: usize) -> Affine {
    let fx = |i: usize| {
        let raw = be32(d, o + i * 4) as i32;
        raw as f64 / 65536.0
    };
    Affine {
        a: fx(0),
        b: fx(1),
        c: fx(2),
        d: fx(3),
        e: fx(4),
        f: fx(5),
    }
}

/// Wrap `t` so it applies about the centre `(cx, cy)`: `T(c)·t·T(-c)`.
fn around(t: Affine, cx: f64, cy: f64) -> Affine {
    let to_c = Affine {
        e: cx,
        f: cy,
        ..Affine::identity()
    };
    let from_c = Affine {
        e: -cx,
        f: -cy,
        ..Affine::identity()
    };
    to_c.then(t).then(from_c)
}

/// A scale by `(sx, sy)` about `(cx, cy)`.
fn scale_around(sx: f64, sy: f64, cx: f64, cy: f64) -> Affine {
    let s = Affine {
        a: sx,
        b: 0.0,
        c: 0.0,
        d: sy,
        e: 0.0,
        f: 0.0,
    };
    around(s, cx, cy)
}

/// A rotation by `ang` radians about `(cx, cy)`.
fn rotate_around(ang: f64, cx: f64, cy: f64) -> Affine {
    let (s, c) = ang.sin_cos();
    let r = Affine {
        a: c,
        b: s,
        c: -s,
        d: c,
        e: 0.0,
        f: 0.0,
    };
    around(r, cx, cy)
}

// ---------------------------------------------------------------------------
// ItemVariationStore + DeltaSetIndexMap — OpenType font variations applied to
// the `PaintVar*` deltas (variable colour fonts).
// ---------------------------------------------------------------------------

/// One variation region's `(start, peak, end)` along a single axis (F2Dot14).
#[derive(Debug, Clone, Copy)]
struct AxisRegion {
    start: f64,
    peak: f64,
    end: f64,
}

/// One `ItemVariationData` subtable: the region indices it uses and a flat
/// `item_count × region_count` delta matrix.
#[derive(Debug, Clone)]
struct ItemVarData {
    region_indices: Vec<u16>,
    /// `deltas[item * region_count + region]`.
    deltas: Vec<f64>,
    region_count: usize,
}

/// An `ItemVariationStore` (+ optional `DeltaSetIndexMap`) resolving a
/// `varIndexBase + field` to a scalar delta for a given normalized instance.
#[derive(Debug, Clone)]
struct VarStore {
    /// `regions[r][axis]` — the variation regions.
    regions: Vec<Vec<AxisRegion>>,
    data: Vec<ItemVarData>,
    /// The `DeltaSetIndexMap`: `map[i] = (outer, inner)`. Empty ⇒ the var index
    /// is used directly as `outer = idx >> 16`, `inner = idx & 0xFFFF`.
    index_map: Vec<(u16, u16)>,
}

impl VarStore {
    /// Parse the `ItemVariationStore` at `store_off` and the `DeltaSetIndexMap`
    /// at `map_off` (both offsets from the `COLR` table start; `map_off == 0`
    /// means none). `None` if the store is malformed or not format 1.
    fn parse(d: &[u8], store_off: usize, map_off: usize) -> Option<VarStore> {
        if store_off + 8 > d.len() || be16(d, store_off) != 1 {
            return None; // only format 1 is defined
        }
        let region_list_off = store_off + be32(d, store_off + 2) as usize;
        let data_count = be16(d, store_off + 6) as usize;

        // VariationRegionList: axisCount(u16), regionCount(u16), then regions.
        if region_list_off + 4 > d.len() {
            return None;
        }
        let axis_count = be16(d, region_list_off) as usize;
        let region_count = be16(d, region_list_off + 2) as usize;
        let mut regions = Vec::with_capacity(region_count);
        for r in 0..region_count {
            let base = region_list_off + 4 + r * axis_count * 6;
            let mut axes = Vec::with_capacity(axis_count);
            for a in 0..axis_count {
                let o = base + a * 6;
                if o + 6 > d.len() {
                    return None;
                }
                axes.push(AxisRegion {
                    start: f2dot14(d, o),
                    peak: f2dot14(d, o + 2),
                    end: f2dot14(d, o + 4),
                });
            }
            regions.push(axes);
        }

        // ItemVariationData subtables.
        let mut data = Vec::with_capacity(data_count);
        for i in 0..data_count {
            let off_field = store_off + 8 + i * 4;
            if off_field + 4 > d.len() {
                break;
            }
            let sub = store_off + be32(d, off_field) as usize;
            if let Some(ivd) = Self::parse_item_var_data(d, sub) {
                data.push(ivd);
            } else {
                data.push(ItemVarData {
                    region_indices: Vec::new(),
                    deltas: Vec::new(),
                    region_count: 0,
                });
            }
        }

        let index_map = if map_off != 0 {
            parse_delta_set_index_map(d, map_off)
        } else {
            Vec::new()
        };

        Some(VarStore {
            regions,
            data,
            index_map,
        })
    }

    /// Parse one `ItemVariationData`: itemCount(u16), wordDeltaCount(u16),
    /// regionIndexCount(u16), regionIndexes[u16], then the delta sets. Bit 15 of
    /// `wordDeltaCount` (LONG_WORDS) makes the "word" deltas 32-bit.
    fn parse_item_var_data(d: &[u8], off: usize) -> Option<ItemVarData> {
        if off + 6 > d.len() {
            return None;
        }
        let item_count = be16(d, off) as usize;
        let word_delta_count_raw = be16(d, off + 2);
        let long_words = word_delta_count_raw & 0x8000 != 0;
        let word_count = (word_delta_count_raw & 0x7FFF) as usize;
        let region_count = be16(d, off + 4) as usize;
        let mut region_indices = Vec::with_capacity(region_count);
        for r in 0..region_count {
            let o = off + 6 + r * 2;
            if o + 2 > d.len() {
                return None;
            }
            region_indices.push(be16(d, o));
        }
        // Each delta-set row: `word_count` long deltas then `region_count -
        // word_count` short deltas. With LONG_WORDS, "long" = i32 and "short" =
        // i16; otherwise "long" = i16 and "short" = i8.
        let (long_size, short_size) = if long_words { (4, 2) } else { (2, 1) };
        let row_size =
            word_count * long_size + region_count.saturating_sub(word_count) * short_size;
        let rows_off = off + 6 + region_count * 2;
        let mut deltas = Vec::with_capacity(item_count * region_count);
        for item in 0..item_count {
            let row = rows_off + item * row_size;
            let mut p = row;
            for r in 0..region_count {
                let val = if r < word_count {
                    if long_words {
                        let v = be32(d, p) as i32 as f64;
                        p += 4;
                        v
                    } else {
                        let v = bei16(d, p) as f64;
                        p += 2;
                        v
                    }
                } else if long_words {
                    let v = bei16(d, p) as f64;
                    p += 2;
                    v
                } else {
                    let v = d.get(p).copied().unwrap_or(0) as i8 as f64;
                    p += 1;
                    v
                };
                deltas.push(val);
            }
        }
        Some(ItemVarData {
            region_indices,
            deltas,
            region_count,
        })
    }

    /// The scalar delta for variation index `var_index` at normalized `coords`.
    fn delta(&self, var_index: u32, coords: &[f64]) -> f64 {
        let (outer, inner) = if !self.index_map.is_empty() {
            match self.index_map.get(var_index as usize) {
                Some(&oi) => (oi.0 as usize, oi.1 as usize),
                None => return 0.0,
            }
        } else {
            ((var_index >> 16) as usize, (var_index & 0xFFFF) as usize)
        };
        let Some(ivd) = self.data.get(outer) else {
            return 0.0;
        };
        if ivd.region_count == 0 || inner >= ivd.deltas.len() / ivd.region_count.max(1) {
            return 0.0;
        }
        let base = inner * ivd.region_count;
        let mut sum = 0.0;
        for (k, &ri) in ivd.region_indices.iter().enumerate() {
            let Some(region) = self.regions.get(ri as usize) else {
                continue;
            };
            let scalar = region_scalar(region, coords);
            if scalar != 0.0 {
                sum += scalar * ivd.deltas[base + k];
            }
        }
        sum
    }
}

/// The interpolation scalar of a variation `region` at normalized `coords`
/// (the product of each axis's tent-function contribution).
fn region_scalar(region: &[AxisRegion], coords: &[f64]) -> f64 {
    let mut scalar = 1.0;
    for (axis, r) in region.iter().enumerate() {
        let coord = coords.get(axis).copied().unwrap_or(0.0);
        // A peak of 0 means this axis does not participate.
        if r.peak == 0.0 {
            continue;
        }
        if coord == r.peak {
            continue; // factor 1.0
        }
        if coord <= r.start || coord >= r.end {
            return 0.0; // outside the region's support
        }
        let factor = if coord < r.peak {
            (coord - r.start) / (r.peak - r.start)
        } else {
            (r.end - coord) / (r.end - r.peak)
        };
        scalar *= factor;
    }
    scalar
}

/// Parse a `DeltaSetIndexMap` (format 0 or 1) at `off` into a flat
/// `[(outerIndex, innerIndex)]`. Empty on malformed input.
fn parse_delta_set_index_map(d: &[u8], off: usize) -> Vec<(u16, u16)> {
    if off + 1 > d.len() {
        return Vec::new();
    }
    let format = d[off];
    // format 0: entryFormat(u8), mapCount(u16). format 1: entryFormat(u8),
    // mapCount(u32).
    let (entry_format, map_count, data_start) = match format {
        0 => {
            if off + 4 > d.len() {
                return Vec::new();
            }
            (d[off + 1], be16(d, off + 2) as usize, off + 4)
        }
        1 => {
            if off + 6 > d.len() {
                return Vec::new();
            }
            (d[off + 1], be32(d, off + 2) as usize, off + 6)
        }
        _ => return Vec::new(),
    };
    let inner_bits = (entry_format & 0x0F) as u32 + 1;
    let entry_size = ((entry_format & 0x30) >> 4) as usize + 1; // bytes per entry
    let inner_mask = (1u32 << inner_bits) - 1;
    let mut out = Vec::with_capacity(map_count);
    for i in 0..map_count {
        let o = data_start + i * entry_size;
        if o + entry_size > d.len() {
            break;
        }
        let mut entry = 0u32;
        for b in 0..entry_size {
            entry = (entry << 8) | d[o + b] as u32;
        }
        let outer = (entry >> inner_bits) as u16;
        let inner = (entry & inner_mask) as u16;
        out.push((outer, inner));
    }
    out
}

// ---------------------------------------------------------------------------
// CBDT / CBLC — embedded colour bitmap strikes (Google colour emoji).
// ---------------------------------------------------------------------------
//
// `CBLC` is the location index (a list of strikes, each pointing at glyph-id
// ranges via IndexSubTables); `CBDT` holds the actual image bytes. We resolve
// the largest strike and, for a glyph, return its PNG (bitmap formats 17/18/19
// embed a PNG, like `sbix`). Reuses the same bitmap placement path.

/// One glyph bitmap from `CBDT`: the PNG bytes, strike ppem, and the glyph's
/// integer bearings (pixels at the strike's ppem).
#[derive(Debug, Clone)]
pub struct CbdtGlyph {
    pub png: Vec<u8>,
    pub ppem: f64,
    pub bearing_x: f64,
    pub bearing_y: f64,
}

/// A `CBLC` IndexSubTable descriptor resolved to one glyph-id range.
#[derive(Debug, Clone)]
struct CblcRange {
    first: u16,
    last: u16,
    index_format: u16,
    image_format: u16,
    // Absolute offset (into `CBLC`) of the IndexSubTable body, just past its
    // 8-byte IndexSubHeader.
    body: usize,
}

/// The `CBLC`+`CBDT` tables resolved to the best (largest ppem) strike.
#[derive(Debug, Clone)]
pub struct Cbdt {
    cbdt: Vec<u8>,
    ppem: f64,
    /// The strike bit depth (1/2/4/8) for non-PNG grayscale/mono bitmaps.
    bit_depth: u8,
    ranges: Vec<CblcRange>,
}

impl Cbdt {
    /// Parse `CBLC`+`CBDT`, choosing the strike with the largest ppem. `None`
    /// if no usable strike is found.
    pub fn parse(cblc: &[u8], cbdt: &[u8]) -> Option<Cbdt> {
        // CBLC header: version(4) + numSizes(u32). Then BitmapSize[48] records.
        let num_sizes = be32(cblc, 4) as usize;
        let mut best: Option<(usize, u8)> = None; // (record offset, ppem)
        for i in 0..num_sizes {
            let rec = 8 + i * 48;
            if rec + 48 > cblc.len() {
                break;
            }
            // ppemX sits at byte 44 of the BitmapSize record (…, ppemX, ppemY,
            // bitDepth, flags).
            let ppem = cblc[rec + 44];
            if best.map(|(_, p)| ppem > p).unwrap_or(true) {
                best = Some((rec, ppem));
            }
        }
        let (rec, ppem) = best?;
        // BitmapSize: indexSubTableArrayOffset(u32) at 0, indexTablesSize(u32) at
        // 4, numberOfIndexSubTables(u32) at 8, …, ppemX/ppemY at 44/45, bitDepth
        // at 46.
        let ist_array = be32(cblc, rec) as usize;
        let num_ist = be32(cblc, rec + 8) as usize;
        let bit_depth = cblc.get(rec + 46).copied().unwrap_or(1).max(1);

        let mut ranges = Vec::new();
        for i in 0..num_ist {
            // IndexSubTableArray entry: firstGlyphIndex(u16), lastGlyphIndex(u16),
            // additionalOffsetToIndexSubtable(u32 from ist_array).
            let e = ist_array + i * 8;
            if e + 8 > cblc.len() {
                break;
            }
            let first = be16(cblc, e);
            let last = be16(cblc, e + 2);
            let sub = ist_array + be32(cblc, e + 4) as usize;
            if sub + 8 > cblc.len() {
                continue;
            }
            // IndexSubHeader: indexFormat(u16), imageFormat(u16), imageDataOffset(u32).
            let index_format = be16(cblc, sub);
            let image_format = be16(cblc, sub + 2);
            ranges.push(CblcRange {
                first,
                last,
                index_format,
                image_format,
                body: sub + 8,
            });
        }
        if ranges.is_empty() {
            return None;
        }
        Some(Cbdt {
            cbdt: cbdt.to_vec(),
            ppem: ppem as f64,
            bit_depth,
            ranges,
        })
    }

    /// Resolve `gid` to its `(cbdt_offset, byte_len, image_format)` record.
    /// Handles index formats 1 (4-byte offsets), 2 (constant size), 3 (2-byte
    /// offsets), 4 (sparse glyph→offset map) and 5 (sparse constant size).
    fn locate(&self, cblc: &[u8], gid: u16) -> Option<(usize, usize, u16)> {
        let r = self
            .ranges
            .iter()
            .find(|r| gid >= r.first && gid <= r.last)?;
        let local = (gid - r.first) as usize;
        // imageDataOffset (last field of the IndexSubHeader) — the CBDT base.
        let base = be32(cblc, r.body - 4) as usize;
        match r.index_format {
            // Format 1: 4-byte offsets into CBDT (sbitOffsets[glyphCount+1]).
            1 => {
                let o = r.body + local * 4;
                if o + 8 > cblc.len() {
                    return None;
                }
                let start = be32(cblc, o) as usize;
                let end = be32(cblc, o + 4) as usize;
                if end <= start {
                    return None;
                }
                Some((base + start, end - start, r.image_format))
            }
            // Format 2: constant-size images. imageSize(u32) then bigMetrics(8).
            2 => {
                if r.body + 4 > cblc.len() {
                    return None;
                }
                let size = be32(cblc, r.body) as usize;
                Some((base + local * size, size, r.image_format))
            }
            // Format 3: 2-byte offsets (sbitOffsets[glyphCount+1]).
            3 => {
                let o = r.body + local * 2;
                if o + 4 > cblc.len() {
                    return None;
                }
                let start = be16(cblc, o) as usize;
                let end = be16(cblc, o + 2) as usize;
                if end <= start {
                    return None;
                }
                Some((base + start, end - start, r.image_format))
            }
            // Format 4: sparse. numGlyphs(u32) then glyphIdOffsetPair[numGlyphs+1]
            // of (glyphId:u16, offset:u16). Find this gid, take its run length.
            4 => {
                let num = be32(cblc, r.body) as usize;
                let pairs = r.body + 4;
                for i in 0..num {
                    let p = pairs + i * 4;
                    if p + 6 > cblc.len() {
                        break;
                    }
                    if be16(cblc, p) == gid {
                        let start = be16(cblc, p + 2) as usize;
                        let end = be16(cblc, p + 6) as usize; // next pair's offset
                        if end <= start {
                            return None;
                        }
                        return Some((base + start, end - start, r.image_format));
                    }
                }
                None
            }
            // Format 5: sparse constant-size. imageSize(u32), bigMetrics(8),
            // numGlyphs(u32), glyphIdArray[numGlyphs] of u16. The CBDT index is
            // the glyph's position in that array.
            5 => {
                if r.body + 12 > cblc.len() {
                    return None;
                }
                let size = be32(cblc, r.body) as usize;
                let num = be32(cblc, r.body + 12) as usize;
                let ids = r.body + 16;
                for i in 0..num {
                    let o = ids + i * 2;
                    if o + 2 > cblc.len() {
                        break;
                    }
                    if be16(cblc, o) == gid {
                        return Some((base + i * size, size, r.image_format));
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// The colour-bitmap for `gid` (re-encoded to PNG), with its bearings, in
    /// the chosen strike. Decodes the PNG image formats (17/18/19) directly and
    /// the mono/grayscale formats (1/2/5/6/7) into PNG. `None` if absent.
    pub fn glyph(&self, cblc: &[u8], gid: u16) -> Option<CbdtGlyph> {
        let (off, len, image_format) = self.locate(cblc, gid)?;
        if off + len > self.cbdt.len() || len == 0 {
            return None;
        }
        let rec = &self.cbdt[off..off + len];
        match image_format {
            // PNG-bearing formats: extract the embedded PNG.
            17..=19 => {
                let (bx, by, png) = match image_format {
                    // smallGlyphMetrics(5) + dataLen(u32) + PNG.
                    17 if rec.len() >= 9 => (rec[2] as i8 as f64, rec[3] as i8 as f64, &rec[9..]),
                    // bigGlyphMetrics(8) + dataLen(u32) + PNG.
                    18 if rec.len() >= 12 => (rec[2] as i8 as f64, rec[3] as i8 as f64, &rec[12..]),
                    19 => (0.0, 0.0, rec), // PNG only (metrics from CBLC)
                    _ => return None,
                };
                const PNG_SIG: &[u8] = b"\x89PNG";
                let s = if png.len() >= 4 && &png[..4] == PNG_SIG {
                    0
                } else if png.len() >= 8 && &png[4..8] == PNG_SIG {
                    4
                } else {
                    png.windows(4)
                        .position(|w| w == PNG_SIG)
                        .filter(|&p| p <= 4)?
                };
                Some(CbdtGlyph {
                    png: png[s..].to_vec(),
                    ppem: self.ppem,
                    bearing_x: bx,
                    bearing_y: by,
                })
            }
            // Mono / grayscale bitmap formats → decode into PNG.
            //   1: smallMetrics(5) + byte-aligned bitmap
            //   2: smallMetrics(5) + bit-aligned bitmap
            //   5: bit-aligned bitmap only (metrics come from CBLC format 2/5)
            //   6: bigMetrics(8) + byte-aligned bitmap
            //   7: bigMetrics(8) + bit-aligned bitmap
            1 | 2 | 5 | 6 | 7 => {
                let (w, h, bx, by, bits) = self.bitmap_geometry(cblc, image_format, gid, rec)?;
                if w == 0 || h == 0 {
                    return None;
                }
                let byte_aligned = image_format == 1 || image_format == 6;
                let rgba = decode_bitmap_to_rgba(bits, w, h, self.bit_depth, byte_aligned)?;
                let png = crate::raster::png::encode_png(w as u32, h as u32, &rgba);
                Some(CbdtGlyph {
                    png,
                    ppem: self.ppem,
                    bearing_x: bx,
                    bearing_y: by,
                })
            }
            _ => None,
        }
    }

    /// Resolve a non-PNG record to `(width, height, bearingX, bearingY,
    /// bitmap_bytes)`. Formats 1/2 carry smallGlyphMetrics, 6/7 carry
    /// bigGlyphMetrics; format 5 has no metrics in CBDT so they come from the
    /// CBLC IndexSubTable (format 2/5) bigMetrics.
    fn bitmap_geometry<'a>(
        &self,
        cblc: &'a [u8],
        image_format: u16,
        gid: u16,
        rec: &'a [u8],
    ) -> Option<(usize, usize, f64, f64, &'a [u8])> {
        match image_format {
            1 | 2 => {
                // smallGlyphMetrics: height,width,bearingX,bearingY,advance.
                if rec.len() < 5 {
                    return None;
                }
                let h = rec[0] as usize;
                let w = rec[1] as usize;
                Some((w, h, rec[2] as i8 as f64, rec[3] as i8 as f64, &rec[5..]))
            }
            6 | 7 => {
                // bigGlyphMetrics: height,width,horiBearingX,horiBearingY,…(8).
                if rec.len() < 8 {
                    return None;
                }
                let h = rec[0] as usize;
                let w = rec[1] as usize;
                Some((w, h, rec[2] as i8 as f64, rec[3] as i8 as f64, &rec[8..]))
            }
            5 => {
                // Metrics live in the CBLC IndexSubTable (format 2 or 5)
                // bigMetrics at body+4.
                let r = self
                    .ranges
                    .iter()
                    .find(|r| gid >= r.first && gid <= r.last)?;
                let m = r.body + 4; // past imageSize(u32)
                if m + 8 > cblc.len() {
                    return None;
                }
                let h = cblc[m] as usize;
                let w = cblc[m + 1] as usize;
                Some((
                    w,
                    h,
                    cblc[m + 2] as i8 as f64,
                    cblc[m + 3] as i8 as f64,
                    rec,
                ))
            }
            _ => None,
        }
    }
}

/// Decode a mono/grayscale embedded-bitmap (`bit_depth` of 1/2/4/8) into 8-bit
/// RGBA. Pixels are the foreground colour (black) with coverage in the alpha
/// channel, matching how colour-emoji bitmaps composite over text. `byte_aligned`
/// pads each row to a whole byte (formats 1/6); otherwise rows are bit-packed
/// contiguously (formats 2/5/7).
fn decode_bitmap_to_rgba(
    bits: &[u8],
    w: usize,
    h: usize,
    bit_depth: u8,
    byte_aligned: bool,
) -> Option<Vec<u8>> {
    let bd = bit_depth as usize;
    if !(bd == 1 || bd == 2 || bd == 4 || bd == 8) {
        return None;
    }
    let max = ((1u32 << bd) - 1) as f64;
    let mut rgba = vec![0u8; w * h * 4];
    // Byte-aligned rows are padded up to a whole byte; bit-aligned rows pack
    // contiguously across the whole bitmap.
    let row_bits = if byte_aligned {
        (w * bd).div_ceil(8) * 8
    } else {
        w * bd
    };
    for y in 0..h {
        let mut cursor = y * row_bits;
        for x in 0..w {
            let sample = read_bits(bits, cursor, bd)?;
            cursor += bd;
            let coverage = (sample as f64 / max * 255.0).round() as u8;
            let i = (y * w + x) * 4;
            // Foreground black; bitmap coverage in the alpha channel.
            rgba[i] = 0;
            rgba[i + 1] = 0;
            rgba[i + 2] = 0;
            rgba[i + 3] = coverage;
        }
    }
    Some(rgba)
}

/// Read `count` bits (1..=8) starting at global bit offset `start` from a
/// big-endian bit stream (MSB first within each byte). `None` if out of range.
fn read_bits(data: &[u8], start: usize, count: usize) -> Option<u32> {
    let mut v = 0u32;
    for k in 0..count {
        let bit = start + k;
        let byte = bit / 8;
        let off = 7 - (bit % 8);
        let b = data.get(byte)?;
        v = (v << 1) | ((b >> off) & 1) as u32;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built COLR v0 + CPAL: base gid 5 → 2 layers (gid 10 palette 0 = red,
    /// gid 11 palette 1 = blue).
    fn fixture() -> (Vec<u8>, Vec<u8>) {
        // COLR: header(14) + 1 base record(6) + 2 layer records(8) = 28 bytes.
        let mut colr = vec![
            0, 0, // version 0
            0, 1, // numBaseGlyphRecords = 1
            0, 0, 0, 14, // baseGlyphRecordsOffset = 14
            0, 0, 0, 20, // layerRecordsOffset = 20
            0, 2, // numLayerRecords = 2
        ];
        colr.extend_from_slice(&[0, 5, 0, 0, 0, 2]); // base: gid 5, first 0, num 2
        colr.extend_from_slice(&[0, 10, 0, 0]); // layer: gid 10, palette 0
        colr.extend_from_slice(&[0, 11, 0, 1]); // layer: gid 11, palette 1

        // CPAL: header(12) + indices(2) + 2 records(8) = 22 bytes.
        let mut cpal = vec![
            0, 0, // version 0
            0, 2, // numPaletteEntries = 2
            0, 1, // numPalettes = 1
            0, 2, // numColorRecords = 2
            0, 0, 0, 14, // colorRecordsArrayOffset = 14
            0, 0, // colorRecordIndices[0] = 0
        ];
        cpal.extend_from_slice(&[0, 0, 255, 255]); // record 0: BGRA = red
        cpal.extend_from_slice(&[255, 0, 0, 255]); // record 1: BGRA = blue
        (colr, cpal)
    }

    #[test]
    fn parses_layers_and_palette_colours() {
        let (colr, cpal) = fixture();
        let cg = ColorGlyphs::parse(&colr, &cpal).expect("parsed");
        let layers = cg.layers(5).expect("gid 5 is a colour glyph");
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].gid, 10);
        assert_eq!(layers[0].rgb, [1.0, 0.0, 0.0], "layer 0 = red (palette 0)");
        assert_eq!(layers[1].gid, 11);
        assert_eq!(layers[1].rgb, [0.0, 0.0, 1.0], "layer 1 = blue (palette 1)");
        assert!(cg.layers(99).is_none(), "non-colour glyph yields None");
    }

    #[test]
    fn sbix_extracts_png_strike() {
        let mut sbix = vec![
            0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 12, // header + strikeOffsets[0]=12
            0, 32, 0, 72, // strike: ppem=32, ppi=72
            0, 0, 0, 20, 0, 0, 0, 20, 0, 0, 0, 36, 0, 0, 0, 36, // glyphDataOffsets[0..4]
            0, 5, 0, 0, b'p', b'n', b'g', b' ', // glyph 1: originX=5, originY=0, "png "
        ];
        sbix.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]); // PNG bytes
        let sb = Sbix::parse(&sbix, 3).expect("parsed");
        assert!(sb.glyph(0).is_none(), "glyph 0 is empty");
        let g = sb.glyph(1).expect("glyph 1 has a PNG bitmap");
        assert_eq!(g.ppem, 32.0);
        assert_eq!(g.origin_x, 5.0);
        assert_eq!(&g.png[..4], &[0x89, b'P', b'N', b'G'], "PNG signature");
        assert!(sb.glyph(2).is_none(), "glyph 2 is empty");
    }

    #[test]
    fn foreground_palette_index_flagged() {
        let (mut colr, cpal) = fixture();
        // Rewrite layer 1's palette index to 0xFFFF (foreground).
        let layer1 = 24; // layerRecordsOffset(20) + 1 record(4)
        colr[layer1 + 2] = 0xFF;
        colr[layer1 + 3] = 0xFF;
        let cg = ColorGlyphs::parse(&colr, &cpal).unwrap();
        let layers = cg.layers(5).unwrap();
        assert!(!layers[0].use_foreground && layers[1].use_foreground);
    }

    /// Hand-built COLR **v1** + CPAL: base gid 7 → PaintColrLayers of two
    /// PaintGlyph layers — gid 10 filled by a PaintSolid (palette 0), gid 11
    /// filled by a PaintLinearGradient (stops palette 1 → palette 2). Palette
    /// 0 = red, 1 = green, 2 = blue.
    fn colrv1_fixture() -> (Vec<u8>, Vec<u8>) {
        let f2 = |v: u16| [(v >> 8) as u8, (v & 0xFF) as u8];
        let one = f2(0x4000); // 1.0 in F2Dot14
        let mut colr: Vec<u8> = Vec::new();
        // v1 header (34 bytes). No v0 base/layer records; lists hold the v1 data.
        colr.extend_from_slice(&[0, 1]); // version = 1
        colr.extend_from_slice(&[0, 0]); // numBaseGlyphRecords = 0
        colr.extend_from_slice(&[0, 0, 0, 0]); // baseGlyphRecordsOffset = 0
        colr.extend_from_slice(&[0, 0, 0, 0]); // layerRecordsOffset = 0
        colr.extend_from_slice(&[0, 0]); // numLayerRecords = 0
        colr.extend_from_slice(&[0, 0, 0, 34]); // baseGlyphListOffset = 34
        colr.extend_from_slice(&[0, 0, 0, 50]); // layerListOffset = 50
        colr.extend_from_slice(&[0, 0, 0, 0]); // clipListOffset = 0
        colr.extend_from_slice(&[0, 0, 0, 0]); // varIndexMapOffset = 0
        colr.extend_from_slice(&[0, 0, 0, 0]); // itemVariationStoreOffset = 0
        assert_eq!(colr.len(), 34);
        // BaseGlyphList @34: numRecords(u32)=1 + one record (gid 7 → paint @44).
        colr.extend_from_slice(&[0, 0, 0, 1]); // numBaseGlyphPaintRecords = 1
        colr.extend_from_slice(&[0, 7]); // glyphID = 7
        colr.extend_from_slice(&[0, 0, 0, 10]); // paintOffset = 10 (→ 34+10 = 44)
        assert_eq!(colr.len(), 44);
        // PaintColrLayers @44: format 1, numLayers=2, firstLayerIndex(u32)=0.
        colr.extend_from_slice(&[1, 2, 0, 0, 0, 0]);
        assert_eq!(colr.len(), 50);
        // LayerList @50: numLayers(u32)=2 + two Offset32 (from list start = 50).
        colr.extend_from_slice(&[0, 0, 0, 2]); // numLayers = 2
        colr.extend_from_slice(&[0, 0, 0, 12]); // layer[0] → 50+12 = 62
        colr.extend_from_slice(&[0, 0, 0, 23]); // layer[1] → 50+23 = 73
        assert_eq!(colr.len(), 62);
        // PaintGlyph #0 @62: format 10, paintOffset(Offset24)=6 (→68), glyphID=10.
        colr.extend_from_slice(&[10, 0, 0, 6, 0, 10]);
        assert_eq!(colr.len(), 68);
        // PaintSolid @68: format 2, paletteIndex(u16)=0, alpha=1.0.
        colr.extend_from_slice(&[2, 0, 0]);
        colr.extend_from_slice(&one);
        assert_eq!(colr.len(), 73);
        // PaintGlyph #1 @73: format 10, paintOffset(Offset24)=6 (→79), glyphID=11.
        colr.extend_from_slice(&[10, 0, 0, 6, 0, 11]);
        assert_eq!(colr.len(), 79);
        // PaintLinearGradient @79: format 4, colorLineOffset(Offset24)=16 (→95),
        // p0=(0,0) p1=(100,0) p2=(0,100).
        colr.extend_from_slice(&[4, 0, 0, 16]);
        colr.extend_from_slice(&[0, 0, 0, 0]); // x0,y0
        colr.extend_from_slice(&[0, 100, 0, 0]); // x1=100,y1=0
        colr.extend_from_slice(&[0, 0, 0, 100]); // x2=0,y2=100
        assert_eq!(colr.len(), 95);
        // ColorLine @95: extend(u8)=0, numStops(u16)=2, two stops.
        colr.extend_from_slice(&[0, 0, 2]);
        colr.extend_from_slice(&[0, 0, 0, 1]); // stop0: offset 0, palette 1
        colr.extend_from_slice(&one); // stop0 alpha = 1.0
        colr.extend_from_slice(&one); // stop1: offset 1.0
        colr.extend_from_slice(&[0, 2]); // stop1: palette 2
        colr.extend_from_slice(&one); // stop1 alpha = 1.0

        // CPAL: 3 entries, palette 0 = red, 1 = green, 2 = blue (records BGRA).
        let mut cpal = vec![
            0, 0, // version 0
            0, 3, // numPaletteEntries = 3
            0, 1, // numPalettes = 1
            0, 3, // numColorRecords = 3
            0, 0, 0, 14, // colorRecordsArrayOffset = 14
            0, 0, // colorRecordIndices[0] = 0
        ];
        cpal.extend_from_slice(&[0, 0, 255, 255]); // red
        cpal.extend_from_slice(&[0, 255, 0, 255]); // green
        cpal.extend_from_slice(&[255, 0, 0, 255]); // blue
        (colr, cpal)
    }

    #[test]
    fn colrv1_flattens_solid_and_gradient_layers() {
        let (colr, cpal) = colrv1_fixture();
        let cg = Colrv1::parse(&colr, &cpal).expect("parsed v1");
        let layers = cg.layers(7).expect("gid 7 is a v1 colour glyph");
        assert_eq!(layers.len(), 2, "two flattened layers");

        // Layer 0: solid red on glyph 10.
        assert_eq!(layers[0].gid, 10);
        match &layers[0].fill {
            PaintFill::Solid { rgb, alpha, .. } => {
                assert_eq!(*rgb, [1.0, 0.0, 0.0], "palette 0 = red");
                assert!((alpha - 1.0).abs() < 1e-6);
            }
            other => panic!("layer 0 should be solid, got {other:?}"),
        }

        // Layer 1: linear gradient on glyph 11 from green → blue.
        assert_eq!(layers[1].gid, 11);
        match &layers[1].fill {
            PaintFill::Linear { p0, p1, stops, .. } => {
                assert_eq!(*p0, (0.0, 0.0));
                assert_eq!(*p1, (100.0, 0.0));
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[0].rgb, [0.0, 1.0, 0.0], "stop 0 = green (palette 1)");
                assert_eq!(stops[1].rgb, [0.0, 0.0, 1.0], "stop 1 = blue (palette 2)");
            }
            other => panic!("layer 1 should be a linear gradient, got {other:?}"),
        }

        assert!(cg.layers(99).is_none(), "non-v1 glyph yields None");
    }

    #[test]
    fn cbdt_extracts_png_strike() {
        // CBLC: header(8) + one BitmapSize(48). The IndexSubTableArray + one
        // IndexSubTable (format 1) point gid 1 at a PNG record in CBDT.
        let mut cblc = vec![0u8; 8];
        cblc[0..4].copy_from_slice(&[0, 3, 0, 0]); // version 3.0
        cblc[4..8].copy_from_slice(&[0, 0, 0, 1]); // numSizes = 1
                                                   // BitmapSize @8 (48 bytes): indexSubTableArrayOffset(u32)=56 at [0],
                                                   // numberOfIndexSubTables(u32)=1 at [8], ppemX at byte 44.
        let mut bsize = vec![0u8; 48];
        bsize[0..4].copy_from_slice(&[0, 0, 0, 56]); // ist array @56
        bsize[8..12].copy_from_slice(&[0, 0, 0, 1]); // numIndexSubTables = 1
        bsize[44] = 32; // ppemX = 32
        cblc.extend_from_slice(&bsize);
        assert_eq!(cblc.len(), 56);
        // IndexSubTableArray @56: firstGlyph=1, lastGlyph=1, offset(u32)=8 (→64).
        cblc.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 8]);
        assert_eq!(cblc.len(), 64);
        // IndexSubTable @64: indexFormat=1, imageFormat=17, imageDataOffset(u32)=0,
        // then sbitOffsets[2] = {0, len}. The PNG record lives at CBDT[0].
        cblc.extend_from_slice(&[0, 1, 0, 17, 0, 0, 0, 0]); // header
        let png = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
        // smallGlyphMetrics(5) + dataLen(4) + PNG.
        let rec_len = 5 + 4 + png.len();
        cblc.extend_from_slice(&[0, 0, 0, 0]); // sbitOffset[0] = 0
        cblc.extend_from_slice(&(rec_len as u32).to_be_bytes()); // sbitOffset[1]

        // CBDT: header(4) + the image record at offset 0… but offsets are from
        // imageDataOffset(0); place the record at the table start after header.
        // Format 17 record: height,width,bearingX,bearingY,advance(5) +
        // dataLen(u32) + PNG.
        let mut cbdt = Vec::new();
        cbdt.extend_from_slice(&[32, 32, 3, 30, 34]); // metrics (bearingX=3, bearingY=30)
        cbdt.extend_from_slice(&(png.len() as u32).to_be_bytes());
        cbdt.extend_from_slice(&png);

        let cb = Cbdt::parse(&cblc, &cbdt).expect("parsed CBLC/CBDT");
        assert!(cb.glyph(&cblc, 0).is_none(), "gid 0 outside the range");
        let g = cb.glyph(&cblc, 1).expect("gid 1 has a PNG bitmap");
        assert_eq!(g.ppem, 32.0);
        assert_eq!(g.bearing_x, 3.0);
        assert_eq!(g.bearing_y, 30.0);
        assert_eq!(&g.png[..4], &[0x89, b'P', b'N', b'G'], "PNG signature");
    }

    /// CPAL with `n` opaque entries, colours indexed 0..n given as RGB triplets
    /// (stored BGRA). Used by the COLR v1 fixtures below.
    fn cpal_rgb(colours: &[[u8; 3]]) -> Vec<u8> {
        let n = colours.len() as u16;
        let mut cpal = vec![0, 0];
        cpal.extend_from_slice(&n.to_be_bytes()); // numPaletteEntries
        cpal.extend_from_slice(&[0, 1]); // numPalettes
        cpal.extend_from_slice(&n.to_be_bytes()); // numColorRecords
        cpal.extend_from_slice(&[0, 0, 0, 14]); // colorRecordsArrayOffset
        cpal.extend_from_slice(&[0, 0]); // colorRecordIndices[0]
        for c in colours {
            cpal.extend_from_slice(&[c[2], c[1], c[0], 255]); // BGRA
        }
        cpal
    }

    /// A COLR v1 header (34 bytes) with the given list/store offsets.
    fn colrv1_header(base_list: u32, layer_list: u32, var_idx_map: u32, var_store: u32) -> Vec<u8> {
        let mut h = vec![0, 1, 0, 0]; // version 1, numBaseGlyphRecords 0
        h.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // v0 offsets/count
        h.extend_from_slice(&base_list.to_be_bytes());
        h.extend_from_slice(&layer_list.to_be_bytes());
        h.extend_from_slice(&[0, 0, 0, 0]); // clipListOffset
        h.extend_from_slice(&var_idx_map.to_be_bytes());
        h.extend_from_slice(&var_store.to_be_bytes());
        debug_assert_eq!(h.len(), 34);
        h
    }

    #[test]
    fn colrv1_sweep_gradient_flattened() {
        // base gid 3 → PaintGlyph(3) → PaintSweepGradient(start 0°, end 180°,
        // stops palette 0 → palette 1). Palette 0 = red, 1 = blue.
        let one = 0x4000u16.to_be_bytes();
        let mut colr = colrv1_header(34, 0, 0, 0);
        // BaseGlyphList @34: 1 record (gid 3 → paint @44).
        colr.extend_from_slice(&[0, 0, 0, 1, 0, 3, 0, 0, 0, 10]);
        // PaintGlyph @44: format 10, paintOffset(O24)=6 (→50), glyphID=3.
        colr.extend_from_slice(&[10, 0, 0, 6, 0, 3]);
        // PaintSweepGradient @50: format 8, colorLineOffset(O24)=12 (→62),
        // centerX=10, centerY=20, startAngle=0.0, endAngle=1.0 (×180° = 180°).
        colr.extend_from_slice(&[8, 0, 0, 12]); // format + colorLineOffset
        colr.extend_from_slice(&[0, 10, 0, 20]); // centerX, centerY
        colr.extend_from_slice(&[0, 0]); // startAngle = 0
        colr.extend_from_slice(&one); // endAngle = 1.0
                                      // ColorLine @62: extend 0, 2 stops (palette 0 @0.0, palette 1 @1.0).
        colr.extend_from_slice(&[0, 0, 2]);
        colr.extend_from_slice(&[0, 0, 0, 0]); // stop0: offset 0, palette 0
        colr.extend_from_slice(&one);
        colr.extend_from_slice(&one); // stop1: offset 1.0
        colr.extend_from_slice(&[0, 1]); // palette 1
        colr.extend_from_slice(&one);

        let cpal = cpal_rgb(&[[255, 0, 0], [0, 0, 255]]);
        let cg = Colrv1::parse(&colr, &cpal).expect("v1 parsed");
        let layers = cg.layers(3).expect("gid 3 is a v1 glyph");
        assert_eq!(layers.len(), 1);
        match &layers[0].fill {
            PaintFill::Sweep {
                center,
                start_angle,
                end_angle,
                stops,
            } => {
                assert_eq!(*center, (10.0, 20.0));
                assert!((start_angle - 0.0).abs() < 1e-9, "start 0 rad");
                assert!(
                    (end_angle - std::f64::consts::PI).abs() < 1e-9,
                    "end = π rad (180°)"
                );
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[0].rgb, [1.0, 0.0, 0.0]);
                assert_eq!(stops[1].rgb, [0.0, 0.0, 1.0]);
            }
            other => panic!("expected a sweep gradient, got {other:?}"),
        }
    }

    #[test]
    fn colrv1_composite_maps_blend_mode() {
        // base gid 2 → PaintComposite(SCREEN=9) of backdrop=PaintGlyph(2)→
        // PaintSolid(palette 0) and source=PaintGlyph(2)→PaintSolid(palette 1).
        let one = 0x4000u16.to_be_bytes();
        let mut colr = colrv1_header(34, 0, 0, 0);
        // BaseGlyphList @34: gid 2 → paint @44.
        colr.extend_from_slice(&[0, 0, 0, 1, 0, 2, 0, 0, 0, 10]);
        // PaintComposite @44: format 32, sourcePaintOffset(O24), compositeMode,
        // backdropPaintOffset(O24). source @55, backdrop @61.
        // @44 layout: [32][src O24][mode u8][backdrop O24] = 8 bytes → ends @52.
        colr.extend_from_slice(&[32]); // format
        colr.extend_from_slice(&[0, 0, 11]); // sourcePaintOffset = 11 (→55)
        colr.extend_from_slice(&[9]); // compositeMode = SCREEN
        colr.extend_from_slice(&[0, 0, 17]); // backdropPaintOffset = 17 (→61)
                                             // pad to @55 (we are at @52): 3 bytes.
        colr.extend_from_slice(&[0, 0, 0]);
        // source PaintGlyph @55: format 10, sub O24=6 (→61... clash). Instead point
        // the source glyph's solid to its own paint. Use sub=12 (→67).
        // Rewrite: @55 PaintGlyph(gid 2) sub=12 → PaintSolid @67.
        colr.extend_from_slice(&[10, 0, 0, 12, 0, 2]); // @55..61
                                                       // backdrop PaintGlyph @61: format 10, sub=11 (→72), gid 2.
        colr.extend_from_slice(&[10, 0, 0, 11, 0, 2]); // @61..67
                                                       // source PaintSolid @67: palette 1.
        colr.extend_from_slice(&[2, 0, 1]);
        colr.extend_from_slice(&one); // @67..72
                                      // backdrop PaintSolid @72: palette 0.
        colr.extend_from_slice(&[2, 0, 0]);
        colr.extend_from_slice(&one); // @72..77

        let cpal = cpal_rgb(&[[255, 0, 0], [0, 255, 0]]);
        let cg = Colrv1::parse(&colr, &cpal).expect("v1 parsed");
        let layers = cg.layers(2).expect("gid 2 is a v1 glyph");
        assert_eq!(layers.len(), 2, "backdrop + source");
        // Backdrop is drawn first (Normal), source second (Screen blend).
        assert_eq!(layers[0].blend, BlendName::Normal, "backdrop = normal");
        assert_eq!(layers[1].blend, BlendName::Screen, "source = screen");
        match (&layers[0].fill, &layers[1].fill) {
            (PaintFill::Solid { rgb: a, .. }, PaintFill::Solid { rgb: b, .. }) => {
                assert_eq!(*a, [1.0, 0.0, 0.0], "backdrop red");
                assert_eq!(*b, [0.0, 1.0, 0.0], "source green");
            }
            _ => panic!("expected two solid fills"),
        }
    }

    #[test]
    fn colrv1_variable_delta_changes_alpha() {
        // base gid 1 → PaintGlyph(1) → PaintVarSolid(palette 0, alpha 0.5,
        // varIndexBase 0). One axis, one region (peak +1.0), one ItemVarData whose
        // single delta row scales alpha by +0.5 (F2Dot14 8192) at the peak.
        let half = 0x2000u16.to_be_bytes(); // 0.5 in F2Dot14
                                            // Lay out: header(34) + BaseGlyphList(10) + PaintGlyph(6) + PaintVarSolid(9)
                                            // then the ItemVariationStore. Compute the store offset.
        let mut colr = Vec::new();
        // placeholder header; patched after we know the store offset.
        colr.extend_from_slice(&colrv1_header(34, 0, 0, 0));
        colr.extend_from_slice(&[0, 0, 0, 1, 0, 1, 0, 0, 0, 10]); // BaseGlyphList @34
        colr.extend_from_slice(&[10, 0, 0, 6, 0, 1]); // PaintGlyph @44 → sub @50
                                                      // PaintVarSolid @50: format 3, paletteIndex 0, alpha 0.5, varIndexBase 0.
        colr.extend_from_slice(&[3, 0, 0]);
        colr.extend_from_slice(&half);
        colr.extend_from_slice(&[0, 0, 0, 0]); // varIndexBase = 0 → @59
        let store_off = colr.len() as u32; // ItemVariationStore starts here (@59)
                                           // ItemVariationStore: format 1, variationRegionListOffset(u32),
                                           // itemVariationDataCount(u16)=1, itemVariationDataOffsets[1](u32).
                                           // Layout (relative to store_off):
                                           //   0: format(2) = 1
                                           //   2: regionListOffset(4)
                                           //   6: dataCount(2) = 1
                                           //   8: dataOffset[0](4)
                                           //   12: VariationRegionList
                                           //   ...: ItemVariationData
        let region_list_rel = 12u32;
        // RegionList: axisCount(2)=1, regionCount(2)=1, region[1][1] of
        // start,peak,end (F2Dot14): 0.0, 1.0, 1.0 → 12+4 = 16 bytes; IVD @ rel 22.
        let ivd_rel = 22u32;
        colr.extend_from_slice(&[0, 1]); // format 1
        colr.extend_from_slice(&region_list_rel.to_be_bytes());
        colr.extend_from_slice(&[0, 1]); // dataCount = 1
        colr.extend_from_slice(&ivd_rel.to_be_bytes()); // dataOffset[0]
                                                        // VariationRegionList @ store+12.
        colr.extend_from_slice(&[0, 1]); // axisCount = 1
        colr.extend_from_slice(&[0, 1]); // regionCount = 1
        colr.extend_from_slice(&[0, 0]); // start = 0.0
        colr.extend_from_slice(&0x4000u16.to_be_bytes()); // peak = 1.0
        colr.extend_from_slice(&0x4000u16.to_be_bytes()); // end = 1.0
                                                          // ItemVariationData @ store+22: itemCount(2)=1, wordDeltaCount(2)=0,
                                                          // regionIndexCount(2)=1, regionIndexes[1]=0, then deltaSet[1][1] (i8).
        colr.extend_from_slice(&[0, 1]); // itemCount = 1
        colr.extend_from_slice(&[0, 0]); // wordDeltaCount = 0 (all short = i8)
        colr.extend_from_slice(&[0, 1]); // regionIndexCount = 1
        colr.extend_from_slice(&[0, 0]); // regionIndexes[0] = 0
                                         // delta row (1 item × 1 region): +8192 alpha (F2Dot14). But the delta is
                                         // stored in font units for the field; alpha deltas are F2Dot14, so a
                                         // value of 0x2000 (=8192) means +0.5. As an i8 that overflows, so use a
                                         // word delta instead — switch wordDeltaCount to 1.
                                         // (patched below: rebuild IVD with a word delta)
                                         // Patch: overwrite wordDeltaCount to 1 and append a 16-bit delta.
        let ivd_abs = store_off as usize + ivd_rel as usize;
        colr[ivd_abs + 2] = 0; // wordDeltaCount high byte
        colr[ivd_abs + 3] = 1; // wordDeltaCount = 1 (one long/word delta = i16)
        colr.extend_from_slice(&0x2000i16.to_be_bytes()); // delta = +8192 → +0.5 alpha

        // Patch the header's varStoreOffset (bytes 30..34).
        colr[30..34].copy_from_slice(&store_off.to_be_bytes());

        let cpal = cpal_rgb(&[[255, 0, 0]]);

        // Default instance (no coords): alpha = 0.5.
        let default = Colrv1::parse(&colr, &cpal).expect("v1 parsed");
        let d_layers = default.layers(1).expect("gid 1");
        let a_default = match &d_layers[0].fill {
            PaintFill::Solid { alpha, .. } => *alpha,
            _ => panic!("solid"),
        };
        assert!((a_default - 0.5).abs() < 1e-6, "default alpha = 0.5");

        // Instance at axis = +1.0 (peak): alpha = 0.5 + 0.5 = 1.0.
        let varied = Colrv1::parse(&colr, &cpal).unwrap().with_coords(vec![1.0]);
        let v_layers = varied.layers(1).expect("gid 1");
        let a_varied = match &v_layers[0].fill {
            PaintFill::Solid { alpha, .. } => *alpha,
            _ => panic!("solid"),
        };
        assert!(
            (a_varied - 1.0).abs() < 1e-6,
            "delta applied: alpha 0.5 → 1.0, got {a_varied}"
        );
        assert!(a_varied > a_default, "variation delta increased the alpha");
    }

    #[test]
    fn colrv1_linear_carries_p2_rotation_reference() {
        // The Linear fill must retain p2 so the renderer can model the rotation.
        let (colr, cpal) = colrv1_fixture(); // p2 = (0, 100)
        let cg = Colrv1::parse(&colr, &cpal).unwrap();
        let layers = cg.layers(7).unwrap();
        match &layers[1].fill {
            PaintFill::Linear { p0, p1, p2, .. } => {
                assert_eq!(*p0, (0.0, 0.0));
                assert_eq!(*p1, (100.0, 0.0));
                assert_eq!(*p2, (0.0, 100.0), "rotation reference preserved");
            }
            other => panic!("expected a linear gradient, got {other:?}"),
        }
    }

    #[test]
    fn cbdt_format4_index_and_non_png_bitmap() {
        // CBLC format-4 sparse index → CBDT format-1 (smallMetrics + byte-aligned
        // 1-bpp bitmap). gid 7 maps to a 2×2 checkerboard.
        let mut cblc = vec![0u8; 8];
        cblc[0..4].copy_from_slice(&[0, 3, 0, 0]); // version 3.0
        cblc[4..8].copy_from_slice(&[0, 0, 0, 1]); // numSizes = 1
                                                   // BitmapSize @8 (48): indexSubTableArrayOffset=56, numIndexSubTables=1,
                                                   // ppemX=16 @44, bitDepth=1 @46.
        let mut bsize = vec![0u8; 48];
        bsize[0..4].copy_from_slice(&[0, 0, 0, 56]); // ist array @56
        bsize[8..12].copy_from_slice(&[0, 0, 0, 1]); // numIndexSubTables
        bsize[44] = 16; // ppemX
        bsize[46] = 1; // bitDepth = 1 (mono)
        cblc.extend_from_slice(&bsize); // @8..56
                                        // IndexSubTableArray @56: firstGlyph=7, lastGlyph=7, offset=8 (→64).
        cblc.extend_from_slice(&[0, 7, 0, 7, 0, 0, 0, 8]); // @56..64
                                                           // IndexSubTable @64: indexFormat=4, imageFormat=1, imageDataOffset=0,
                                                           // then format-4 body: numGlyphs(u32)=1, pairs[(gid 7,off 0),(sentinel,off len)].
        cblc.extend_from_slice(&[0, 4, 0, 1, 0, 0, 0, 0]); // header @64..72
        cblc.extend_from_slice(&[0, 0, 0, 1]); // numGlyphs = 1
                                               // smallMetrics(5) + byte-aligned 2×2 1bpp bitmap (2 bytes, 1 row/byte).
        let rec_len = 5 + 2;
        // glyphIdOffsetPair[numGlyphs+1]: each is (glyphId u16, offset u16).
        cblc.extend_from_slice(&[0, 7, 0, 0]); // pair[0]: gid 7, offset 0
        cblc.extend_from_slice(&[0xFF, 0xFF]); // pair[1]: sentinel glyphId
        cblc.extend_from_slice(&(rec_len as u16).to_be_bytes()); // pair[1]: offset = run end

        // CBDT: format-1 record at offset 0.
        //   smallGlyphMetrics: height=2,width=2,bearingX=1,bearingY=2,advance=2.
        //   byte-aligned bitmap: row0 = 0b10000000, row1 = 0b01000000.
        let mut cbdt = Vec::new();
        cbdt.extend_from_slice(&[2, 2, 1, 2, 2]); // smallMetrics
        cbdt.extend_from_slice(&[0b1000_0000, 0b0100_0000]); // 2 rows, top-left + below-right

        let cb = Cbdt::parse(&cblc, &cbdt).expect("CBLC/CBDT parsed");
        let g = cb.glyph(&cblc, 7).expect("gid 7 decodes a bitmap");
        assert_eq!(g.ppem, 16.0);
        assert_eq!(g.bearing_x, 1.0);
        assert_eq!(g.bearing_y, 2.0);
        // The mono bitmap was re-encoded to a 2×2 PNG.
        assert_eq!(&g.png[..4], &[0x89, b'P', b'N', b'G'], "re-encoded PNG");
        // Decode the PNG back and verify the checkerboard alpha (foreground set
        // at (0,0) and (1,1), clear elsewhere).
        let img = crate::raster::png_decode::decode_png(&g.png).expect("decode");
        assert_eq!((img.width, img.height), (2, 2));
        let alpha = |x: usize, y: usize| img.rgba[(y * 2 + x) * 4 + 3];
        assert_eq!(alpha(0, 0), 255, "top-left set");
        assert_eq!(alpha(1, 0), 0, "top-right clear");
        assert_eq!(alpha(0, 1), 0, "bottom-left clear");
        assert_eq!(alpha(1, 1), 255, "bottom-right set");
    }

    // ── pure helpers ─────────────────────────────────────────────────────────

    #[test]
    fn integer_readers_big_endian() {
        let d = [0x12u8, 0x34, 0x56, 0x78];
        assert_eq!(be16(&d, 0), 0x1234);
        assert_eq!(be24(&d, 0), 0x0012_3456);
        assert_eq!(be32(&d, 0), 0x1234_5678);
        assert_eq!(bei16(&[0xFF, 0xFF], 0), -1);
        // Out-of-range reads return 0 (defensive).
        assert_eq!(be16(&d, 99), 0);
        assert_eq!(be24(&d, 99), 0);
        assert_eq!(be32(&d, 99), 0);
    }

    #[test]
    fn f2dot14_and_f16dot16() {
        // F2Dot14: 0x4000 = 1.0, 0xC000 = -1.0
        assert_eq!(f2dot14(&[0x40, 0x00], 0), 1.0);
        assert_eq!(f2dot14(&[0xC0, 0x00], 0), -1.0);
        // read_affine: six 16.16 fixed. 0x0001_0000 = 1.0.
        let mut d = Vec::new();
        for v in [1.0f64, 0.0, 0.0, 1.0, 2.0, 3.0] {
            d.extend_from_slice(&((v * 65536.0) as i32).to_be_bytes());
        }
        let a = read_affine(&d, 0);
        assert_eq!((a.a, a.d, a.e, a.f), (1.0, 1.0, 2.0, 3.0));
    }

    #[test]
    fn affine_identity_then_compose() {
        let id = Affine::identity();
        let t = Affine {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 2.0,
            e: 1.0,
            f: 1.0,
        };
        // identity ∘ t == t
        let r = id.then(t);
        assert_eq!((r.a, r.d, r.e, r.f), (2.0, 2.0, 1.0, 1.0));
    }

    #[test]
    fn scale_and_rotate_around_centre() {
        // Scale 2× about (10,10): the centre maps to itself.
        let s = scale_around(2.0, 2.0, 10.0, 10.0);
        let map = |t: &Affine, x: f64, y: f64| (t.a * x + t.c * y + t.e, t.b * x + t.d * y + t.f);
        let (cx, cy) = map(&s, 10.0, 10.0);
        assert!((cx - 10.0).abs() < 1e-9 && (cy - 10.0).abs() < 1e-9);
        // A point at +1 in x maps to +2 from centre.
        let (px, _) = map(&s, 11.0, 10.0);
        assert!((px - 12.0).abs() < 1e-9);
        // Rotate by π about origin: (1,0) → (≈ -1? depends on sign); centre fixed.
        let rot = rotate_around(std::f64::consts::PI, 5.0, 5.0);
        let (rx, ry) = map(&rot, 5.0, 5.0);
        assert!((rx - 5.0).abs() < 1e-9 && (ry - 5.0).abs() < 1e-9);
    }

    #[test]
    fn blend_name_pdf_tokens_and_composite_modes() {
        assert_eq!(BlendName::Normal.pdf_name(), b"Normal");
        assert_eq!(BlendName::Multiply.pdf_name(), b"Multiply");
        assert_eq!(BlendName::SoftLight.pdf_name(), b"SoftLight");
        assert_eq!(BlendName::Exclusion.pdf_name(), b"Exclusion");
        // Composite-mode mapping: Porter-Duff (0..=8) → Normal; separable modes.
        assert!(matches!(
            BlendName::from_composite_mode(3),
            BlendName::Normal
        ));
        assert!(matches!(
            BlendName::from_composite_mode(9),
            BlendName::Screen
        ));
        assert!(matches!(
            BlendName::from_composite_mode(19),
            BlendName::Multiply
        ));
        assert!(matches!(
            BlendName::from_composite_mode(200),
            BlendName::Normal
        ));
    }

    #[test]
    fn region_scalar_triangular_support() {
        let region = [AxisRegion {
            start: -1.0,
            peak: 0.0,
            end: 1.0,
        }];
        // peak == 0 → axis does not participate → full factor.
        assert_eq!(region_scalar(&region, &[0.5]), 1.0);
        let region = [AxisRegion {
            start: 0.0,
            peak: 0.5,
            end: 1.0,
        }];
        assert_eq!(region_scalar(&region, &[0.5]), 1.0); // at peak
        assert!((region_scalar(&region, &[0.25]) - 0.5).abs() < 1e-9); // rising
        assert!((region_scalar(&region, &[0.75]) - 0.5).abs() < 1e-9); // falling
        assert_eq!(region_scalar(&region, &[2.0]), 0.0); // outside support
    }

    #[test]
    fn read_bits_msb_first() {
        let data = [0b1010_1100u8, 0b0011_0000];
        assert_eq!(read_bits(&data, 0, 4), Some(0b1010));
        assert_eq!(read_bits(&data, 4, 4), Some(0b1100));
        assert_eq!(read_bits(&data, 8, 4), Some(0b0011));
        // Past the end → None.
        assert_eq!(read_bits(&data, 8, 100), None);
    }

    #[test]
    fn parse_cpal_palette_reads_bgra_records() {
        let (_, cpal) = fixture();
        let pal = parse_cpal_palette(&cpal);
        assert_eq!(pal.len(), 2);
        // record 0 = red (BGRA 0,0,255,255) → RGBA [1,0,0,1]
        assert_eq!(pal[0], [1.0, 0.0, 0.0, 1.0]);
        // record 1 = blue (BGRA 255,0,0,255) → RGBA [0,0,1,1]
        assert_eq!(pal[1], [0.0, 0.0, 1.0, 1.0]);
        // numPalettes == 0 → empty.
        let mut empty = cpal.clone();
        empty[4] = 0;
        empty[5] = 0;
        assert!(parse_cpal_palette(&empty).is_empty());
    }
}
