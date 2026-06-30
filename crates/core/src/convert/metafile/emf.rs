//! Enhanced Metafile (EMF) record interpreter — 32-bit records.
//!
//! Parses the `ENHMETAHEADER` (`iType == 1`: `rclBounds` device rect +
//! `rclFrame` .01-mm frame) and the 32-bit record stream
//! `[iType:u32][nSize:u32][data…]`. Coordinates are 32-bit signed. Honors the
//! window/viewport mapping plus the EMF world transform
//! (`SetWorldTransform`/`ModifyWorldTransform`).

use super::raster::*;
use super::MetafileRaster;

const MAX_DIM: u32 = 4096;

// EMF record type codes (a subset — the rest are skipped-safely).
const EMR_HEADER: u32 = 1;
const EMR_POLYBEZIER: u32 = 2;
const EMR_POLYGON: u32 = 3;
const EMR_POLYLINE: u32 = 4;
const EMR_POLYBEZIERTO: u32 = 5;
const EMR_POLYLINETO: u32 = 6;
const EMR_POLYPOLYLINE: u32 = 7;
const EMR_POLYPOLYGON: u32 = 8;
const EMR_SETWINDOWEXTEX: u32 = 9;
const EMR_SETWINDOWORGEX: u32 = 10;
const EMR_SETVIEWPORTEXTEX: u32 = 11;
const EMR_SETVIEWPORTORGEX: u32 = 12;
const EMR_EOF: u32 = 14;
const EMR_SETPIXELV: u32 = 15;
const EMR_SETMAPMODE: u32 = 17;
const EMR_SETBKMODE: u32 = 18;
const EMR_SETPOLYFILLMODE: u32 = 19;
const EMR_SETROP2: u32 = 20;
const EMR_SETSTRETCHBLTMODE: u32 = 21;
const EMR_SETTEXTCOLOR: u32 = 24;
const EMR_SETBKCOLOR: u32 = 25;
const EMR_SETWORLDTRANSFORM: u32 = 35;
const EMR_MODIFYWORLDTRANSFORM: u32 = 36;
const EMR_SELECTOBJECT: u32 = 37;
const EMR_CREATEPEN: u32 = 38;
const EMR_CREATEBRUSHINDIRECT: u32 = 39;
const EMR_DELETEOBJECT: u32 = 40;
const EMR_ELLIPSE: u32 = 42;
const EMR_RECTANGLE: u32 = 43;
const EMR_ROUNDRECT: u32 = 44;
const EMR_ARC: u32 = 45;
const EMR_CHORD: u32 = 46;
const EMR_PIE: u32 = 47;
const EMR_LINETO: u32 = 54;
const EMR_ARCTO: u32 = 55;
const EMR_MOVETOEX: u32 = 27;
const EMR_FILLRGN: u32 = 71;
const EMR_PAINTRGN: u32 = 74;
const EMR_EXTSELECTCLIPRGN: u32 = 75;
const EMR_BITBLT: u32 = 76;
const EMR_STRETCHBLT: u32 = 77;
const EMR_STRETCHDIBITS: u32 = 81;
const EMR_EXTCREATEFONTINDIRECTW: u32 = 82;
const EMR_EXTTEXTOUTA: u32 = 83;
const EMR_EXTTEXTOUTW: u32 = 84;
const EMR_POLYBEZIER16: u32 = 85;
const EMR_POLYGON16: u32 = 86;
const EMR_POLYLINE16: u32 = 87;
const EMR_POLYBEZIERTO16: u32 = 88;
const EMR_POLYLINETO16: u32 = 89;
const EMR_POLYPOLYLINE16: u32 = 90;
const EMR_POLYPOLYGON16: u32 = 91;
const EMR_EXTCREATEPEN: u32 = 95;
const EMR_SETDIBITSTODEVICE: u32 = 80;

/// Decode an EMF byte buffer into an RGBA raster, or `None` if it isn't a valid
/// EMF / can't be parsed. Never panics on malformed input.
pub fn decode(data: &[u8]) -> Option<MetafileRaster> {
    if data.len() < 88 {
        return None;
    }
    // Header record: iType(4)=1, nSize(4), rclBounds(16), rclFrame(16),
    // signature(4)=" EMF" (0x464D4520).
    if rd_u32(data, 0)? != EMR_HEADER {
        return None;
    }
    let header_size = rd_u32(data, 4)? as usize;
    if header_size < 88 || header_size > data.len() {
        return None;
    }
    if rd_u32(data, 40)? != 0x464D_4520 {
        return None; // " EMF" signature
    }

    // rclBounds: device units; rclFrame: .01 mm.
    let b_left = rd_i32(data, 8)?;
    let b_top = rd_i32(data, 12)?;
    let b_right = rd_i32(data, 16)?;
    let b_bottom = rd_i32(data, 20)?;

    // Device-pixel size from the inclusive bounds rectangle.
    let mut dev_w = (b_right - b_left).unsigned_abs() + 1;
    let mut dev_h = (b_bottom - b_top).unsigned_abs() + 1;
    if dev_w == 0 || dev_h == 0 {
        return None;
    }
    // Cap, preserving aspect.
    if dev_w > MAX_DIM || dev_h > MAX_DIM {
        let k = (MAX_DIM as f64 / dev_w as f64).min(MAX_DIM as f64 / dev_h as f64);
        dev_w = ((dev_w as f64) * k).round().max(1.0) as u32;
        dev_h = ((dev_h as f64) * k).round().max(1.0) as u32;
    }

    // Base transform: device-bounds origin → (0,0), scaled to the (capped)
    // raster. EMF logical coords are already device-space pre-world-transform,
    // so window/viewport default to identity (1:1) unless records change them.
    let sx = dev_w as f64 / ((b_right - b_left).unsigned_abs().max(1) as f64);
    let sy = dev_h as f64 / ((b_bottom - b_top).unsigned_abs().max(1) as f64);
    let base = Affine {
        m11: sx,
        m12: 0.0,
        m21: 0.0,
        m22: sy,
        dx: -(b_left as f64) * sx,
        dy: -(b_top as f64) * sy,
    };

    let mut gdi = Gdi::new(dev_w, dev_h, base);
    // EMF: window/viewport identity by default (coords already device-like).
    gdi.win_ext = Pt { x: 1.0, y: 1.0 };
    gdi.vp_ext = Pt { x: 1.0, y: 1.0 };

    play(&mut gdi, data, header_size);

    Some(MetafileRaster {
        width: gdi.canvas.width,
        height: gdi.canvas.height,
        rgba: gdi.canvas.pixels,
    })
}

/// Walk the record stream after the header.
fn play(gdi: &mut Gdi, data: &[u8], header_size: usize) {
    let mut p = header_size;
    let mut guard = 0usize;
    while p + 8 <= data.len() {
        guard += 1;
        if guard > data.len() {
            break;
        }
        let itype = match rd_u32(data, p) {
            Some(v) => v,
            None => break,
        };
        let nsize = match rd_u32(data, p + 4) {
            Some(v) => v as usize,
            None => break,
        };
        // nSize includes the 8-byte header and is a multiple of 4.
        if nsize < 8 || p + nsize > data.len() {
            break;
        }
        let body = &data[p + 8..p + nsize];
        exec_record(gdi, itype, body);
        if itype == EMR_EOF {
            break;
        }
        p += nsize;
    }
}

