//! TrueType / OpenType (SFNT) outline extraction — zero dependencies.
//!
//! Parses just enough of an embedded font program (`/FontFile2`) to turn a
//! glyph id into filled contours for the rasterizer: the `head`, `maxp`, `loca`,
//! `glyf`, `hmtx` and `cmap` tables. Glyph outlines are quadratic-Bézier
//! contours in font units; the renderer scales them by `1/units_per_em · size`.

fn be16(d: &[u8], o: usize) -> u16 {
    if o + 2 <= d.len() {
        ((d[o] as u16) << 8) | d[o + 1] as u16
    } else {
        0
    }
}

fn bei16(d: &[u8], o: usize) -> i16 {
    be16(d, o) as i16
}

fn be32(d: &[u8], o: usize) -> u32 {
    if o + 4 <= d.len() {
        ((d[o] as u32) << 24) | ((d[o + 1] as u32) << 16) | ((d[o + 2] as u32) << 8) | d[o + 3] as u32
    } else {
        0
    }
}

/// A parsed TrueType font program.
#[derive(Debug, Clone)]
pub struct TrueTypeFont {
    data: Vec<u8>,
    units_per_em: u16,
    num_glyphs: u16,
    loca: Vec<u32>,
    glyf: usize,
    hmtx: usize,
    num_h_metrics: u16,
    cmap: Vec<CmapSub>,
    /// `(offset, len)` of the `COLR` / `CPAL` colour-font tables, if present.
    colr: Option<(usize, usize)>,
    cpal: Option<(usize, usize)>,
    /// `(offset, len)` of the `sbix` bitmap-emoji table, if present.
    sbix: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
struct CmapSub {
    offset: usize,
    /// Higher score = preferred (Unicode tables first).
    score: u8,
}

impl TrueTypeFont {
    /// Parse a font program. Returns `None` if the essential tables are absent.
    pub fn parse(bytes: &[u8]) -> Option<TrueTypeFont> {
        let data = bytes.to_vec();
        // `ttcf` collections: use the first font.
        let base = if &data.get(0..4)? == b"ttcf" {
            be32(&data, 12) as usize
        } else {
            0
        };
        let num_tables = be16(&data, base + 4) as usize;
        let mut tables: std::collections::BTreeMap<[u8; 4], (usize, usize)> =
            std::collections::BTreeMap::new();
        for i in 0..num_tables {
            let rec = base + 12 + i * 16;
            if rec + 16 > data.len() {
                break;
            }
            let mut tag = [0u8; 4];
            tag.copy_from_slice(&data[rec..rec + 4]);
            let off = be32(&data, rec + 8) as usize;
            let len = be32(&data, rec + 12) as usize;
            tables.insert(tag, (off, len));
        }

        let head = tables.get(b"head")?.0;
        let maxp = tables.get(b"maxp")?.0;
        let (glyf, _) = *tables.get(b"glyf")?;
        let loca_off = tables.get(b"loca")?.0;

        let units_per_em = be16(&data, head + 18).max(1);
        let index_to_loc = bei16(&data, head + 50);
        let num_glyphs = be16(&data, maxp + 4);

        let mut loca = Vec::with_capacity(num_glyphs as usize + 1);
        for i in 0..=num_glyphs as usize {
            let v = if index_to_loc == 0 {
                be16(&data, loca_off + i * 2) as u32 * 2
            } else {
                be32(&data, loca_off + i * 4)
            };
            loca.push(v);
        }

        let (hmtx, num_h_metrics) = match (tables.get(b"hmtx"), tables.get(b"hhea")) {
            (Some(&(h, _)), Some(&(hhea, _))) => (h, be16(&data, hhea + 34)),
            _ => (0, 0),
        };

        let mut cmap = Vec::new();
        if let Some(&(cmap_off, _)) = tables.get(b"cmap") {
            let n = be16(&data, cmap_off + 2) as usize;
            for i in 0..n {
                let rec = cmap_off + 4 + i * 8;
                let platform = be16(&data, rec);
                let encoding = be16(&data, rec + 2);
                let sub = cmap_off + be32(&data, rec + 4) as usize;
                let score = match (platform, encoding) {
                    (3, 10) | (0, 4) | (0, 6) => 4, // full Unicode
                    (3, 1) | (0, 3) => 3,           // BMP Unicode
                    (0, _) => 2,
                    (3, 0) => 1, // symbol
                    _ => 0,
                };
                cmap.push(CmapSub { offset: sub, score });
            }
            cmap.sort_by(|a, b| b.score.cmp(&a.score));
        }

        let colr = tables.get(b"COLR").copied();
        let cpal = tables.get(b"CPAL").copied();
        let sbix = tables.get(b"sbix").copied();

        Some(TrueTypeFont {
            data,
            units_per_em,
            num_glyphs,
            loca,
            glyf,
            hmtx,
            num_h_metrics,
            cmap,
            colr,
            cpal,
            sbix,
        })
    }

