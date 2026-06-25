//! Windows Metafile (WMF) record interpreter — 16-bit records.
//!
//! Handles both the **placeable** header (`0x9AC6CDD7`: bounding rect + units
//! per inch, the form embedded in Office/RTF) and a bare `METAHEADER`. Each
//! record is `[recordSize:u32 (in 16-bit words)][function:u16][params…]`;
//! coordinates and most params are 16-bit. Drawing maps logical coordinates
//! through the [`Gdi`] window/viewport transform onto the device canvas.

use super::raster::*;
use super::MetafileRaster;

/// Maximum device-pixel extent (the engine caps metafile rasters at ~4096²).
const MAX_DIM: u32 = 4096;

/// Decode a WMF byte buffer into an RGBA raster, or `None` if it isn't a valid
/// WMF / can't be parsed. Never panics on malformed input.
pub fn decode(data: &[u8]) -> Option<MetafileRaster> {
    if data.len() < 18 {
        return None;
    }

    // ── Placeable header (optional) ──────────────────────────────────────────
    let mut off = 0usize;
    let mut bbox = None; // (left, top, right, bottom) in logical units
    let mut units_per_inch = 96.0f64;
    if rd_u32(data, 0) == Some(0x9AC6_CDD7) {
        // APMHEADER: key(4) hmf(2) bbox(8 × i16) inch(2) reserved(4) checksum(2)
        let left = rd_i16(data, 6)? as f64;
        let top = rd_i16(data, 8)? as f64;
        let right = rd_i16(data, 10)? as f64;
        let bottom = rd_i16(data, 12)? as f64;
        let inch = rd_u16(data, 14)? as f64;
        if inch > 0.0 {
            units_per_inch = inch;
        }
        bbox = Some((left, top, right, bottom));
        off = 22;
    }

    // ── Standard METAHEADER ──────────────────────────────────────────────────
    // type(2) headerSize(2) version(2) sizeLow(2) sizeHigh(2) numObjects(2)
    // maxRecord(4) numMembers(2)
    if off + 18 > data.len() {
        return None;
    }
    let mtype = rd_u16(data, off)?;
    if mtype != 1 && mtype != 2 {
        return None; // 1 = memory metafile, 2 = disk metafile
    }
    let header_words = rd_u16(data, off + 2)?;
    if header_words != 9 {
        return None; // METAHEADER is always 9 words
    }
    let num_objects = rd_u16(data, off + 10)? as usize;
    let records_off = off + 18;
    if records_off > data.len() {
        return None;
    }

    // ── Device sizing ────────────────────────────────────────────────────────
    // Decide the device raster from the placeable bbox (preferred) or, lacking
    // one, from the window extent discovered during a quick pre-scan.
    let frame = compute_base(data, records_off, bbox, units_per_inch)?;
    let dev_w = frame.dev_w.round().clamp(1.0, MAX_DIM as f64) as u32;
    let dev_h = frame.dev_h.round().clamp(1.0, MAX_DIM as f64) as u32;

    let mut gdi = Gdi::new(dev_w, dev_h, frame.base);
    gdi.objects = Vec::with_capacity(num_objects);
    // Seed window/viewport to the logical frame so the default window→viewport
    // map is identity and drawing coordinates flow straight through `base` to
    // device pixels. A later `SetWindowExt`/`SetViewportExt` then re-scales
    // relative to this frame, exactly as GDI does.
    gdi.win_org = Pt {
        x: frame.org_x,
        y: frame.org_y,
    };
    gdi.win_ext = Pt {
        x: frame.ext_x,
        y: frame.ext_y,
    };
    gdi.vp_org = Pt {
        x: frame.org_x,
        y: frame.org_y,
    };
    gdi.vp_ext = Pt {
        x: frame.ext_x,
        y: frame.ext_y,
    };

    play(&mut gdi, data, records_off);

    Some(MetafileRaster {
        width: gdi.canvas.width,
        height: gdi.canvas.height,
        rgba: gdi.canvas.pixels,
    })
}

/// The logical drawing frame recovered for a WMF: its origin and extent (logical
/// units) plus the `base` transform mapping that frame to the device raster.
#[derive(Debug, Clone, Copy)]
struct Frame {
    org_x: f64,
    org_y: f64,
    ext_x: f64,
    ext_y: f64,
    dev_w: f64,
    dev_h: f64,
    base: Affine,
}

