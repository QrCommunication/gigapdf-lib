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
        ((d[o] as u32) << 24) | ((d[o + 1] as u32) << 16) | ((d[o + 2] as u32) << 8) | d[o + 3] as u32
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
        let num_entries = be16(cpal, 2) as usize;
        let num_palettes = be16(cpal, 4) as usize;
        let num_records = be16(cpal, 6) as usize;
        let records_off = be32(cpal, 8) as usize;
        let mut palette = Vec::new();
        if num_palettes > 0 {
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
                // Records are BGRA.
                palette.push([
                    cpal[o + 2] as f64 / 255.0,
                    cpal[o + 1] as f64 / 255.0,
                    cpal[o] as f64 / 255.0,
                    cpal[o + 3] as f64 / 255.0,
                ]);
            }
        }

        if bases.is_empty() || palette.is_empty() {
            return None;
        }
        Some(ColorGlyphs { bases, layers, palette })
    }

    /// The colour layers of `base_gid`, or `None` if it isn't a colour glyph.
    pub fn layers(&self, base_gid: u16) -> Option<Vec<Layer>> {
        let idx = self.bases.binary_search_by_key(&base_gid, |b| b.0).ok()?;
        let (_, first, num) = self.bases[idx];
        let mut out = Vec::with_capacity(num as usize);
        for i in 0..num as usize {
            let (gid, pi) = *self.layers.get(first as usize + i)?;
            if pi == 0xFFFF {
                out.push(Layer { gid, rgb: [0.0, 0.0, 0.0], alpha: 1.0, use_foreground: true });
            } else {
                let c = self.palette.get(pi as usize).copied().unwrap_or([0.0, 0.0, 0.0, 1.0]);
                out.push(Layer { gid, rgb: [c[0], c[1], c[2]], alpha: c[3], use_foreground: false });
            }
        }
        Some(out)
    }
}

fn bei16(d: &[u8], o: usize) -> i16 {
    be16(d, o) as i16
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
        Some(Sbix { data: sbix.to_vec(), strike_off, ppem: ppem as f64, num_glyphs })
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
}