    /// Parse the font's `sbix` bitmap-emoji table, if present (Apple colour
    /// emoji). `None` for ordinary fonts.
    pub fn sbix_glyphs(&self) -> Option<super::color::Sbix> {
        let (o, l) = self.sbix?;
        super::color::Sbix::parse(self.data.get(o..o + l)?, self.num_glyphs)
    }

    /// Parse the font's COLR/CPAL colour-glyph tables, if it has them (colour
    /// emoji). `None` for ordinary monochrome fonts.
    pub fn color_glyphs(&self) -> Option<super::color::ColorGlyphs> {
        let (co, cl) = self.colr?;
        let (po, pl) = self.cpal?;
        let colr = self.data.get(co..co + cl)?;
        let cpal = self.data.get(po..po + pl)?;
        super::color::ColorGlyphs::parse(colr, cpal)
    }

    /// Font design units per em (the outline coordinate scale).
    pub fn units_per_em(&self) -> f64 {
        self.units_per_em as f64
    }

    /// Number of glyphs in the font.
    pub fn num_glyphs(&self) -> u16 {
        self.num_glyphs
    }

    /// Glyph advance width in font units.
    pub fn advance_width(&self, gid: u16) -> f64 {
        if self.hmtx == 0 || self.num_h_metrics == 0 {
            return self.units_per_em as f64 * 0.5;
        }
        let i = (gid as usize).min(self.num_h_metrics as usize - 1);
        be16(&self.data, self.hmtx + i * 4) as f64
    }

    /// Map a Unicode scalar to a glyph id using the best available cmap.
    pub fn gid_for_unicode(&self, cp: u32) -> Option<u16> {
        for sub in &self.cmap {
            if let Some(gid) = self.cmap_lookup(sub.offset, cp) {
                if gid != 0 {
                    return Some(gid);
                }
            }
            // Symbol fonts map into the 0xF000 private-use block.
            if sub.score == 1 {
                if let Some(gid) = self.cmap_lookup(sub.offset, 0xF000 + (cp & 0xFF)) {
                    if gid != 0 {
                        return Some(gid);
                    }
                }
            }
        }
        None
    }