/// Determine the logical→device base transform + device size. Uses the placeable
/// bbox when present; otherwise scans `SetWindowExt`/`SetWindowOrg` to recover
/// the logical frame.
fn compute_base(
    data: &[u8],
    records_off: usize,
    bbox: Option<(f64, f64, f64, f64)>,
    units_per_inch: f64,
) -> Option<Frame> {
    // Scale logical units → device pixels at ~96 dpi for screen-like output.
    let target_dpi = 96.0;
    if let Some((l, t, r, b)) = bbox {
        let (ox, oy) = (l.min(r), t.min(b));
        let lw = (r - l).abs().max(1.0);
        let lh = (b - t).abs().max(1.0);
        let scale = target_dpi / units_per_inch;
        let mut dw = lw * scale;
        let mut dh = lh * scale;
        // Cap while preserving aspect.
        let cap = MAX_DIM as f64;
        if dw > cap || dh > cap {
            let k = (cap / dw).min(cap / dh);
            dw *= k;
            dh *= k;
        }
        let sx = dw / lw;
        let sy = dh / lh;
        // Map frame-logical (origin..origin+extent) → device (0..dev).
        let base = Affine {
            m11: sx,
            m12: 0.0,
            m21: 0.0,
            m22: sy,
            dx: -ox * sx,
            dy: -oy * sy,
        };
        return Some(Frame {
            org_x: ox,
            org_y: oy,
            ext_x: lw,
            ext_y: lh,
            dev_w: dw,
            dev_h: dh,
            base,
        });
    }

    // No placeable bbox: pre-scan for window origin/extent.
    let (mut win_ext_x, mut win_ext_y) = (0.0f64, 0.0f64);
    let (mut win_org_x, mut win_org_y) = (0.0f64, 0.0f64);
    let mut p = records_off;
    while p + 6 <= data.len() {
        let size = rd_u32(data, p)? as usize;
        if size < 3 {
            break;
        }
        let func = rd_u16(data, p + 4)?;
        match func {
            0x020C => {
                // META_SETWINDOWEXT: y then x (16-bit each)
                win_ext_y = rd_i16(data, p + 6).unwrap_or(0) as f64;
                win_ext_x = rd_i16(data, p + 8).unwrap_or(0) as f64;
            }
            0x020B => {
                // META_SETWINDOWORG: y then x
                win_org_y = rd_i16(data, p + 6).unwrap_or(0) as f64;
                win_org_x = rd_i16(data, p + 8).unwrap_or(0) as f64;
            }
            0 => break,
            _ => {}
        }
        p += size * 2;
    }
    let lw = win_ext_x.abs().max(1.0);
    let lh = win_ext_y.abs().max(1.0);
    let cap = MAX_DIM as f64;
    let mut dw = lw;
    let mut dh = lh;
    if dw > cap || dh > cap {
        let k = (cap / dw).min(cap / dh);
        dw *= k;
        dh *= k;
    }
    let sx = dw / lw;
    let sy = dh / lh;
    let base = Affine {
        m11: sx,
        m12: 0.0,
        m21: 0.0,
        m22: sy,
        dx: -win_org_x * sx,
        dy: -win_org_y * sy,
    };
    Some(Frame {
        org_x: win_org_x,
        org_y: win_org_y,
        ext_x: lw,
        ext_y: lh,
        dev_w: dw,
        dev_h: dh,
        base,
    })
}

/// Track the WMF handle table: GDI handles are dense indices into the object
/// table, which is exactly how [`Gdi::add_object`] assigns slots.
fn play(gdi: &mut Gdi, data: &[u8], records_off: usize) {
    let mut p = records_off;
    // Bound the number of records to avoid pathological loops on crafted input.
    let mut guard = 0usize;
    let max_records = data.len(); // each record ≥ 3 words ⇒ far more than enough

    while p + 6 <= data.len() {
        guard += 1;
        if guard > max_records {
            break;
        }
        let size = match rd_u32(data, p) {
            Some(s) => s as usize,
            None => break,
        };
        if size < 3 {
            break; // a record is at least 3 words (size + function)
        }
        let func = match rd_u16(data, p + 4) {
            Some(f) => f,
            None => break,
        };
        let rec_bytes = size * 2;
        if p + rec_bytes > data.len() {
            // Last record truncated — stop cleanly.
            break;
        }
        // Parameter words begin after the 6-byte record header.
        let params = &data[p + 6..p + rec_bytes];
        exec_record(gdi, func, params);
        if func == 0x0000 {
            break; // META_EOF
        }
        p += rec_bytes;
    }
}