/// EMF handles are 1-based indices (handle 0 is reserved). We store objects at
/// `handle-1` in the dense object table, growing it as needed.
fn put_handle(gdi: &mut Gdi, handle: u32, obj: GdiObject) {
    if handle == 0 {
        return;
    }
    let idx = (handle - 1) as usize;
    // Guard against absurd handle indices from crafted input.
    if idx > 1 << 20 {
        return;
    }
    if idx >= gdi.objects.len() {
        gdi.objects.resize(idx + 1, GdiObject::Empty);
    }
    gdi.objects[idx] = obj;
}

/// Execute one EMF record.
fn exec_record(gdi: &mut Gdi, itype: u32, b: &[u8]) {
    match itype {
        // ── Transform / state ──────────────────────────────────────────────
        EMR_SETWINDOWEXTEX => {
            gdi.win_ext = Pt {
                x: i32f(b, 0),
                y: i32f(b, 1),
            };
        }
        EMR_SETWINDOWORGEX => {
            gdi.win_org = Pt {
                x: i32f(b, 0),
                y: i32f(b, 1),
            };
        }
        EMR_SETVIEWPORTEXTEX => {
            gdi.vp_ext = Pt {
                x: i32f(b, 0),
                y: i32f(b, 1),
            };
        }
        EMR_SETVIEWPORTORGEX => {
            gdi.vp_org = Pt {
                x: i32f(b, 0),
                y: i32f(b, 1),
            };
        }
        EMR_SETMAPMODE => {
            gdi.map_mode = i32i(b, 0);
        }
        EMR_SETWORLDTRANSFORM => {
            if let Some(xf) = read_xform(b, 0) {
                gdi.world = xf;
            }
        }
        EMR_MODIFYWORLDTRANSFORM => {
            // XForm(24 bytes) then iMode(4): 1=identity, 2=left-multiply(world∘xf),
            // 3=right-multiply(xf∘world), 4=set.
            if let Some(xf) = read_xform(b, 0) {
                let mode = rd_u32(b, 24).unwrap_or(4);
                gdi.world = match mode {
                    1 => Affine::identity(),
                    2 => gdi.world.concat(&xf),
                    3 => xf.concat(&gdi.world),
                    _ => xf,
                };
            }
        }
        EMR_SETTEXTCOLOR => {
            gdi.text_color = Rgba::from_colorref(rd_u32(b, 0).unwrap_or(0));
        }
        EMR_SETBKCOLOR => {
            gdi.bk_color = Rgba::from_colorref(rd_u32(b, 0).unwrap_or(0));
        }
        EMR_SETBKMODE => {
            gdi.bk_opaque = rd_u32(b, 0).unwrap_or(2) == 2;
        }
        EMR_SETPOLYFILLMODE => {
            gdi.poly_fill_alternate = rd_u32(b, 0).unwrap_or(1) != 2;
        }
        EMR_SETROP2 => {
            // iMode(4): the binary raster op. Applies to vector paint.
            gdi.canvas.rop2 = Rop2::from_u32(rd_u32(b, 0).unwrap_or(13));
        }
        EMR_SETSTRETCHBLTMODE => {
            // iMode(4): nearest vs HALFTONE for DIB blits.
            gdi.stretch_mode = StretchMode::from_u32(rd_u32(b, 0).unwrap_or(3));
        }
        EMR_EXTSELECTCLIPRGN => ext_select_clip_rgn(gdi, b),

        // ── Position / lines ───────────────────────────────────────────────
        EMR_MOVETOEX => {
            gdi.pos = gdi.to_device(i32f(b, 0), i32f(b, 1));
        }
        EMR_LINETO => {
            let to = gdi.to_device(i32f(b, 0), i32f(b, 1));
            let from = gdi.pos;
            gdi.stroke_open_device(&[from, to], false);
            gdi.pos = to;
        }

        // ── Polylines / polygons (32-bit points) ───────────────────────────
        EMR_POLYLINE => poly32(gdi, b, Shape::Polyline),
        EMR_POLYGON => poly32(gdi, b, Shape::Polygon),
        EMR_POLYLINETO => poly32_to(gdi, b, false),
        EMR_POLYBEZIER => bezier32(gdi, b, false),
        EMR_POLYBEZIERTO => bezier32(gdi, b, true),
        EMR_POLYPOLYLINE => poly_poly32(gdi, b, false),
        EMR_POLYPOLYGON => poly_poly32(gdi, b, true),

        // ── Polylines / polygons (16-bit points) ───────────────────────────
        EMR_POLYLINE16 => poly16(gdi, b, Shape::Polyline),
        EMR_POLYGON16 => poly16(gdi, b, Shape::Polygon),
        EMR_POLYLINETO16 => poly16_to(gdi, b),
        EMR_POLYBEZIER16 => bezier16(gdi, b, false),
        EMR_POLYBEZIERTO16 => bezier16(gdi, b, true),
        EMR_POLYPOLYLINE16 => poly_poly16(gdi, b, false),
        EMR_POLYPOLYGON16 => poly_poly16(gdi, b, true),

        // ── Shapes ─────────────────────────────────────────────────────────
        EMR_RECTANGLE => {
            let (l, t, r, bo) = rect4(b);
            let poly = rect_poly(gdi, l, t, r, bo);
            gdi.fill_and_stroke(&[poly]);
        }
        EMR_ELLIPSE => {
            let (l, t, r, bo) = rect4(b);
            let seg = ellipse_seg(gdi, l, t, r, bo);
            let poly = ellipse_poly(gdi, l, t, r, bo, seg);
            gdi.fill_and_stroke(&[poly]);
        }
        EMR_ROUNDRECT => {
            let (l, t, r, bo) = rect4(b);
            // szlCorner at offset 16: cx(4), cy(4)
            let cx = i32f(b, 4);
            let cy = i32f(b, 5);
            let poly = round_rect_poly(gdi, l, t, r, bo, cx / 2.0, cy / 2.0);
            gdi.fill_and_stroke(&[poly]);
        }
        EMR_ARC => arc32(gdi, b, ArcKind::Arc),
        EMR_ARCTO => arc32(gdi, b, ArcKind::Arc),
        EMR_PIE => arc32(gdi, b, ArcKind::Pie),
        EMR_CHORD => arc32(gdi, b, ArcKind::Chord),
        EMR_SETPIXELV => {
            // rclBounds-less: ptlPixel(8) = x,y; crColor(4)
            let x = i32f(b, 0);
            let y = i32f(b, 1);
            let color = Rgba::from_colorref(rd_u32(b, 8).unwrap_or(0));
            let d = gdi.to_device(x, y);
            gdi.canvas
                .blend(d.x.round() as i32, d.y.round() as i32, color, 1.0);
        }

        // ── Regions (reduced to bbox) ───────────────────────────────────────
        EMR_FILLRGN => fill_rgn(gdi, b),
        EMR_PAINTRGN => paint_rgn(gdi, b),

        // ── Object table ────────────────────────────────────────────────────
        EMR_CREATEPEN => create_pen(gdi, b),
        EMR_EXTCREATEPEN => ext_create_pen(gdi, b),
        EMR_CREATEBRUSHINDIRECT => create_brush(gdi, b),
        EMR_EXTCREATEFONTINDIRECTW => create_font(gdi, b),
        EMR_SELECTOBJECT => select_object(gdi, b),
        EMR_DELETEOBJECT => {
            let handle = rd_u32(b, 0).unwrap_or(0);
            if handle != 0 {
                gdi.delete_object((handle - 1) as usize);
            }
        }

        // ── Text ────────────────────────────────────────────────────────────
        EMR_EXTTEXTOUTA => ext_text_out(gdi, b, false),
        EMR_EXTTEXTOUTW => ext_text_out(gdi, b, true),

        // ── DIB blits ──────────────────────────────────────────────────────
        EMR_STRETCHDIBITS => stretch_dibits(gdi, b),
        EMR_SETDIBITSTODEVICE => set_dibits_to_device(gdi, b),
        EMR_BITBLT => bitblt(gdi, b),
        EMR_STRETCHBLT => stretchblt(gdi, b),

        _ => {}
    }
}