    fn cmap_lookup(&self, sub: usize, cp: u32) -> Option<u16> {
        let format = be16(&self.data, sub);
        match format {
            0 => {
                if cp < 256 {
                    Some(self.data.get(sub + 6 + cp as usize).copied().unwrap_or(0) as u16)
                } else {
                    None
                }
            }
            6 => {
                let first = be16(&self.data, sub + 6) as u32;
                let count = be16(&self.data, sub + 8) as u32;
                if cp >= first && cp < first + count {
                    Some(be16(&self.data, sub + 10 + (cp - first) as usize * 2))
                } else {
                    None
                }
            }
            4 => self.cmap_format4(sub, cp),
            12 => {
                let n = be32(&self.data, sub + 12) as usize;
                for i in 0..n {
                    let g = sub + 16 + i * 12;
                    let start = be32(&self.data, g);
                    let end = be32(&self.data, g + 4);
                    let start_gid = be32(&self.data, g + 8);
                    if cp >= start && cp <= end {
                        return Some((start_gid + (cp - start)) as u16);
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn cmap_format4(&self, sub: usize, cp: u32) -> Option<u16> {
        if cp > 0xFFFF {
            return None;
        }
        let cp = cp as u16;
        let seg_x2 = be16(&self.data, sub + 6) as usize;
        let segs = seg_x2 / 2;
        let end_codes = sub + 14;
        let start_codes = end_codes + seg_x2 + 2;
        let id_deltas = start_codes + seg_x2;
        let id_ranges = id_deltas + seg_x2;
        for i in 0..segs {
            let end = be16(&self.data, end_codes + i * 2);
            if cp <= end {
                let start = be16(&self.data, start_codes + i * 2);
                if cp < start {
                    return Some(0);
                }
                let delta = be16(&self.data, id_deltas + i * 2);
                let range_off = be16(&self.data, id_ranges + i * 2);
                if range_off == 0 {
                    return Some(cp.wrapping_add(delta));
                }
                let gi = id_ranges + i * 2 + range_off as usize + (cp - start) as usize * 2;
                let g = be16(&self.data, gi);
                if g == 0 {
                    return Some(0);
                }
                return Some(g.wrapping_add(delta));
            }
        }
        None
    }

    /// Flattened glyph contours in font units, ready to fill (each a closed
    /// polygon). Resolves composite glyphs recursively.
    pub fn glyph_polygons(&self, gid: u16) -> Vec<Vec<(f64, f64)>> {
        let mut contours = Vec::new();
        self.collect_glyph(gid, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0, &mut contours);
        contours
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_glyph(
        &self,
        gid: u16,
        dx: f64,
        dy: f64,
        a: f64,
        b: f64,
        c: f64,
        d: f64,
        depth: usize,
        out: &mut Vec<Vec<(f64, f64)>>,
    ) {
        if depth > 6 || gid as usize + 1 >= self.loca.len() {
            return;
        }
        let start = self.glyf + self.loca[gid as usize] as usize;
        let end = self.glyf + self.loca[gid as usize + 1] as usize;
        if end <= start || end > self.data.len() {
            return; // empty glyph (e.g. space)
        }
        let num_contours = bei16(&self.data, start);
        let xf = |x: f64, y: f64| (a * x + c * y + dx, b * x + d * y + dy);

        if num_contours < 0 {
            self.collect_composite(start + 10, dx, dy, a, b, c, d, depth, out);
            return;
        }
        let nc = num_contours as usize;
        let mut p = start + 10;
        let mut end_pts = Vec::with_capacity(nc);
        for _ in 0..nc {
            end_pts.push(be16(&self.data, p));
            p += 2;
        }
        let num_points = end_pts.last().map(|&e| e as usize + 1).unwrap_or(0);
        let instr_len = be16(&self.data, p) as usize;
        p += 2 + instr_len;

        // Flags (with repeat compression).
        let mut flags = Vec::with_capacity(num_points);
        while flags.len() < num_points {
            let f = self.data.get(p).copied().unwrap_or(0);
            p += 1;
            flags.push(f);
            if f & 0x08 != 0 {
                let repeat = self.data.get(p).copied().unwrap_or(0);
                p += 1;
                for _ in 0..repeat {
                    if flags.len() < num_points {
                        flags.push(f);
                    }
                }
            }
        }

        // X then Y coordinates (delta-encoded per the flag bits).
        let mut xs = Vec::with_capacity(num_points);
        let mut x = 0i32;
        for &f in &flags {
            if f & 0x02 != 0 {
                let dxv = self.data.get(p).copied().unwrap_or(0) as i32;
                p += 1;
                x += if f & 0x10 != 0 { dxv } else { -dxv };
            } else if f & 0x10 == 0 {
                x += bei16(&self.data, p) as i32;
                p += 2;
            }
            xs.push(x);
        }
        let mut ys = Vec::with_capacity(num_points);
        let mut y = 0i32;
        for &f in &flags {
            if f & 0x04 != 0 {
                let dyv = self.data.get(p).copied().unwrap_or(0) as i32;
                p += 1;
                y += if f & 0x20 != 0 { dyv } else { -dyv };
            } else if f & 0x20 == 0 {
                y += bei16(&self.data, p) as i32;
                p += 2;
            }
            ys.push(y);
        }

        // Walk each contour, reconstructing implied on-curve midpoints, and
        // flatten the quadratic segments.
        let mut start_idx = 0usize;
        for &endp in &end_pts {
            let endp = endp as usize;
            if endp < start_idx || endp >= num_points {
                break;
            }
            let n = endp - start_idx + 1;
            let pts: Vec<(f64, f64, bool)> = (0..n)
                .map(|i| {
                    let idx = start_idx + i;
                    let (px, py) = xf(xs[idx] as f64, ys[idx] as f64);
                    (px, py, flags[idx] & 0x01 != 0)
                })
                .collect();
            out.push(flatten_quadratic_contour(&pts));
            start_idx = endp + 1;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_composite(
        &self,
        mut p: usize,
        dx: f64,
        dy: f64,
        pa: f64,
        pb: f64,
        pc: f64,
        pd: f64,
        depth: usize,
        out: &mut Vec<Vec<(f64, f64)>>,
    ) {
        loop {
            let flags = be16(&self.data, p);
            let comp_gid = be16(&self.data, p + 2);
            p += 4;
            let (arg1, arg2);
            if flags & 0x0001 != 0 {
                arg1 = bei16(&self.data, p) as f64;
                arg2 = bei16(&self.data, p + 2) as f64;
                p += 4;
            } else {
                arg1 = (self.data.get(p).copied().unwrap_or(0) as i8) as f64;
                arg2 = (self.data.get(p + 1).copied().unwrap_or(0) as i8) as f64;
                p += 2;
            }
            let (mut a, mut b, mut c, mut d) = (1.0, 0.0, 0.0, 1.0);
            if flags & 0x0008 != 0 {
                a = f2dot14(&self.data, p);
                d = a;
                p += 2;
            } else if flags & 0x0040 != 0 {
                a = f2dot14(&self.data, p);
                d = f2dot14(&self.data, p + 2);
                p += 4;
            } else if flags & 0x0080 != 0 {
                a = f2dot14(&self.data, p);
                b = f2dot14(&self.data, p + 2);
                c = f2dot14(&self.data, p + 4);
                d = f2dot14(&self.data, p + 6);
                p += 8;
            }
            // ARGS_ARE_XY_VALUES (0x0002): arg1/arg2 are an offset.
            let (ox, oy) = if flags & 0x0002 != 0 {
                (arg1, arg2)
            } else {
                (0.0, 0.0)
            };
            // Compose parent transform with this component's.
            let ndx = pa * ox + pc * oy + dx;
            let ndy = pb * ox + pd * oy + dy;
            let na = pa * a + pc * b;
            let nb = pb * a + pd * b;
            let nc = pa * c + pc * d;
            let nd = pb * c + pd * d;
            self.collect_glyph(comp_gid, ndx, ndy, na, nb, nc, nd, depth + 1, out);

            if flags & 0x0020 == 0 {
                break; // no MORE_COMPONENTS
            }
        }
    }
}

fn f2dot14(d: &[u8], o: usize) -> f64 {
    bei16(d, o) as f64 / 16384.0
}

/// Flatten a TrueType quadratic contour (with implied on-curve midpoints) into
/// a closed polygon.
fn flatten_quadratic_contour(pts: &[(f64, f64, bool)]) -> Vec<(f64, f64)> {
    if pts.is_empty() {
        return Vec::new();
    }
    // Build a normalized point list that starts on-curve, inserting implied
    // midpoints between consecutive off-curve control points.
    let n = pts.len();
    let start = pts.iter().position(|p| p.2).unwrap_or(0);
    let mut seq: Vec<(f64, f64, bool)> = Vec::with_capacity(n + 4);
    // If no on-curve point exists, synthesize one at the midpoint of [0,1].
    if !pts[start].2 {
        let a = pts[0];
        let b = pts[1 % n];
        seq.push(((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0, true));
    }
    for i in 0..n {
        let cur = pts[(start + i) % n];
        if let Some(&prev) = seq.last() {
            if !prev.2 && !cur.2 {
                // Two consecutive off-curve points → implied on-curve midpoint.
                seq.push(((prev.0 + cur.0) / 2.0, (prev.1 + cur.1) / 2.0, true));
            }
        }
        seq.push(cur);
    }

    let mut poly = Vec::new();
    let first = seq[0];
    poly.push((first.0, first.1));
    let mut i = 0;
    while i < seq.len() {
        let next = seq[(i + 1) % seq.len()];
        if next.2 {
            // on-curve → straight line
            poly.push((next.0, next.1));
            i += 1;
        } else {
            // off-curve control → quadratic to the following on-curve point
            let ctrl = next;
            let after = seq[(i + 2) % seq.len()];
            let p0 = *poly.last().unwrap();
            flatten_quadratic(p0, (ctrl.0, ctrl.1), (after.0, after.1), &mut poly);
            i += 2;
        }
    }
    poly
}

fn flatten_quadratic(p0: (f64, f64), c: (f64, f64), p1: (f64, f64), out: &mut Vec<(f64, f64)>) {
    const STEPS: usize = 8;
    for i in 1..=STEPS {
        let t = i as f64 / STEPS as f64;
        let mt = 1.0 - t;
        let x = mt * mt * p0.0 + 2.0 * mt * t * c.0 + t * t * p1.0;
        let y = mt * mt * p0.1 + 2.0 * mt * t * c.1 + t * t * p1.1;
        out.push((x, y));
    }
}