/// Execute one WMF record given its function code and parameter slice.
fn exec_record(gdi: &mut Gdi, func: u16, p: &[u8]) {
    match func {
        // ── State / transform ──────────────────────────────────────────────
        0x020B => {
            // META_SETWINDOWORG: y, x
            gdi.win_org.y = i16w(p, 0);
            gdi.win_org.x = i16w(p, 1);
        }
        0x020C => {
            // META_SETWINDOWEXT: y, x
            gdi.win_ext.y = i16w(p, 0);
            gdi.win_ext.x = i16w(p, 1);
        }
        0x020D => {
            // META_SETVIEWPORTORG: y, x
            gdi.vp_org.y = i16w(p, 0);
            gdi.vp_org.x = i16w(p, 1);
        }
        0x020E => {
            // META_SETVIEWPORTEXT: y, x
            gdi.vp_ext.y = i16w(p, 0);
            gdi.vp_ext.x = i16w(p, 1);
        }
        0x0103 => {
            // META_SETMAPMODE
            gdi.map_mode = u16w(p, 0) as i32;
        }
        0x0209 => {
            // META_SETTEXTCOLOR: COLORREF (2 words)
            gdi.text_color = Rgba::from_colorref(colorref(p, 0));
        }
        0x0201 => {
            // META_SETBKCOLOR
            gdi.bk_color = Rgba::from_colorref(colorref(p, 0));
        }
        0x0102 => {
            // META_SETBKMODE: 1 = TRANSPARENT, 2 = OPAQUE
            gdi.bk_opaque = u16w(p, 0) == 2;
        }
        0x0106 => {
            // META_SETPOLYFILLMODE: 1 = ALTERNATE, 2 = WINDING
            gdi.poly_fill_alternate = u16w(p, 0) != 2;
        }
        0x0104 => {
            // META_SETROP2: binary raster op (1 word). Applies to vector paint.
            gdi.canvas.rop2 = Rop2::from_u32(u16w(p, 0) as u32);
        }
        0x0107 => {
            // META_SETSTRETCHBLTMODE: nearest vs HALFTONE for DIB blits.
            gdi.stretch_mode = StretchMode::from_u32(u16w(p, 0) as u32);
        }

        // ── Path drawing ───────────────────────────────────────────────────
        0x0214 => {
            // META_MOVETO: y, x
            let y = i16w(p, 0);
            let x = i16w(p, 1);
            gdi.pos = gdi.to_device(x, y);
        }
        0x0213 => {
            // META_LINETO: y, x
            let y = i16w(p, 0);
            let x = i16w(p, 1);
            let to = gdi.to_device(x, y);
            let from = gdi.pos;
            gdi.stroke_open_device(&[from, to], false);
            gdi.pos = to;
        }
        0x0538 => poly_polygon(gdi, p),    // META_POLYPOLYGON
        0x0324 => polygon(gdi, p),         // META_POLYGON
        0x0325 => polyline_record(gdi, p), // META_POLYLINE

        // ── Shapes ─────────────────────────────────────────────────────────
        0x041B => {
            // META_RECTANGLE: bottom, right, top, left
            let b = i16w(p, 0);
            let r = i16w(p, 1);
            let t = i16w(p, 2);
            let l = i16w(p, 3);
            let poly = rect_poly(gdi, l, t, r, b);
            gdi.fill_and_stroke(&[poly]);
        }
        0x041C => {
            // META_ROUNDRECT: height, width, bottom, right, top, left
            let rh = i16w(p, 0);
            let rw = i16w(p, 1);
            let b = i16w(p, 2);
            let r = i16w(p, 3);
            let t = i16w(p, 4);
            let l = i16w(p, 5);
            let poly = round_rect_poly(gdi, l, t, r, b, rw, rh);
            gdi.fill_and_stroke(&[poly]);
        }
        0x0418 => {
            // META_ELLIPSE: bottom, right, top, left
            let b = i16w(p, 0);
            let r = i16w(p, 1);
            let t = i16w(p, 2);
            let l = i16w(p, 3);
            let poly = ellipse_poly(gdi, l, t, r, b, ellipse_segments(gdi, l, t, r, b));
            gdi.fill_and_stroke(&[poly]);
        }
        0x0817 => arc_pie_chord(gdi, p, ArcKind::Arc),
        0x081A => arc_pie_chord(gdi, p, ArcKind::Pie),
        0x0830 => arc_pie_chord(gdi, p, ArcKind::Chord),
        0x041F => {
            // META_SETPIXEL: COLORREF(2), y, x
            let color = Rgba::from_colorref(colorref(p, 0));
            let y = i16w(p, 2);
            let x = i16w(p, 3);
            let d = gdi.to_device(x, y);
            gdi.canvas
                .blend(d.x.round() as i32, d.y.round() as i32, color, 1.0);
        }

        // ── Region paint (region reduced to bbox) ──────────────────────────
        0x0228 => fill_region(gdi, p, true), // META_FILLREGION (region, brush)
        0x012B => paint_region(gdi, p),      // META_PAINTREGION

        // ── Text ────────────────────────────────────────────────────────────
        0x0521 => text_out(gdi, p),     // META_TEXTOUT
        0x0A32 => ext_text_out(gdi, p), // META_EXTTEXTOUT

        // ── Object table ────────────────────────────────────────────────────
        0x02FA => create_pen_indirect(gdi, p),
        0x02FC => create_brush_indirect(gdi, p),
        0x02FB => create_font_indirect(gdi, p),
        0x06FF => {
            // META_CREATEREGION: a scan-based region. Parse its rect list so the
            // region's true union (not just its AABB) can be filled/clipped.
            let rects = parse_wmf_region(p);
            let _ = gdi.add_object(GdiObject::Region(rects));
        }
        0x02FF => {
            // 0x02FF is otherwise unmodelled here; keep an empty region slot so
            // handle indices stay aligned (legacy behaviour).
            let _ = gdi.add_object(GdiObject::Region(Vec::new()));
        }
        0x01F0 => {
            // META_DELETEOBJECT: index
            let idx = u16w(p, 0) as usize;
            gdi.delete_object(idx);
        }
        0x012D => {
            // META_SELECTOBJECT: index
            let idx = u16w(p, 0) as usize;
            gdi.select_object(idx);
        }
        0x012C => {
            // META_SELECTCLIPREGION: clip to the selected region's union bbox.
            let idx = u16w(p, 0) as usize;
            let bbox = match gdi.objects.get(idx) {
                Some(GdiObject::Region(rects)) => region_bbox(rects),
                _ => None,
            };
            gdi.set_clip_logrect(bbox);
        }
        0x02FD => create_solid_brush_pattern(gdi), // META_CREATEPATTERNBRUSH
        0x0142 => create_dib_pattern_brush(gdi, p), // META_DIBCREATEPATTERNBRUSH

        // ── DIB blits ──────────────────────────────────────────────────────
        0x0F43 => stretch_dibits(gdi, p),  // META_STRETCHDIB
        0x0940 => dib_stretch_blt(gdi, p), // META_DIBSTRETCHBLT
        0x0B41 => dib_bit_blt(gdi, p),     // META_DIBBITBLT
        0x0922 => set_dib_to_dev(gdi, p),  // META_SETDIBTODEV (SetDIBitsToDevice)

        // Everything else: skip-safely (clipping ops, escapes, palette, etc.).
        _ => {}
    }
}