// ── readers ─────────────────────────────────────────────────────────────────

/// i32 at dword `d`, as f64.
fn i32f(b: &[u8], d: usize) -> f64 {
    rd_i32(b, d * 4).unwrap_or(0) as f64
}

/// i32 at dword `d`.
fn i32i(b: &[u8], d: usize) -> i32 {
    rd_i32(b, d * 4).unwrap_or(0)
}

/// An `RECTL` (left, top, right, bottom) starting at the body offset 0.
fn rect4(b: &[u8]) -> (f64, f64, f64, f64) {
    (i32f(b, 0), i32f(b, 1), i32f(b, 2), i32f(b, 3))
}

/// Read an EMF `XFORM` (six f32: m11, m12, m21, m22, dx, dy) at dword `d`.
fn read_xform(b: &[u8], d: usize) -> Option<Affine> {
    let m11 = rd_f32(b, d * 4)? as f64;
    let m12 = rd_f32(b, d * 4 + 4)? as f64;
    let m21 = rd_f32(b, d * 4 + 8)? as f64;
    let m22 = rd_f32(b, d * 4 + 12)? as f64;
    let dx = rd_f32(b, d * 4 + 16)? as f64;
    let dy = rd_f32(b, d * 4 + 20)? as f64;
    Some(Affine {
        m11,
        m12,
        m21,
        m22,
        dx,
        dy,
    })
}