// ── helpers reading 16-bit params ──────────────────────────────────────────

fn i16w(p: &[u8], word: usize) -> f64 {
    rd_i16(p, word * 2).unwrap_or(0) as f64
}

fn u16w(p: &[u8], word: usize) -> u16 {
    rd_u16(p, word * 2).unwrap_or(0)
}

/// A COLORREF stored at `word` (two 16-bit words, low word first).
fn colorref(p: &[u8], word: usize) -> u32 {
    let lo = u16w(p, word) as u32;
    let hi = u16w(p, word + 1) as u32;
    lo | (hi << 16)
}

/// Pick an ellipse tessellation count from its on-device size.
fn ellipse_segments(gdi: &Gdi, l: f64, t: f64, r: f64, b: f64) -> usize {
    let a = gdi.to_device(l, t);
    let c = gdi.to_device(r, b);
    let d = (c.x - a.x).hypot(c.y - a.y);
    ((d as usize) / 3).clamp(24, 180)
}

// ── object creation ─────────────────────────────────────────────────────────

fn create_pen_indirect(gdi: &mut Gdi, p: &[u8]) {
    // LOGPEN: style(2), width.x(2), width.y(2), COLORREF(4)
    let style = PenStyle::from_u32(u16w(p, 0) as u32);
    let width = i16w(p, 1).abs();
    let color = Rgba::from_colorref(colorref(p, 3));
    gdi.add_object(GdiObject::Pen(Pen {
        style,
        width,
        color,
    }));
}

fn create_brush_indirect(gdi: &mut Gdi, p: &[u8]) {
    // LOGBRUSH: style(2), COLORREF(4), hatch(2)
    let style_raw = u16w(p, 0);
    let color = Rgba::from_colorref(colorref(p, 1));
    let hatch_raw = u16w(p, 3) as u32; // lbHatch (the HS_* selector for BS_HATCHED)
    let brush = match style_raw {
        0 => Brush::solid(color),                                  // BS_SOLID
        1 => Brush::null(),                                        // BS_NULL/HOLLOW
        2 => Brush::hatched(color, HatchStyle::from_u32(hatch_raw)), // BS_HATCHED
        _ => Brush::solid(color),
    };
    gdi.add_object(GdiObject::Brush(brush));
}

/// META_DIBCREATEPATTERNBRUSH (`0x0142`) — decode the packed DIB tile so the
/// pattern brush tiles real pixels; an undecodable tile falls back to mid-grey.
/// Layout: usage(2), then the packed DIB (BITMAPINFOHEADER + palette + bits).
fn create_dib_pattern_brush(gdi: &mut Gdi, p: &[u8]) {
    // The first param word is the colour-table usage; the DIB starts after it.
    let tile = p.get(2..).and_then(decode_packed_dib);
    gdi.add_object(GdiObject::Brush(Brush::pattern(tile)));
}

/// META_CREATEPATTERNBRUSH (`0x01F9`) — a monochrome pattern bitmap brush; we
/// can't cheaply recover its bits here, so approximate with the mid-grey solid
/// fallback (a `Pattern` brush carrying no tile).
fn create_solid_brush_pattern(gdi: &mut Gdi) {
    gdi.add_object(GdiObject::Brush(Brush::pattern(None)));
}

fn create_font_indirect(gdi: &mut Gdi, p: &[u8]) {
    // LOGFONT: height(2,i16) width(2,i16) escapement(2) orientation(2) weight(2)
    // italic(1) underline(1) strikeout(1) charset(1) ... facename(32)
    let height = (i16w(p, 0)).abs();
    let width = (i16w(p, 1)).abs();
    let escapement = rd_i16(p, 4).unwrap_or(0) as i32;
    let weight = u16w(p, 4);
    let bold = weight >= 600;
    let italic = p.get(18).copied().unwrap_or(0) != 0;
    gdi.add_object(GdiObject::Font(Font {
        height: if height > 0.0 { height } else { 12.0 },
        width,
        escapement,
        bold,
        italic,
    }));
}

// ── polygons / polylines ────────────────────────────────────────────────────

fn polygon(gdi: &mut Gdi, p: &[u8]) {
    // META_POLYGON: count(2), then count × (x:i16, y:i16)
    let count = u16w(p, 0) as usize;
    let mut poly = Vec::with_capacity(count);
    for i in 0..count {
        let x = i16w(p, 1 + i * 2);
        let y = i16w(p, 2 + i * 2);
        poly.push(gdi.to_device(x, y));
    }
    if poly.len() >= 2 {
        gdi.fill_and_stroke(&[poly]);
    }
}

fn polyline_record(gdi: &mut Gdi, p: &[u8]) {
    // META_POLYLINE: count(2), then count × (x:i16, y:i16)
    let count = u16w(p, 0) as usize;
    let mut pts = Vec::with_capacity(count);
    for i in 0..count {
        let x = i16w(p, 1 + i * 2);
        let y = i16w(p, 2 + i * 2);
        pts.push(gdi.to_device(x, y));
    }
    if pts.len() >= 2 {
        gdi.stroke_open_device(&pts, false);
    }
}

fn poly_polygon(gdi: &mut Gdi, p: &[u8]) {
    // META_POLYPOLYGON: numPolys(2), counts[numPolys](2 each), then all points
    let num = u16w(p, 0) as usize;
    if num == 0 {
        return;
    }
    let mut counts = Vec::with_capacity(num);
    for i in 0..num {
        counts.push(u16w(p, 1 + i) as usize);
    }
    let mut word = 1 + num;
    let mut polys = Vec::with_capacity(num);
    for &c in &counts {
        let mut poly = Vec::with_capacity(c);
        for _ in 0..c {
            let x = i16w(p, word);
            let y = i16w(p, word + 1);
            poly.push(gdi.to_device(x, y));
            word += 2;
        }
        if poly.len() >= 2 {
            polys.push(poly);
        }
    }
    if !polys.is_empty() {
        gdi.fill_and_stroke(&polys);
    }
}

enum ArcKind {
    Arc,
    Pie,
    Chord,
}

fn arc_pie_chord(gdi: &mut Gdi, p: &[u8], kind: ArcKind) {
    // params: yEnd, xEnd, yStart, xStart, bottom, right, top, left  (8 × i16)
    let y_end = i16w(p, 0);
    let x_end = i16w(p, 1);
    let y_start = i16w(p, 2);
    let x_start = i16w(p, 3);
    let b = i16w(p, 4);
    let r = i16w(p, 5);
    let t = i16w(p, 6);
    let l = i16w(p, 7);
    let cx = (l + r) / 2.0;
    let cy = (t + b) / 2.0;
    let rx = (r - l).abs() / 2.0;
    let ry = (b - t).abs() / 2.0;
    let seg = (((rx + ry) as usize) / 3).clamp(16, 180);
    let pts = arc_points(gdi, l, t, r, b, x_start, y_start, x_end, y_end, seg);
    match kind {
        ArcKind::Arc => {
            gdi.stroke_open_device(&pts, false);
        }
        ArcKind::Pie => {
            let mut poly = pts;
            poly.push(gdi.to_device(cx, cy));
            gdi.fill_and_stroke(&[poly]);
        }
        ArcKind::Chord => {
            gdi.fill_and_stroke(&[pts]);
        }
    }
    let _ = (cx, cy, rx, ry);
}

// ── regions ─────────────────────────────────────────────────────────────────

fn fill_region(gdi: &mut Gdi, p: &[u8], _with_brush: bool) {
    // META_FILLREGION: regionIndex(2), brushIndex(2). Fill the region's full
    // rectangle union with the named brush's colour.
    let region_idx = u16w(p, 0) as usize;
    let brush_idx = u16w(p, 1) as usize;
    let rects = region_rects(gdi, region_idx);
    let color = match gdi.objects.get(brush_idx) {
        Some(GdiObject::Brush(b)) if b.style != BrushStyle::Null => b.color,
        _ => return,
    };
    let polys: Vec<Vec<Pt>> = rects
        .iter()
        .map(|rc| rect_poly(gdi, rc.left, rc.top, rc.right, rc.bottom))
        .collect();
    if !polys.is_empty() {
        fill_polygons(
            &mut gdi.canvas,
            &polys,
            color,
            gdi.poly_fill_alternate,
            gdi.clip.as_ref(),
        );
    }
}

fn paint_region(gdi: &mut Gdi, p: &[u8]) {
    // META_PAINTREGION: regionIndex(2) — paint the region's full union with the
    // current brush.
    let region_idx = u16w(p, 0) as usize;
    if gdi.cur_brush.style == BrushStyle::Null {
        return;
    }
    let color = gdi.cur_brush.color;
    let alt = gdi.poly_fill_alternate;
    let rects = region_rects(gdi, region_idx);
    let polys: Vec<Vec<Pt>> = rects
        .iter()
        .map(|rc| rect_poly(gdi, rc.left, rc.top, rc.right, rc.bottom))
        .collect();
    if !polys.is_empty() {
        fill_polygons(&mut gdi.canvas, &polys, color, alt, gdi.clip.as_ref());
    }
}

/// The full rectangle list of region object `idx` (empty if it isn't a region).
fn region_rects(gdi: &Gdi, idx: usize) -> Vec<LogRect> {
    match gdi.objects.get(idx) {
        Some(GdiObject::Region(rects)) => rects.clone(),
        _ => Vec::new(),
    }
}

/// Parse a WMF `META_CREATEREGION` payload into its list of scan rectangles.
///
/// The record body holds a `Region` object: a small header (`nextInChain`(2),
/// `objType`(2), `objCount`(4), `regionSize`(2), `scanCount`(2),
/// `maxScan`(2)), the bounding rect (`left,top,right,bottom`, 4×i16), then
/// `scanCount` `Scan` structures. Each scan is `count`(2), `top`(2),
/// `bottom`(2), then `count` `(left,right)` x-extent pairs (2×i16 each) and a
/// trailing `count`(2). We turn every x-extent into one `LogRect` so the
/// region's true area (the union of its scan rectangles) is recoverable. Bounds
/// are checked throughout; a malformed body yields whatever was parsed so far.
fn parse_wmf_region(p: &[u8]) -> Vec<LogRect> {
    // Region header is 16 bytes (8 × u16); the bounding rect follows (8 bytes).
    if p.len() < 24 {
        return Vec::new();
    }
    let scan_count = u16w(p, 5) as usize; // word index 5 = rgnScanCount
    let mut rects = Vec::new();
    // Scans begin after the 16-byte header + 8-byte bounding rect (12 words in).
    let mut word = 12usize;
    for _ in 0..scan_count {
        // Scan: count(1w), top(1w), bottom(1w), then count×(left,right), count(1w).
        if (word + 3) * 2 > p.len() {
            break;
        }
        let count = u16w(p, word) as usize;
        let top = i16w(p, word + 1);
        let bottom = i16w(p, word + 2);
        word += 3;
        // Sanity-cap the pair count against the remaining payload.
        let avail_pairs = p.len().saturating_sub(word * 2) / 4;
        if count > avail_pairs {
            break;
        }
        for _ in 0..count {
            let left = i16w(p, word);
            let right = i16w(p, word + 1);
            word += 2;
            rects.push(LogRect {
                left,
                top,
                right,
                bottom,
            });
        }
        word += 1; // trailing duplicate count word
    }
    rects
}