fn rd_f32(b: &[u8], o: usize) -> Option<f32> {
    b.get(o..o + 4)
        .map(|s| f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

enum Shape {
    Polyline,
    Polygon,
}

enum ArcKind {
    Arc,
    Pie,
    Chord,
}

// ── 32-bit point arrays ──────────────────────────────────────────────────────

/// Body of EMR_POLYLINE/POLYGON: rclBounds(16), count(4), points(count × 2×i32).
fn poly32(gdi: &mut Gdi, b: &[u8], shape: Shape) {
    let count = rd_u32(b, 16).unwrap_or(0) as usize;
    let pts = read_points32(gdi, b, 20, count);
    emit_shape(gdi, pts, shape);
}

fn poly32_to(gdi: &mut Gdi, b: &[u8], _bez: bool) {
    let count = rd_u32(b, 16).unwrap_or(0) as usize;
    let mut pts = read_points32(gdi, b, 20, count);
    if pts.is_empty() {
        return;
    }
    let mut all = vec![gdi.pos];
    all.append(&mut pts);
    gdi.stroke_open_device(&all, false);
    if let Some(last) = all.last() {
        gdi.pos = *last;
    }
}

fn bezier32(gdi: &mut Gdi, b: &[u8], relative_to: bool) {
    let count = rd_u32(b, 16).unwrap_or(0) as usize;
    let ctrl = read_points32(gdi, b, 20, count);
    emit_bezier(gdi, ctrl, relative_to);
}

/// EMR_POLYPOLYLINE/POLYGON: rclBounds(16), nPolys(4), totalPts(4), counts[],
/// points[].
fn poly_poly32(gdi: &mut Gdi, b: &[u8], polygon: bool) {
    let n = rd_u32(b, 16).unwrap_or(0) as usize;
    let total = rd_u32(b, 20).unwrap_or(0) as usize;
    if n == 0 || n > total.max(1) + 1 {
        return;
    }
    let mut counts = Vec::with_capacity(n);
    for i in 0..n {
        counts.push(rd_u32(b, 24 + i * 4).unwrap_or(0) as usize);
    }
    let pts_off = 24 + n * 4;
    let mut word = 0usize;
    let mut polys = Vec::new();
    for &c in &counts {
        let pts = read_points32(gdi, b, pts_off + word * 8, c);
        word += c;
        if pts.len() >= 2 {
            polys.push(pts);
        }
    }
    if polys.is_empty() {
        return;
    }
    if polygon {
        gdi.fill_and_stroke(&polys);
    } else {
        for p in &polys {
            gdi.stroke_open_device(p, false);
        }
    }
}

fn read_points32(gdi: &Gdi, b: &[u8], off: usize, count: usize) -> Vec<Pt> {
    let mut pts = Vec::with_capacity(count.min(1 << 16));
    for i in 0..count {
        let o = off + i * 8;
        let (Some(x), Some(y)) = (rd_i32(b, o), rd_i32(b, o + 4)) else {
            break;
        };
        pts.push(gdi.to_device(x as f64, y as f64));
    }
    pts
}

// ── 16-bit point arrays (EMF *16 records) ────────────────────────────────────

fn poly16(gdi: &mut Gdi, b: &[u8], shape: Shape) {
    let count = rd_u32(b, 16).unwrap_or(0) as usize;
    let pts = read_points16(gdi, b, 20, count);
    emit_shape(gdi, pts, shape);
}

fn poly16_to(gdi: &mut Gdi, b: &[u8]) {
    let count = rd_u32(b, 16).unwrap_or(0) as usize;
    let mut pts = read_points16(gdi, b, 20, count);
    if pts.is_empty() {
        return;
    }
    let mut all = vec![gdi.pos];
    all.append(&mut pts);
    gdi.stroke_open_device(&all, false);
    if let Some(last) = all.last() {
        gdi.pos = *last;
    }
}

fn bezier16(gdi: &mut Gdi, b: &[u8], relative_to: bool) {
    let count = rd_u32(b, 16).unwrap_or(0) as usize;
    let ctrl = read_points16(gdi, b, 20, count);
    emit_bezier(gdi, ctrl, relative_to);
}

fn poly_poly16(gdi: &mut Gdi, b: &[u8], polygon: bool) {
    let n = rd_u32(b, 16).unwrap_or(0) as usize;
    let total = rd_u32(b, 20).unwrap_or(0) as usize;
    if n == 0 || n > total.max(1) + 1 {
        return;
    }
    let mut counts = Vec::with_capacity(n);
    for i in 0..n {
        counts.push(rd_u32(b, 24 + i * 4).unwrap_or(0) as usize);
    }
    let pts_off = 24 + n * 4;
    let mut word = 0usize;
    let mut polys = Vec::new();
    for &c in &counts {
        let pts = read_points16(gdi, b, pts_off + word * 4, c);
        word += c;
        if pts.len() >= 2 {
            polys.push(pts);
        }
    }
    if polys.is_empty() {
        return;
    }
    if polygon {
        gdi.fill_and_stroke(&polys);
    } else {
        for p in &polys {
            gdi.stroke_open_device(p, false);
        }
    }
}

fn read_points16(gdi: &Gdi, b: &[u8], off: usize, count: usize) -> Vec<Pt> {
    let mut pts = Vec::with_capacity(count.min(1 << 16));
    for i in 0..count {
        let o = off + i * 4;
        let (Some(x), Some(y)) = (rd_i16(b, o), rd_i16(b, o + 2)) else {
            break;
        };
        pts.push(gdi.to_device(x as f64, y as f64));
    }
    pts
}

// ── shape emission ───────────────────────────────────────────────────────────

fn emit_shape(gdi: &mut Gdi, pts: Vec<Pt>, shape: Shape) {
    if pts.len() < 2 {
        return;
    }
    match shape {
        Shape::Polyline => gdi.stroke_open_device(&pts, false),
        Shape::Polygon => gdi.fill_and_stroke(&[pts]),
    }
}

/// Flatten a poly-Bézier (cubic segments) into a device polyline and stroke it.
/// `relative_to` prepends the current position as the first anchor.
///
/// Each cubic is flattened **adaptively** (recursive de Casteljau): a segment
/// subdivides only while its control points stray farther than a device-pixel
/// chord tolerance from the chord, so gentle curves emit a handful of points
/// while tight ones get many — sharper than the old fixed 24-step sampling, and
/// cheaper on near-straight runs. The anchors are already in device pixels (the
/// callers mapped them through the transform), so the tolerance is a true
/// on-screen flatness bound.
fn emit_bezier(gdi: &mut Gdi, ctrl: Vec<Pt>, relative_to: bool) {
    let mut anchors: Vec<Pt> = Vec::new();
    if relative_to {
        anchors.push(gdi.pos);
    }
    anchors.extend(ctrl);
    if anchors.len() < 4 {
        return;
    }
    // ~¼ device pixel: visually exact yet bounded in point count.
    const TOL: f64 = 0.25;
    let mut flat: Vec<Pt> = vec![anchors[0]];
    let mut i = 1;
    while i + 2 < anchors.len() {
        let p0 = *flat.last().unwrap();
        let p1 = anchors[i];
        let p2 = anchors[i + 1];
        let p3 = anchors[i + 2];
        flatten_cubic(p0, p1, p2, p3, TOL, 0, &mut flat);
        i += 3;
    }
    gdi.stroke_open_device(&flat, false);
    if let Some(last) = flat.last() {
        gdi.pos = *last;
    }
}

/// Recursively flatten the cubic `p0..p3` (device pixels) into `out`, appending
/// every point **after** `p0` (the caller already pushed `p0`). Subdivides while
/// the curve's flatness (max control-point deviation from the `p0→p3` chord)
/// exceeds `tol`, capped at `depth` 18 to bound the recursion on pathological
/// control nets.
fn flatten_cubic(p0: Pt, p1: Pt, p2: Pt, p3: Pt, tol: f64, depth: u32, out: &mut Vec<Pt>) {
    const MAX_DEPTH: u32 = 18;
    if depth >= MAX_DEPTH || cubic_is_flat(p0, p1, p2, p3, tol) {
        out.push(p3);
        return;
    }
    // de Casteljau split at t = 0.5.
    let p01 = midpoint(p0, p1);
    let p12 = midpoint(p1, p2);
    let p23 = midpoint(p2, p3);
    let p012 = midpoint(p01, p12);
    let p123 = midpoint(p12, p23);
    let mid = midpoint(p012, p123);
    flatten_cubic(p0, p01, p012, mid, tol, depth + 1, out);
    flatten_cubic(mid, p123, p23, p3, tol, depth + 1, out);
}

/// `true` when the cubic is within `tol` device pixels of its `p0→p3` chord —
/// i.e. both inner control points lie close enough to the chord line. Uses the
/// standard distance-of-control-point-to-chord flatness test.
fn cubic_is_flat(p0: Pt, p1: Pt, p2: Pt, p3: Pt, tol: f64) -> bool {
    let dx = p3.x - p0.x;
    let dy = p3.y - p0.y;
    let len2 = dx * dx + dy * dy;
    if len2 <= 1e-12 {
        // Degenerate chord (p0 ≈ p3): fall back to the raw spread of controls.
        let d1 = (p1.x - p0.x).hypot(p1.y - p0.y);
        let d2 = (p2.x - p0.x).hypot(p2.y - p0.y);
        return d1.max(d2) <= tol;
    }
    // Perpendicular distance of p1 and p2 to the chord line through p0,p3.
    let d1 = ((p1.x - p0.x) * dy - (p1.y - p0.y) * dx).abs();
    let d2 = ((p2.x - p0.x) * dy - (p2.y - p0.y) * dx).abs();
    let inv_len = 1.0 / len2.sqrt();
    (d1 * inv_len).max(d2 * inv_len) <= tol
}

fn midpoint(a: Pt, b: Pt) -> Pt {
    Pt {
        x: (a.x + b.x) * 0.5,
        y: (a.y + b.y) * 0.5,
    }
}

fn ellipse_seg(gdi: &Gdi, l: f64, t: f64, r: f64, b: f64) -> usize {
    let a = gdi.to_device(l, t);
    let c = gdi.to_device(r, b);
    (((c.x - a.x).hypot(c.y - a.y) as usize) / 3).clamp(24, 200)
}

/// EMR_ARC/ARCTO/PIE/CHORD body: rclBox(16), ptlStart(8), ptlEnd(8).
fn arc32(gdi: &mut Gdi, b: &[u8], kind: ArcKind) {
    let (l, t, r, bo) = rect4(b);
    let x_start = i32f(b, 4);
    let y_start = i32f(b, 5);
    let x_end = i32f(b, 6);
    let y_end = i32f(b, 7);
    let cx = (l + r) / 2.0;
    let cy = (t + bo) / 2.0;
    let rx = (r - l).abs() / 2.0;
    let ry = (bo - t).abs() / 2.0;
    let seg = (((rx + ry) as usize) / 3).clamp(16, 200);
    let pts = arc_points(gdi, l, t, r, bo, x_start, y_start, x_end, y_end, seg);
    match kind {
        ArcKind::Arc => gdi.stroke_open_device(&pts, false),
        ArcKind::Pie => {
            let mut poly = pts;
            poly.push(gdi.to_device(cx, cy));
            gdi.fill_and_stroke(&[poly]);
        }
        ArcKind::Chord => gdi.fill_and_stroke(&[pts]),
    }
}

// ── object creation ─────────────────────────────────────────────────────────

/// EMR_CREATEPEN: ihPen(4), LOGPEN { style(4), width.x(4), width.y(4),
/// COLORREF(4) }.
fn create_pen(gdi: &mut Gdi, b: &[u8]) {
    let handle = rd_u32(b, 0).unwrap_or(0);
    let style = PenStyle::from_u32(rd_u32(b, 4).unwrap_or(0));
    let width = i32f(b, 2).abs(); // width.x at dword 2
    let color = Rgba::from_colorref(rd_u32(b, 16).unwrap_or(0));
    put_handle(
        gdi,
        handle,
        GdiObject::Pen(Pen {
            style,
            width,
            color,
        }),
    );
}

/// EMR_EXTCREATEPEN: ihPen(4), offBmi(4), cbBmi(4), offBits(4), cbBits(4),
/// then EXTLOGPEN { elpPenStyle(4), elpWidth(4), elpBrushStyle(4),
/// elpColor(4), elpHatch(4), elpNumEntries(4), elpStyleEntry[] }.
fn ext_create_pen(gdi: &mut Gdi, b: &[u8]) {
    let handle = rd_u32(b, 0).unwrap_or(0);
    // EXTLOGPEN begins at dword 5 (after the 5 offset/count dwords).
    let style_raw = rd_u32(b, 20).unwrap_or(0);
    let width = (rd_u32(b, 24).unwrap_or(1) as f64).max(0.0);
    let color = Rgba::from_colorref(rd_u32(b, 32).unwrap_or(0));
    let style = PenStyle::from_u32(style_raw);
    put_handle(
        gdi,
        handle,
        GdiObject::Pen(Pen {
            style,
            width,
            color,
        }),
    );
}

/// EMR_CREATEBRUSHINDIRECT: ihBrush(4), LOGBRUSH32 { style(4), COLORREF(4),
/// hatch(4) }.
fn create_brush(gdi: &mut Gdi, b: &[u8]) {
    let handle = rd_u32(b, 0).unwrap_or(0);
    let style_raw = rd_u32(b, 4).unwrap_or(0);
    let color = Rgba::from_colorref(rd_u32(b, 8).unwrap_or(0));
    let hatch_raw = rd_u32(b, 12).unwrap_or(0); // lbHatch (HS_* selector)
    let brush = match style_raw {
        0 => Brush::solid(color),                                    // BS_SOLID
        1 => Brush::null(),                                          // BS_NULL
        2 => Brush::hatched(color, HatchStyle::from_u32(hatch_raw)), // BS_HATCHED
        _ => Brush::solid(color),
    };
    put_handle(gdi, handle, GdiObject::Brush(brush));
}

/// EMR_EXTCREATEFONTINDIRECTW: ihFont(4), then LOGFONT(W) — height(4,i32),
/// width(4,i32), escapement(4), orientation(4), weight(4), italic(1) …
fn create_font(gdi: &mut Gdi, b: &[u8]) {
    let handle = rd_u32(b, 0).unwrap_or(0);
    let height = i32f(b, 1).abs();
    let width = i32f(b, 2).abs();
    let escapement = rd_i32(b, 12).unwrap_or(0);
    let weight = rd_i32(b, 20).unwrap_or(400);
    let italic = b.get(24).copied().unwrap_or(0) != 0;
    put_handle(
        gdi,
        handle,
        GdiObject::Font(Font {
            height: if height > 0.0 { height } else { 12.0 },
            width,
            escapement,
            bold: weight >= 600,
            italic,
        }),
    );
}

fn select_object(gdi: &mut Gdi, b: &[u8]) {
    let handle = rd_u32(b, 0).unwrap_or(0);
    if handle & 0x8000_0000 != 0 {
        // Stock object: map a few common ones; otherwise leave state unchanged.
        apply_stock_object(gdi, handle & 0x7FFF_FFFF);
        return;
    }
    if handle != 0 {
        gdi.select_object((handle - 1) as usize);
    }
}

/// Apply a GDI stock object (`SelectObject` with the 0x8000_0000 flag set).
fn apply_stock_object(gdi: &mut Gdi, id: u32) {
    match id {
        0 => gdi.cur_brush = Brush::white(), // WHITE_BRUSH
        1 => gdi.cur_brush = Brush::solid(Rgba::rgb(192, 192, 192)), // LTGRAY_BRUSH
        2 => gdi.cur_brush = Brush::solid(Rgba::rgb(128, 128, 128)), // GRAY_BRUSH
        3 => gdi.cur_brush = Brush::solid(Rgba::rgb(64, 64, 64)), // DKGRAY_BRUSH
        4 => gdi.cur_brush = Brush::solid(Rgba::rgb(0, 0, 0)), // BLACK_BRUSH
        5 => gdi.cur_brush = Brush::null(),  // NULL_BRUSH
        6 => gdi.cur_pen = Pen::cosmetic_black(), // WHITE_PEN handled as black? no:
        7 => gdi.cur_pen = Pen::cosmetic_black(), // BLACK_PEN
        8 => {
            gdi.cur_pen = Pen {
                style: PenStyle::Null,
                width: 0.0,
                color: Rgba::TRANSPARENT,
            }
        } // NULL_PEN
        _ => {}
    }
    // WHITE_PEN (6) is white; fix after the match for clarity.
    if id == 6 {
        gdi.cur_pen = Pen {
            style: PenStyle::Solid,
            width: 0.0,
            color: Rgba::rgb(255, 255, 255),
        };
    }
}

// ── regions ─────────────────────────────────────────────────────────────────

/// EMR_FILLRGN: rclBounds(16), cbRgnData(4), ihBrush(4), RGNDATA…
fn fill_rgn(gdi: &mut Gdi, b: &[u8]) {
    let brush_handle = rd_u32(b, 20).unwrap_or(0);
    let color = match (
        brush_handle,
        gdi.objects.get(brush_handle.wrapping_sub(1) as usize),
    ) {
        (h, Some(GdiObject::Brush(br))) if h != 0 && br.style != BrushStyle::Null => br.color,
        _ => return,
    };
    // RGNDATA follows rclBounds(16) + cbRgnData(4) + ihBrush(4) = byte 24.
    let rects = parse_emf_rgndata(b, 24);
    let polys = region_polys(gdi, &rects, b);
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

/// EMR_PAINTRGN: rclBounds(16), cbRgnData(4), RGNDATA… → current brush.
fn paint_rgn(gdi: &mut Gdi, b: &[u8]) {
    if gdi.cur_brush.style == BrushStyle::Null {
        return;
    }
    let color = gdi.cur_brush.color;
    let alt = gdi.poly_fill_alternate;
    // RGNDATA follows rclBounds(16) + cbRgnData(4) = byte 20.
    let rects = parse_emf_rgndata(b, 20);
    let polys = region_polys(gdi, &rects, b);
    if !polys.is_empty() {
        fill_polygons(&mut gdi.canvas, &polys, color, alt, gdi.clip.as_ref());
    }
}

/// EMR_EXTSELECTCLIPRGN: cbRgnData(4), iMode(4), RGNDATA… — set the device clip
/// to the **union** (bounding rect) of the region's rectangles. `RGN_COPY`
/// (iMode 5) replaces the clip; an empty region clears it.
fn ext_select_clip_rgn(gdi: &mut Gdi, b: &[u8]) {
    // RGNDATA follows cbRgnData(4) + iMode(4) = byte 8.
    let rects = parse_emf_rgndata(b, 8);
    let bbox = region_bbox(&rects);
    gdi.set_clip_logrect(bbox);
}

/// Build device polygons for region `rects`; falls back to the record's
/// `rclBounds` rectangle when the RGNDATA carried no parsable rectangle (so a
/// region we couldn't fully decode still paints its bounding box, not nothing).
fn region_polys(gdi: &Gdi, rects: &[LogRect], b: &[u8]) -> Vec<Vec<Pt>> {
    if rects.is_empty() {
        let (l, t, r, bo) = rgn_bounds(b);
        if (r - l).abs() < 1e-9 && (bo - t).abs() < 1e-9 {
            return Vec::new();
        }
        return vec![rect_poly(gdi, l, t, r, bo)];
    }
    rects
        .iter()
        .map(|rc| rect_poly(gdi, rc.left, rc.top, rc.right, rc.bottom))
        .collect()
}

/// Parse an EMF `RGNDATA` blob starting at body byte `off` into its rectangle
/// list. The `RGNDATAHEADER` is `dwSize(4), iType(4), nCount(4), nRgnSize(4),
/// rcBound(16)`; the `RDH_RECTANGLES` body that follows is `nCount` `RECTL`s
/// (`left,top,right,bottom`, 4×i32). Bounds-checked; an out-of-range count is
/// capped to the available bytes (never panics, never allocates unboundedly).
fn parse_emf_rgndata(b: &[u8], off: usize) -> Vec<LogRect> {
    // Need at least the 32-byte RGNDATAHEADER.
    if off + 32 > b.len() {
        return Vec::new();
    }
    let count = rd_u32(b, off + 8).unwrap_or(0) as usize;
    let rects_off = off + 32; // header is 32 bytes
    let avail = b.len().saturating_sub(rects_off) / 16; // 16 bytes per RECTL
    let count = count.min(avail);
    let mut rects = Vec::with_capacity(count.min(1 << 16));
    for i in 0..count {
        let o = rects_off + i * 16;
        let (Some(l), Some(t), Some(r), Some(bo)) = (
            rd_i32(b, o),
            rd_i32(b, o + 4),
            rd_i32(b, o + 8),
            rd_i32(b, o + 12),
        ) else {
            break;
        };
        rects.push(LogRect {
            left: l as f64,
            top: t as f64,
            right: r as f64,
            bottom: bo as f64,
        });
    }
    rects
}

/// The `rclBounds` rectangle at the start of a region record body.
fn rgn_bounds(b: &[u8]) -> (f64, f64, f64, f64) {
    (i32f(b, 0), i32f(b, 1), i32f(b, 2), i32f(b, 3))
}

// ── text ─────────────────────────────────────────────────────────────────────

/// EMR_EXTTEXTOUTA/W: rclBounds(16), iGraphicsMode(4), exScale(4), eyScale(4),
/// EMRTEXT { ptlReference(8), nChars(4), offString(4), fOptions(4), rcl(16),
/// offDx(4) }, then the string at `offString` from the record start (= body+8).
fn ext_text_out(gdi: &mut Gdi, b: &[u8], wide: bool) {
    // EMRTEXT starts at body offset 28 (after rclBounds + 3 dwords).
    let ref_x = rd_i32(b, 28).unwrap_or(0) as f64;
    let ref_y = rd_i32(b, 32).unwrap_or(0) as f64;
    let nchars = rd_u32(b, 36).unwrap_or(0) as usize;
    let off_string = rd_u32(b, 40).unwrap_or(0) as usize;
    // offString is from the start of the record (8 bytes before the body).
    let str_off = off_string.saturating_sub(8);
    if nchars == 0 {
        return;
    }
    // Per-cell visibility mask (skip NUL/space, preserve advance for the rest).
    let runs: Vec<bool> = (0..nchars)
        .map(|i| {
            if wide {
                let cu = rd_u16(b, str_off + i * 2).unwrap_or(0);
                cu != 0 && cu != 0x20
            } else {
                let c = b.get(str_off + i).copied().unwrap_or(0);
                c != 0 && c != b' '
            }
        })
        .collect();
    if runs.iter().all(|v| !*v) {
        return;
    }
    gdi.draw_text(ref_x, ref_y, &runs);
}

// ── DIB blits ───────────────────────────────────────────────────────────────

/// EMR_STRETCHDIBITS: rclBounds(16), xDest(4), yDest(4), xSrc(4), ySrc(4),
/// cxSrc(4), cySrc(4), offBmiSrc(4), cbBmiSrc(4), offBitsSrc(4), cbBitsSrc(4),
/// iUsageSrc(4), dwRop(4), cxDest(4), cyDest(4).
fn stretch_dibits(gdi: &mut Gdi, b: &[u8]) {
    let x_dest = i32f(b, 4);
    let y_dest = i32f(b, 5);
    let off_bmi = rd_u32(b, 32).unwrap_or(0) as usize;
    let cb_bmi = rd_u32(b, 36).unwrap_or(0) as usize;
    let off_bits = rd_u32(b, 40).unwrap_or(0) as usize;
    let cb_bits = rd_u32(b, 44).unwrap_or(0) as usize;
    let cx_dest = i32f(b, 13);
    let cy_dest = i32f(b, 14);
    blit_emf_dib(
        gdi, b, off_bmi, cb_bmi, off_bits, cb_bits, x_dest, y_dest, cx_dest, cy_dest,
    );
}

/// EMR_SETDIBITSTODEVICE: rclBounds(16), xDest(4), yDest(4), xSrc(4), ySrc(4),
/// cxSrc(4), cySrc(4), offBmiSrc(4), cbBmiSrc(4), offBitsSrc(4), cbBitsSrc(4),
/// iUsageSrc(4), iStartScan(4), cScans(4). 1:1 dest = src size.
fn set_dibits_to_device(gdi: &mut Gdi, b: &[u8]) {
    let x_dest = i32f(b, 4);
    let y_dest = i32f(b, 5);
    let cx_src = i32f(b, 8);
    let cy_src = i32f(b, 9);
    let off_bmi = rd_u32(b, 32).unwrap_or(0) as usize;
    let cb_bmi = rd_u32(b, 36).unwrap_or(0) as usize;
    let off_bits = rd_u32(b, 40).unwrap_or(0) as usize;
    let cb_bits = rd_u32(b, 44).unwrap_or(0) as usize;
    blit_emf_dib(
        gdi, b, off_bmi, cb_bmi, off_bits, cb_bits, x_dest, y_dest, cx_src, cy_src,
    );
}

/// EMR_BITBLT: rclBounds(16), xDest(4), yDest(4), cxDest(4), cyDest(4),
/// dwRop(4), xSrc(4), ySrc(4), XformSrc(24), crBkColorSrc(4), iUsageSrc(4),
/// offBmiSrc(4), cbBmiSrc(4), offBitsSrc(4), cbBitsSrc(4).
fn bitblt(gdi: &mut Gdi, b: &[u8]) {
    let x_dest = i32f(b, 4);
    let y_dest = i32f(b, 5);
    let cx_dest = i32f(b, 6);
    let cy_dest = i32f(b, 7);
    let off_bmi = rd_u32(b, 84).unwrap_or(0) as usize;
    let cb_bmi = rd_u32(b, 88).unwrap_or(0) as usize;
    let off_bits = rd_u32(b, 92).unwrap_or(0) as usize;
    let cb_bits = rd_u32(b, 96).unwrap_or(0) as usize;
    if off_bmi == 0 || cb_bmi == 0 {
        return; // pattern-only blit (no source bitmap)
    }
    blit_emf_dib(
        gdi, b, off_bmi, cb_bmi, off_bits, cb_bits, x_dest, y_dest, cx_dest, cy_dest,
    );
}

/// EMR_STRETCHBLT: rclBounds(16), xDest(4), yDest(4), cxDest(4), cyDest(4),
/// dwRop(4), xSrc(4), ySrc(4), XformSrc(24), crBkColorSrc(4), iUsageSrc(4),
/// offBmiSrc(4), cbBmiSrc(4), offBitsSrc(4), cbBitsSrc(4), cxSrc(4), cySrc(4).
fn stretchblt(gdi: &mut Gdi, b: &[u8]) {
    let x_dest = i32f(b, 4);
    let y_dest = i32f(b, 5);
    let cx_dest = i32f(b, 6);
    let cy_dest = i32f(b, 7);
    let off_bmi = rd_u32(b, 84).unwrap_or(0) as usize;
    let cb_bmi = rd_u32(b, 88).unwrap_or(0) as usize;
    let off_bits = rd_u32(b, 92).unwrap_or(0) as usize;
    let cb_bits = rd_u32(b, 96).unwrap_or(0) as usize;
    if off_bmi == 0 || cb_bmi == 0 {
        return;
    }
    blit_emf_dib(
        gdi, b, off_bmi, cb_bmi, off_bits, cb_bits, x_dest, y_dest, cx_dest, cy_dest,
    );
}

/// Reassemble the split BITMAPINFO (`off_bmi`/`cb_bmi`) and pixel bits
/// (`off_bits`/`cb_bits`) — offsets from the *record* start (body − 8) — into a
/// packed DIB, decode it, and blit to the logical dest rect. Honors negative
/// dest extents (mirroring).
#[allow(clippy::too_many_arguments)]
fn blit_emf_dib(
    gdi: &mut Gdi,
    b: &[u8],
    off_bmi: usize,
    cb_bmi: usize,
    off_bits: usize,
    cb_bits: usize,
    dx: f64,
    dy: f64,
    dw: f64,
    dh: f64,
) {
    if off_bmi == 0 || cb_bmi == 0 {
        return;
    }
    // Offsets are from the record start; the body slice begins 8 bytes in.
    let bmi_start = off_bmi.saturating_sub(8);
    let bits_start = off_bits.saturating_sub(8);
    let Some(bmi) = b.get(bmi_start..bmi_start + cb_bmi) else {
        return;
    };
    let bits = b.get(bits_start..bits_start + cb_bits).unwrap_or(&[]);
    // Concatenate header+palette and the bits into one packed DIB buffer.
    let mut packed = Vec::with_capacity(bmi.len() + bits.len());
    packed.extend_from_slice(bmi);
    packed.extend_from_slice(bits);
    let Some(dib) = decode_packed_dib(&packed) else {
        return;
    };
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

    // ── tiny EMF builder ─────────────────────────────────────────────────────

    fn pu16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn pu32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn pi32(v: &mut Vec<u8>, x: i32) {
        v.extend_from_slice(&(x as u32).to_le_bytes());
    }

    fn record(out: &mut Vec<u8>, itype: u32, body: &[u8]) {
        let mut body = body.to_vec();
        while !body.len().is_multiple_of(4) {
            body.push(0);
        }
        pu32(out, itype);
        pu32(out, 8 + body.len() as u32);
        out.extend_from_slice(&body);
    }

    fn emf_with(w: i32, h: i32, records: &[u8]) -> Vec<u8> {
        let mut header = Vec::new();
        pi32(&mut header, 0);
        pi32(&mut header, 0);
        pi32(&mut header, w - 1);
        pi32(&mut header, h - 1);
        pi32(&mut header, 0);
        pi32(&mut header, 0);
        pi32(&mut header, w * 26);
        pi32(&mut header, h * 26);
        pu32(&mut header, 0x464D_4520); // " EMF"
        pu32(&mut header, 0x0001_0000);
        pu32(&mut header, 0);
        pu32(&mut header, 0);
        pu16(&mut header, 0);
        pu16(&mut header, 0);
        pu32(&mut header, 0);
        pu32(&mut header, 0);
        pu32(&mut header, 0);
        pi32(&mut header, w);
        pi32(&mut header, h);
        pi32(&mut header, w);
        pi32(&mut header, h);

        let mut v = Vec::new();
        pu32(&mut v, 1);
        pu32(&mut v, 8 + header.len() as u32);
        v.extend_from_slice(&header);
        v.extend_from_slice(records);
        v
    }

    fn px(m: &MetafileRaster, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * m.width + x) * 4) as usize;
        (m.rgba[i], m.rgba[i + 1], m.rgba[i + 2], m.rgba[i + 3])
    }

    /// Body for EMR_CREATEBRUSHINDIRECT (solid): ihBrush, style, colorref, hatch.
    fn solid_brush(handle: u32, colorref: u32) -> Vec<u8> {
        let mut b = Vec::new();
        pu32(&mut b, handle);
        pu32(&mut b, 0); // BS_SOLID
        pu32(&mut b, colorref);
        pu32(&mut b, 0);
        b
    }

    fn select(handle: u32) -> Vec<u8> {
        let mut b = Vec::new();
        pu32(&mut b, handle);
        b
    }

    /// An EMF RGNDATA blob of one rectangle list.
    fn rgndata(rects: &[(i32, i32, i32, i32)]) -> Vec<u8> {
        let mut b = Vec::new();
        // RGNDATAHEADER: dwSize(32), iType(1=RDH_RECTANGLES), nCount, nRgnSize, rcBound(16)
        pu32(&mut b, 32);
        pu32(&mut b, 1);
        pu32(&mut b, rects.len() as u32);
        pu32(&mut b, (rects.len() * 16) as u32);
        // bounding rect (union)
        let l = rects.iter().map(|r| r.0).min().unwrap_or(0);
        let t = rects.iter().map(|r| r.1).min().unwrap_or(0);
        let r = rects.iter().map(|r| r.2).max().unwrap_or(0);
        let bo = rects.iter().map(|r| r.3).max().unwrap_or(0);
        pi32(&mut b, l);
        pi32(&mut b, t);
        pi32(&mut b, r);
        pi32(&mut b, bo);
        for &(rl, rt, rr, rb) in rects {
            pi32(&mut b, rl);
            pi32(&mut b, rt);
            pi32(&mut b, rr);
            pi32(&mut b, rb);
        }
        b
    }

    // ── #179 EXTSELECTCLIPRGN ────────────────────────────────────────────────

    #[test]
    fn ext_select_clip_rgn_bounds_subsequent_fill() {
        let (w, h) = (100i32, 100i32);
        let mut recs = Vec::new();
        // EXTSELECTCLIPRGN: cbRgnData(4), iMode(4=RGN_COPY), RGNDATA{ (0,0)-(40,40) }.
        let rd = rgndata(&[(0, 0, 40, 40)]);
        let mut body = Vec::new();
        pu32(&mut body, rd.len() as u32); // cbRgnData
        pu32(&mut body, 5); // RGN_COPY
        body.extend_from_slice(&rd);
        record(&mut recs, EMR_EXTSELECTCLIPRGN, &body);
        // Null pen, solid blue brush, full-canvas rectangle.
        record(
            &mut recs,
            EMR_CREATEBRUSHINDIRECT,
            &solid_brush(1, 0x00FF_0000),
        );
        record(&mut recs, EMR_SELECTOBJECT, &select(0x8000_0008)); // NULL_PEN stock
        record(&mut recs, EMR_SELECTOBJECT, &select(1));
        let mut rectb = Vec::new();
        pi32(&mut rectb, 0);
        pi32(&mut rectb, 0);
        pi32(&mut rectb, 99);
        pi32(&mut rectb, 99);
        record(&mut recs, EMR_RECTANGLE, &rectb);
        record(&mut recs, EMR_EOF, &[0, 0, 0, 0]);
        let m = decode(&emf_with(w, h, &recs)).expect("decode");

        let inside = px(&m, 15, 15);
        let outside = px(&m, 80, 80);
        assert!(inside.3 > 0, "inside clip painted, got {inside:?}");
        assert_eq!(outside.3, 0, "outside clip empty, got {outside:?}");
    }

    // ── #182 FILLRGN union ───────────────────────────────────────────────────

    #[test]
    fn fill_rgn_paints_union_of_rectangles() {
        let (w, h) = (100i32, 40i32);
        let mut recs = Vec::new();
        // Brush handle 1.
        record(
            &mut recs,
            EMR_CREATEBRUSHINDIRECT,
            &solid_brush(1, 0x0000_0000),
        );
        // EMR_FILLRGN: rclBounds(16), cbRgnData(4), ihBrush(4), RGNDATA.
        let rd = rgndata(&[(0, 0, 30, 40), (70, 0, 100, 40)]);
        let mut body = Vec::new();
        pi32(&mut body, 0); // rclBounds
        pi32(&mut body, 0);
        pi32(&mut body, 100);
        pi32(&mut body, 40);
        pu32(&mut body, rd.len() as u32); // cbRgnData
        pu32(&mut body, 1); // ihBrush
        body.extend_from_slice(&rd);
        record(&mut recs, EMR_FILLRGN, &body);
        record(&mut recs, EMR_EOF, &[0, 0, 0, 0]);
        let m = decode(&emf_with(w, h, &recs)).expect("decode");

        assert!(px(&m, 10, 20).3 > 0, "left rect painted");
        assert!(px(&m, 90, 20).3 > 0, "right rect painted");
        assert_eq!(
            px(&m, 50, 20).3,
            0,
            "gap between rects empty (union, not bbox)"
        );
    }

    // ── #176/#180 SETROP2 ────────────────────────────────────────────────────

    #[test]
    fn setrop2_white_forces_white_fill() {
        let (w, h) = (60i32, 60i32);
        let mut recs = Vec::new();
        let mut rop = Vec::new();
        pu32(&mut rop, 16); // R2_WHITE
        record(&mut recs, EMR_SETROP2, &rop);
        record(
            &mut recs,
            EMR_CREATEBRUSHINDIRECT,
            &solid_brush(1, 0x0000_00FF),
        ); // red
        record(&mut recs, EMR_SELECTOBJECT, &select(0x8000_0008)); // NULL_PEN
        record(&mut recs, EMR_SELECTOBJECT, &select(1));
        let mut rectb = Vec::new();
        pi32(&mut rectb, 10);
        pi32(&mut rectb, 10);
        pi32(&mut rectb, 50);
        pi32(&mut rectb, 50);
        record(&mut recs, EMR_RECTANGLE, &rectb);
        record(&mut recs, EMR_EOF, &[0, 0, 0, 0]);
        let m = decode(&emf_with(w, h, &recs)).expect("decode");
        let c = px(&m, 30, 30);
        assert!(c.3 > 0, "centre painted");
        assert!(
            c.0 > 220 && c.1 > 220 && c.2 > 220,
            "R2_WHITE → white, got {c:?}"
        );
    }

    // ── #181 adaptive Bézier ─────────────────────────────────────────────────

    #[test]
    fn cubic_flatness_detects_straight_and_curved() {
        // A perfectly straight cubic is flat at any sane tolerance.
        let straight = (
            Pt { x: 0.0, y: 0.0 },
            Pt { x: 10.0, y: 0.0 },
            Pt { x: 20.0, y: 0.0 },
            Pt { x: 30.0, y: 0.0 },
        );
        assert!(cubic_is_flat(
            straight.0, straight.1, straight.2, straight.3, 0.25
        ));
        // A sharply bowed cubic is NOT flat.
        let curved = (
            Pt { x: 0.0, y: 0.0 },
            Pt { x: 10.0, y: 40.0 },
            Pt { x: 20.0, y: 40.0 },
            Pt { x: 30.0, y: 0.0 },
        );
        assert!(!cubic_is_flat(curved.0, curved.1, curved.2, curved.3, 0.25));
    }

    #[test]
    fn flatten_cubic_is_adaptive_curve_denser_than_line() {
        // A straight cubic flattens to a single appended point …
        let mut line = Vec::new();
        flatten_cubic(
            Pt { x: 0.0, y: 0.0 },
            Pt { x: 30.0, y: 0.0 },
            Pt { x: 60.0, y: 0.0 },
            Pt { x: 90.0, y: 0.0 },
            0.25,
            0,
            &mut line,
        );
        // … while a strongly curved one yields many more.
        let mut curve = Vec::new();
        flatten_cubic(
            Pt { x: 0.0, y: 0.0 },
            Pt { x: 30.0, y: 90.0 },
            Pt { x: 60.0, y: 90.0 },
            Pt { x: 90.0, y: 0.0 },
            0.25,
            0,
            &mut curve,
        );
        assert_eq!(line.len(), 1, "straight cubic → 1 point");
        assert!(
            curve.len() > 8,
            "curved cubic should subdivide many times, got {}",
            curve.len()
        );
        // Endpoints are preserved.
        assert_eq!(curve.last().unwrap().x, 90.0);
    }

    #[test]
    fn polybezier16_paints_along_the_curve() {
        let (w, h) = (120i32, 120i32);
        let mut recs = Vec::new();
        // Thick black pen so the curve leaves visible ink.
        let mut pen = Vec::new();
        pu32(&mut pen, 1); // ihPen
        pu32(&mut pen, 0); // PS_SOLID
        pi32(&mut pen, 3); // width.x
        pi32(&mut pen, 0); // width.y
        pu32(&mut pen, 0x0000_0000); // black
        record(&mut recs, EMR_CREATEPEN, &pen);
        record(&mut recs, EMR_SELECTOBJECT, &select(1));
        // EMR_POLYBEZIER16: rclBounds(16), count(4), points(count × 2×i16).
        let mut body = Vec::new();
        pi32(&mut body, 0);
        pi32(&mut body, 0);
        pi32(&mut body, 119);
        pi32(&mut body, 119);
        pu32(&mut body, 4); // 4 control points = one cubic
        for (x, y) in [(10i16, 100i16), (40, 10), (80, 10), (110, 100)] {
            body.extend_from_slice(&x.to_le_bytes());
            body.extend_from_slice(&y.to_le_bytes());
        }
        record(&mut recs, EMR_POLYBEZIER16, &body);
        record(&mut recs, EMR_EOF, &[0, 0, 0, 0]);
        let m = decode(&emf_with(w, h, &recs)).expect("decode");

        // Some ink exists near the apex of the curve (around y≈25, x≈60).
        let mut found = false;
        'outer: for y in 18..34 {
            for x in 50..72 {
                if px(&m, x, y).3 > 0 {
                    found = true;
                    break 'outer;
                }
            }
        }
        assert!(found, "the flattened Bézier should paint near its apex");
    }

    // ── RGNDATA parser unit ──────────────────────────────────────────────────

    #[test]
    fn parse_emf_rgndata_reads_rect_list() {
        let rd = rgndata(&[(1, 2, 3, 4), (5, 6, 7, 8)]);
        let rects = parse_emf_rgndata(&rd, 0);
        assert_eq!(rects.len(), 2);
        assert_eq!(
            (rects[0].left, rects[0].top, rects[0].right, rects[0].bottom),
            (1.0, 2.0, 3.0, 4.0)
        );
        assert_eq!(
            (rects[1].left, rects[1].top, rects[1].right, rects[1].bottom),
            (5.0, 6.0, 7.0, 8.0)
        );
        // Truncated/empty payloads never panic.
        assert!(parse_emf_rgndata(&[0u8; 8], 0).is_empty());
        assert!(parse_emf_rgndata(&rd, 9_999).is_empty());
        // A bogus huge count is capped to the available bytes.
        let mut bad = rd.clone();
        bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_emf_rgndata(&bad, 0).len(), 2);
    }
}