// ── text (fallback box / advance) ──────────────────────────────────────────

fn text_out(gdi: &mut Gdi, p: &[u8]) {
    // META_TEXTOUT: count(2), string[count bytes (padded to word)], y(2), x(2)
    let count = u16w(p, 0) as usize;
    let str_words = count.div_ceil(2);
    let y = i16w(p, 1 + str_words);
    let x = i16w(p, 2 + str_words);
    let bytes = byte_slice(p, 2, count);
    draw_text_box(gdi, x, y, &bytes);
}

fn ext_text_out(gdi: &mut Gdi, p: &[u8]) {
    // META_EXTTEXTOUT: y(2), x(2), count(2), options(2), [rect 4×i16 if opts],
    // string, [dx array]
    let y = i16w(p, 0);
    let x = i16w(p, 1);
    let count = u16w(p, 2) as usize;
    let options = u16w(p, 3);
    // ETO_CLIPPED(4) or ETO_OPAQUE(2) ⇒ a bounding rect of 4 words precedes text.
    let has_rect = options & 0x0006 != 0;
    let mut word = 4;
    if has_rect {
        word += 4;
    }
    let bytes = byte_slice(p, word, count);
    draw_text_box(gdi, x, y, &bytes);
}

/// Render text as a reasonable placeholder strip at the logical origin — text is
/// secondary; we draw a light glyph-box per visible char so positioned text
/// reads as "ink here" (honouring font metrics/escapement/bold/italic) without a
/// full font pipeline.
fn draw_text_box(gdi: &mut Gdi, x: f64, y: f64, bytes: &[u8]) {
    if bytes.iter().all(|b| *b == 0 || *b == b' ') {
        return;
    }
    let runs: Vec<bool> = bytes.iter().map(|b| *b != 0 && *b != b' ').collect();
    gdi.draw_text(x, y, &runs);
}

/// Slice `count` bytes of string payload starting at parameter word `word`.
fn byte_slice(p: &[u8], word: usize, count: usize) -> Vec<u8> {
    let start = word * 2;
    p.get(start..start + count)
        .map(|s| s.to_vec())
        .unwrap_or_default()
}

// ── DIB blits ───────────────────────────────────────────────────────────────

/// META_STRETCHDIB: rop(4), usage(2), srcW(2), srcH(2), srcY(2), srcX(2),
/// destH(2), destW(2), destY(2), destX(2), then the packed DIB.
fn stretch_dibits(gdi: &mut Gdi, p: &[u8]) {
    // Word layout: 0-1 rop, 2 usage, 3 srcH, 4 srcW, 5 srcY, 6 srcX,
    // 7 destH, 8 destW, 9 destY, 10 destX, 11.. DIB
    let dest_h = i16w(p, 7);
    let dest_w = i16w(p, 8);
    let dest_y = i16w(p, 9);
    let dest_x = i16w(p, 10);
    let dib_off = 11 * 2;
    blit_packed(gdi, p, dib_off, dest_x, dest_y, dest_w, dest_h);
}

/// META_DIBSTRETCHBLT: rop(4), srcH(2), srcW(2), srcY(2), srcX(2), destH(2),
/// destW(2), destY(2), destX(2), DIB. (No usage word, unlike STRETCHDIB.)
fn dib_stretch_blt(gdi: &mut Gdi, p: &[u8]) {
    let dest_h = i16w(p, 6);
    let dest_w = i16w(p, 7);
    let dest_y = i16w(p, 8);
    let dest_x = i16w(p, 9);
    let dib_off = 10 * 2;
    blit_packed(gdi, p, dib_off, dest_x, dest_y, dest_w, dest_h);
}

/// META_DIBBITBLT: rop(4), srcY(2), srcX(2), height(2), width(2), destY(2),
/// destX(2), DIB. 1:1 blit (dest size = src height/width).
fn dib_bit_blt(gdi: &mut Gdi, p: &[u8]) {
    let height = i16w(p, 4);
    let width = i16w(p, 5);
    let dest_y = i16w(p, 6);
    let dest_x = i16w(p, 7);
    let dib_off = 8 * 2;
    blit_packed(gdi, p, dib_off, dest_x, dest_y, width, height);
}

/// META_SETDIBTODEV: usage(2), scanCount(2), startScan(2), srcY(2), srcX(2),
/// height(2), width(2), destY(2), destX(2), DIB.
fn set_dib_to_dev(gdi: &mut Gdi, p: &[u8]) {
    let height = i16w(p, 5);
    let width = i16w(p, 6);
    let dest_y = i16w(p, 7);
    let dest_x = i16w(p, 8);
    let dib_off = 9 * 2;
    blit_packed(gdi, p, dib_off, dest_x, dest_y, width, height);
}

/// Decode the packed DIB at byte offset `dib_off` in `p` and blit it to the
/// logical destination rect, mapped through the active transform.
fn blit_packed(gdi: &mut Gdi, p: &[u8], dib_off: usize, dx: f64, dy: f64, dw: f64, dh: f64) {
    let Some(slice) = p.get(dib_off..) else {
        return;
    };
    let Some(dib) = decode_packed_dib(slice) else {
        return;
    };
    // Map the logical dest rect corners to device pixels.
    let top_left = gdi.to_device(dx, dy);
    let bottom_right = gdi.to_device(dx + dw, dy + dh);
    let ddw = bottom_right.x - top_left.x;
    let ddh = bottom_right.y - top_left.y;
    let mode = gdi.stretch_mode;
    blit_dib(
        &mut gdi.canvas,
        &dib,
        top_left.x,
        top_left.y,
        ddw,
        ddh,
        gdi.clip.as_ref(),
        mode,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── tiny placeable-WMF builder ───────────────────────────────────────────

    fn pu16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn pi16(v: &mut Vec<u8>, x: i16) {
        v.extend_from_slice(&(x as u16).to_le_bytes());
    }
    fn pu32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    fn record(out: &mut Vec<u8>, func: u16, params: &[u16]) {
        let size = 3 + params.len() as u32;
        pu32(out, size);
        pu16(out, func);
        for p in params {
            pu16(out, *p);
        }
    }

    fn placeable(bbox: (i16, i16, i16, i16), inch: u16, records: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        pu32(&mut v, 0x9AC6_CDD7);
        pu16(&mut v, 0);
        pi16(&mut v, bbox.0);
        pi16(&mut v, bbox.1);
        pi16(&mut v, bbox.2);
        pi16(&mut v, bbox.3);
        pu16(&mut v, inch);
        pu32(&mut v, 0);
        pu16(&mut v, 0);
        pu16(&mut v, 1); // mtType
        pu16(&mut v, 9); // mtHeaderSize
        pu16(&mut v, 0x0300);
        pu32(&mut v, 0);
        pu16(&mut v, 4);
        pu32(&mut v, 0);
        pu16(&mut v, 0);
        v.extend_from_slice(records);
        v
    }

    fn colorref(r: u8, g: u8, b: u8) -> [u16; 2] {
        let c = (r as u32) | ((g as u32) << 8) | ((b as u32) << 16);
        [(c & 0xFFFF) as u16, (c >> 16) as u16]
    }

    fn px(m: &MetafileRaster, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * m.width + x) * 4) as usize;
        (m.rgba[i], m.rgba[i + 1], m.rgba[i + 2], m.rgba[i + 3])
    }

    /// Window 0..100 with org 0; device ≈ 96px at 100 units/inch.
    fn window_setup(recs: &mut Vec<u8>) {
        record(recs, 0x020B, &[0, 0]); // SetWindowOrg y,x
        record(recs, 0x020C, &[100, 100]); // SetWindowExt y,x
    }

    // ── #177 hatch brush rendering ───────────────────────────────────────────

    #[test]
    fn hatched_brush_rectangle_is_striped_not_solid() {
        let mut recs = Vec::new();
        window_setup(&mut recs);
        // Null pen (so only the brush shows), hatched black diagonal-cross brush.
        record(&mut recs, 0x02FA, &[5, 0, 0, 0, 0]); // CreatePen NULL
        let black = colorref(0, 0, 0);
        // CreateBrushIndirect: style=2 (BS_HATCHED), COLORREF black, hatch=5 (DIAGCROSS)
        record(&mut recs, 0x02FC, &[2, black[0], black[1], 5]);
        record(&mut recs, 0x012D, &[0]); // select pen
        record(&mut recs, 0x012D, &[1]); // select brush
                                         // Rectangle covering most of the canvas.
        record(&mut recs, 0x041B, &[95, 95, 5, 5]); // b,r,t,l
        record(&mut recs, 0x0000, &[]); // EOF
        let m = decode(&placeable((0, 0, 100, 100), 100, &recs)).expect("decode");

        // Count painted vs total inside the rectangle interior — a hatch leaves
        // most interior pixels transparent (unlike a solid fill).
        let (mut painted, mut total) = (0u32, 0u32);
        let x0 = m.width / 10;
        let x1 = m.width * 9 / 10;
        let y0 = m.height / 10;
        let y1 = m.height * 9 / 10;
        for y in y0..y1 {
            for x in x0..x1 {
                total += 1;
                if px(&m, x, y).3 > 0 {
                    painted += 1;
                }
            }
        }
        assert!(painted > 0, "hatch must paint some ink");
        assert!(
            painted * 2 < total,
            "hatch should leave most interior empty ({painted}/{total})"
        );
    }

    // ── #176/#180 SETROP2 ────────────────────────────────────────────────────

    #[test]
    fn setrop2_black_forces_black_fill() {
        let mut recs = Vec::new();
        window_setup(&mut recs);
        // R2_BLACK (1) — any subsequent fill paints black.
        record(&mut recs, 0x0104, &[1]); // SETROP2 R2_BLACK
        record(&mut recs, 0x02FA, &[5, 0, 0, 0, 0]); // NULL pen
        let red = colorref(255, 0, 0);
        record(&mut recs, 0x02FC, &[0, red[0], red[1], 0]); // solid RED brush
        record(&mut recs, 0x012D, &[0]);
        record(&mut recs, 0x012D, &[1]);
        record(&mut recs, 0x041B, &[90, 90, 10, 10]);
        record(&mut recs, 0x0000, &[]);
        let m = decode(&placeable((0, 0, 100, 100), 100, &recs)).expect("decode");
        let c = px(&m, m.width / 2, m.height / 2);
        // Despite a red brush, R2_BLACK forces black ink (not red).
        assert!(c.3 > 0, "centre painted");
        assert!(
            c.0 < 40 && c.1 < 40 && c.2 < 40,
            "R2_BLACK should paint black, got {c:?}"
        );
    }

    // ── #175 SELECTCLIPREGION ────────────────────────────────────────────────

    #[test]
    fn select_clip_region_bounds_subsequent_fill() {
        let mut recs = Vec::new();
        window_setup(&mut recs);
        // CreateRegion (handle 0) covering logical (0,0)-(50,50): one scan rect.
        // Region body: header(8 words) + bbox(4 words) + scan.
        let mut rgn = Vec::<u16>::new();
        // header: next(0), objType(6=region), objCount(2 words lo/hi)=0,
        // regionSize(0), scanCount(1), maxScan(1)
        rgn.extend_from_slice(&[0, 0x0006, 0, 0, 0, 1, 1, 0]); // 8 words
        rgn.extend_from_slice(&[0, 0, 50, 50]); // bbox l,t,r,b
                                                // scan: count(1), top(0), bottom(50), (left,right)=(0,50), count(1)
        rgn.extend_from_slice(&[1, 0, 50, 0, 50, 1]);
        record(&mut recs, 0x06FF, &rgn); // META_CREATEREGION
        record(&mut recs, 0x012C, &[0]); // SELECTCLIPREGION handle 0

        // Null pen + solid blue brush, then a full-canvas rectangle.
        record(&mut recs, 0x02FA, &[5, 0, 0, 0, 0]);
        let blue = colorref(0, 0, 255);
        record(&mut recs, 0x02FC, &[0, blue[0], blue[1], 0]);
        record(&mut recs, 0x012D, &[1]); // pen (handle 1)
        record(&mut recs, 0x012D, &[2]); // brush (handle 2)
        record(&mut recs, 0x041B, &[100, 100, 0, 0]); // full rect
        record(&mut recs, 0x0000, &[]);
        let m = decode(&placeable((0, 0, 100, 100), 100, &recs)).expect("decode");

        // Inside the clip (logical ~25,25): painted. Outside (logical ~75,75):
        // clipped away.
        let inside = px(&m, m.width / 4, m.height / 4);
        let outside = px(&m, m.width * 3 / 4, m.height * 3 / 4);
        assert!(inside.3 > 0, "inside clip region painted, got {inside:?}");
        assert_eq!(outside.3, 0, "outside clip region empty, got {outside:?}");
    }

    // ── #182 region union fill ───────────────────────────────────────────────

    #[test]
    fn fill_region_paints_union_of_scan_rects() {
        // A region with two disjoint scan rectangles: a left band and a right
        // band, leaving a transparent gap between them.
        let mut recs = Vec::new();
        window_setup(&mut recs);
        let mut rgn = Vec::<u16>::new();
        rgn.extend_from_slice(&[0, 0x0006, 0, 0, 0, 1, 2, 0]); // scanCount=1, maxScan=2
        rgn.extend_from_slice(&[0, 0, 100, 100]); // bbox
                                                  // One scan spanning y 0..100 with TWO x-extents: (0,30) and (70,100).
        rgn.extend_from_slice(&[2, 0, 100, 0, 30, 70, 100, 2]);
        record(&mut recs, 0x06FF, &rgn); // CREATEREGION → handle 0
                                         // Solid black brush (handle 1).
        let black = colorref(0, 0, 0);
        record(&mut recs, 0x02FC, &[0, black[0], black[1], 0]);
        // FillRegion(region=0, brush=1).
        record(&mut recs, 0x0228, &[0, 1]);
        record(&mut recs, 0x0000, &[]);
        let m = decode(&placeable((0, 0, 100, 100), 100, &recs)).expect("decode");

        let left = px(&m, m.width / 10, m.height / 2);
        let gap = px(&m, m.width / 2, m.height / 2);
        let right = px(&m, m.width * 9 / 10, m.height / 2);
        assert!(left.3 > 0, "left band painted, got {left:?}");
        assert!(right.3 > 0, "right band painted, got {right:?}");
        assert_eq!(gap.3, 0, "gap between bands empty (union, not bbox)");
    }

    // ── region parser unit ───────────────────────────────────────────────────

    #[test]
    fn parse_wmf_region_extracts_scan_rects() {
        // header(8w) + bbox(4w) + scan{count=2, top=3, bottom=9, (1,4),(6,8), 2}
        let mut p = Vec::<u8>::new();
        for w in [0u16, 0x0006, 0, 0, 0, 1, 2, 0] {
            pu16(&mut p, w);
        }
        for w in [0u16, 3, 8, 9] {
            pu16(&mut p, w);
        }
        for w in [2u16, 3, 9, 1, 4, 6, 8, 2] {
            pu16(&mut p, w);
        }
        let rects = parse_wmf_region(&p);
        assert_eq!(rects.len(), 2);
        assert_eq!(
            (rects[0].left, rects[0].top, rects[0].right, rects[0].bottom),
            (1.0, 3.0, 4.0, 9.0)
        );
        assert_eq!(
            (rects[1].left, rects[1].top, rects[1].right, rects[1].bottom),
            (6.0, 3.0, 8.0, 9.0)
        );
        // Truncated payload never panics.
        assert!(parse_wmf_region(&[0u8; 4]).is_empty());
    }
}
