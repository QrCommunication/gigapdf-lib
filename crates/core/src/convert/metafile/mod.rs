//! From-scratch **WMF + EMF metafile rasterizer** (GDI records → RGBA8).
//!
//! Windows Metafile (`.wmf`) and Enhanced Metafile (`.emf`) are *vector* image
//! formats: a list of GDI drawing commands (move/line/polygon/ellipse, DIB
//! blits, text, transforms) rather than a pixel grid. Office documents and RTF
//! embed them for diagrams, logos and clip-art. The engine carries no image
//! crate, so this module interprets those commands itself onto an in-memory
//! [`raster::Canvas`] and returns a tight RGBA8 raster the image pipeline can
//! consume (e.g. re-encode to PNG for a [`super::PlacedImage`]).
//!
//! This module is **self-contained**: it owns its rasterizer ([`raster`]) — a
//! transparent RGBA framebuffer with anti-aliased fill/stroke, the GDI graphics
//! state + object table, the logical→device transform (window/viewport + map
//! mode + EMF world transform), and a from-scratch DIB/BMP decoder — plus the
//! two record interpreters ([`wmf`], [`emf`]). Wiring into `office_import` /
//! `rtf` (issues #3/#4) is a separate follow-up; this module only exposes the
//! decoder + its public API.
//!
//! ## Coverage
//! - **Headers**: WMF placeable (`0x9AC6CDD7`, bbox + units) and bare
//!   `METAHEADER`; EMF `ENHMETAHEADER` (`rclBounds` / `rclFrame`).
//! - **Objects**: pens (`CreatePenIndirect` / `ExtCreatePen` — style, width,
//!   colour), brushes (`CreateBrushIndirect` / `CreateSolidBrush` / pattern→solid),
//!   fonts (`CreateFontIndirect[W]`), with `SelectObject` / `DeleteObject` and
//!   EMF stock objects.
//! - **Paths/shapes**: `MoveTo`/`LineTo`, `Polyline`/`Polygon`/`PolyPolygon`
//!   (+ EMF `*16`), `Rectangle`/`RoundRect`/`Ellipse`, `Arc`/`Pie`/`Chord`,
//!   `PolyBezier` (EMF), `SetPixel[V]`, `FillRgn`/`PaintRgn` (region→bbox).
//! - **Blits**: `BitBlt`/`StretchBlt`/`StretchDIBits`/`SetDIBitsToDevice` —
//!   decoding the embedded DIB (1/4/8/24/32 bpp + RLE4/RLE8).
//! - **Text**: `ExtTextOut`/`TextOut` rendered as a reasonable advance/box
//!   strip (text is secondary; no font shaping).
//! - **Transforms**: `SetWindowOrg`/`Ext`, `SetViewportOrg`/`Ext`, `SetMapMode`,
//!   `SetWorldTransform`/`ModifyWorldTransform` (EMF affine); plus the paint
//!   modes `SetBkMode`/`SetBkColor`/`SetTextColor`/`SetROP2`/`SetPolyFillMode`.
//!
//! Genuinely-rare records (palette management, escapes, EMF GDI+ comment blocks,
//! clipping beyond a bounding rectangle, ROP2 raster ops other than copy) are
//! skipped safely.

mod emf;
mod raster;
mod wmf;

/// A decoded metafile as a tight RGBA8 raster at its natural pixel size, with a
/// transparent background where nothing was painted.
#[derive(Debug, Clone)]
pub struct MetafileRaster {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, row-major top-to-bottom, straight alpha.
    pub rgba: Vec<u8>,
}

/// Decode a **WMF** (placeable or standard) buffer to an RGBA raster, or `None`
/// if it isn't a WMF or can't be parsed. Never panics on malformed input.
pub fn decode_wmf(data: &[u8]) -> Option<MetafileRaster> {
    wmf::decode(data)
}

/// Decode an **EMF** buffer to an RGBA raster, or `None` if it isn't an EMF or
/// can't be parsed. Never panics on malformed input.
pub fn decode_emf(data: &[u8]) -> Option<MetafileRaster> {
    emf::decode(data)
}

/// Sniff the magic bytes and dispatch to [`decode_wmf`] or [`decode_emf`].
/// Recognizes the WMF placeable key (`0x9AC6CDD7`), a bare `METAHEADER`
/// (type 1/2, header size 9 words), and the EMF header (`iType == 1` with the
/// `" EMF"` signature). Returns `None` for anything else.
pub fn decode_metafile(data: &[u8]) -> Option<MetafileRaster> {
    if data.len() < 4 {
        return None;
    }
    // WMF placeable.
    if raster::rd_u32(data, 0) == Some(0x9AC6_CDD7) {
        return decode_wmf(data);
    }
    // EMF: record type 1 + " EMF" signature at offset 40.
    if data.len() >= 44
        && raster::rd_u32(data, 0) == Some(1)
        && raster::rd_u32(data, 40) == Some(0x464D_4520)
    {
        return decode_emf(data);
    }
    // Bare WMF METAHEADER: mtType ∈ {1,2}, mtHeaderSize == 9 words.
    if let (Some(t), Some(hs)) = (raster::rd_u16(data, 0), raster::rd_u16(data, 2)) {
        if (t == 1 || t == 2) && hs == 9 {
            return decode_wmf(data);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── tiny WMF builder ─────────────────────────────────────────────────────

    /// Push a u16 (LE).
    fn pu16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    /// Push an i16 (LE).
    fn pi16(v: &mut Vec<u8>, x: i16) {
        v.extend_from_slice(&(x as u16).to_le_bytes());
    }
    /// Push a u32 (LE).
    fn pu32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    /// Push an i32 (LE).
    fn pi32(v: &mut Vec<u8>, x: i32) {
        v.extend_from_slice(&(x as u32).to_le_bytes());
    }

    /// A WMF record: size (in words) = 3 + params.len(); then function + params.
    fn wmf_record(out: &mut Vec<u8>, func: u16, params: &[u16]) {
        let size = 3 + params.len() as u32;
        pu32(out, size);
        pu16(out, func);
        for p in params {
            pu16(out, *p);
        }
    }

    /// Assemble a placeable WMF from records, with the given logical bbox.
    fn placeable_wmf(bbox: (i16, i16, i16, i16), inch: u16, records: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        // APMHEADER
        pu32(&mut v, 0x9AC6_CDD7); // key
        pu16(&mut v, 0); // hmf
        pi16(&mut v, bbox.0);
        pi16(&mut v, bbox.1);
        pi16(&mut v, bbox.2);
        pi16(&mut v, bbox.3);
        pu16(&mut v, inch);
        pu32(&mut v, 0); // reserved
        pu16(&mut v, 0); // checksum
                         // METAHEADER (9 words)
        pu16(&mut v, 1); // mtType = memory
        pu16(&mut v, 9); // mtHeaderSize
        pu16(&mut v, 0x0300); // version
        pu32(&mut v, 0); // size (low/high) — unused by our parser
        pu16(&mut v, 4); // numObjects
        pu32(&mut v, 0); // maxRecord
        pu16(&mut v, 0); // numMembers
        v.extend_from_slice(records);
        v
    }

    /// COLORREF param words for an (r,g,b).
    fn colorref_words(r: u8, g: u8, b: u8) -> [u16; 2] {
        let c = (r as u32) | ((g as u32) << 8) | ((b as u32) << 16);
        [(c & 0xFFFF) as u16, (c >> 16) as u16]
    }

    /// Sample a canvas pixel as (r,g,b,a).
    fn px(m: &MetafileRaster, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * m.width + x) * 4) as usize;
        (m.rgba[i], m.rgba[i + 1], m.rgba[i + 2], m.rgba[i + 3])
    }

    #[test]
    fn placeable_wmf_polygon_and_rectangle_render() {
        // Logical space 0..100 × 0..100, 100 units/inch ⇒ ~96px device.
        let mut recs = Vec::new();
        // SetWindowOrg(0,0), SetWindowExt(100,100)
        wmf_record(&mut recs, 0x020B, &[0, 0]); // org y,x
        wmf_record(&mut recs, 0x020C, &[100, 100]); // ext y,x

        // CreatePenIndirect: style=Solid(0), width.x=1, width.y=0, COLORREF red
        let red = colorref_words(255, 0, 0);
        wmf_record(&mut recs, 0x02FA, &[0, 1, 0, red[0], red[1]]);
        // CreateBrushIndirect: style=Solid(0), COLORREF blue, hatch=0
        let blue = colorref_words(0, 0, 255);
        wmf_record(&mut recs, 0x02FC, &[0, blue[0], blue[1], 0]);
        // SelectObject(0)=pen, SelectObject(1)=brush
        wmf_record(&mut recs, 0x012D, &[0]);
        wmf_record(&mut recs, 0x012D, &[1]);

        // Polygon: 4 points (a blue diamond around 25..50)
        wmf_record(
            &mut recs,
            0x0324,
            &[
                4, // count
                30u16, 10u16, // (30,10)
                50u16, 30u16, // (50,30)
                30u16, 50u16, // (30,50)
                10u16, 30u16, // (10,30)
            ],
        );

        // Create a green brush and select it, then Rectangle in the lower-right.
        let green = colorref_words(0, 200, 0);
        wmf_record(&mut recs, 0x02FC, &[0, green[0], green[1], 0]);
        wmf_record(&mut recs, 0x012D, &[2]); // select green brush (handle 2)
                                             // META_RECTANGLE: bottom, right, top, left
        wmf_record(&mut recs, 0x041B, &[90, 90, 60, 60]);

        // EOF
        wmf_record(&mut recs, 0x0000, &[]);

        let wmf = placeable_wmf((0, 0, 100, 100), 100, &recs);
        let m = decode_wmf(&wmf).expect("decode placeable wmf");
        assert!(
            m.width >= 64 && m.height >= 64,
            "device size {}x{}",
            m.width,
            m.height
        );

        // Diamond centre (logical 30,30) → device. Scale = width/100.
        let sx = m.width as f64 / 100.0;
        let sy = m.height as f64 / 100.0;
        let dia = px(&m, (30.0 * sx) as u32, (30.0 * sy) as u32);
        assert!(dia.3 > 0, "diamond centre should be painted (a={})", dia.3);
        assert!(
            dia.2 > dia.0 && dia.2 > 100,
            "diamond centre should be blue-ish, got {:?}",
            dia
        );

        // Rectangle centre (logical 75,75) → device, green.
        let rect = px(&m, (75.0 * sx) as u32, (75.0 * sy) as u32);
        assert!(rect.3 > 0, "rectangle centre should be painted");
        assert!(
            rect.1 > rect.0 && rect.1 > rect.2,
            "rectangle centre should be green-ish, got {:?}",
            rect
        );

        // A corner far from both shapes stays transparent.
        let corner = px(&m, m.width - 1, 0);
        assert_eq!(
            corner.3, 0,
            "top-right corner should be transparent, got {:?}",
            corner
        );
    }

    // ── tiny EMF builder ─────────────────────────────────────────────────────

    /// An EMF record: iType + nSize(incl. 8-byte header, multiple of 4) + body.
    fn emf_record(out: &mut Vec<u8>, itype: u32, body: &[u8]) {
        let mut body = body.to_vec();
        while !body.len().is_multiple_of(4) {
            body.push(0);
        }
        let nsize = 8 + body.len() as u32;
        pu32(out, itype);
        pu32(out, nsize);
        out.extend_from_slice(&body);
    }

    /// Build an EMF with bounds (0,0)-(w-1,h-1) device units.
    fn emf_with(w: i32, h: i32, records: &[u8]) -> Vec<u8> {
        let mut header = Vec::new();
        // rclBounds (inclusive)
        pi32(&mut header, 0);
        pi32(&mut header, 0);
        pi32(&mut header, w - 1);
        pi32(&mut header, h - 1);
        // rclFrame (.01 mm) — arbitrary but consistent
        pi32(&mut header, 0);
        pi32(&mut header, 0);
        pi32(&mut header, w * 26);
        pi32(&mut header, h * 26);
        pu32(&mut header, 0x464D_4520); // " EMF"
        pu32(&mut header, 0x0001_0000); // version
                                        // nBytes, nRecords, nHandles, sReserved, nDescription, offDescription,
                                        // nPalEntries, szlDevice(8), szlMillimeters(8) — fill to ≥88 bytes total
        pu32(&mut header, 0); // nBytes (unused)
        pu32(&mut header, 0); // nRecords
        pu16(&mut header, 0); // nHandles
        pu16(&mut header, 0); // sReserved
        pu32(&mut header, 0); // nDescription
        pu32(&mut header, 0); // offDescription
        pu32(&mut header, 0); // nPalEntries
        pi32(&mut header, w); // szlDevice.cx
        pi32(&mut header, h); // szlDevice.cy
        pi32(&mut header, w); // szlMillimeters.cx
        pi32(&mut header, h); // szlMillimeters.cy

        // The header record: iType=1, nSize = 8 + header.len().
        let mut v = Vec::new();
        let nsize = 8 + header.len() as u32;
        pu32(&mut v, 1); // EMR_HEADER
        pu32(&mut v, nsize);
        v.extend_from_slice(&header);
        v.extend_from_slice(records);
        v
    }

    #[test]
    fn emf_lineto_and_ellipse_render() {
        let (w, h) = (120i32, 120i32);
        let mut recs = Vec::new();

        // Create a thick red pen via EMR_CREATEPEN (handle 1).
        // LOGPEN { style(4)=Solid, width.x(4)=4, width.y(4)=0, COLORREF(4) }
        let mut pen = Vec::new();
        pu32(&mut pen, 1); // ihPen
        pu32(&mut pen, 0); // PS_SOLID
        pi32(&mut pen, 4); // width.x
        pi32(&mut pen, 0); // width.y
        pu32(&mut pen, 0x0000_00FF); // COLORREF red (0x00bbggrr)
        emf_record(&mut recs, 38, &pen); // EMR_CREATEPEN

        // SelectObject(handle 1)
        let mut sel = Vec::new();
        pu32(&mut sel, 1);
        emf_record(&mut recs, 37, &sel);

        // MoveToEx(10,10); LineTo(110,110) — a red diagonal.
        let mut mv = Vec::new();
        pi32(&mut mv, 10);
        pi32(&mut mv, 10);
        emf_record(&mut recs, 27, &mv); // EMR_MOVETOEX
        let mut ln = Vec::new();
        pi32(&mut ln, 110);
        pi32(&mut ln, 110);
        emf_record(&mut recs, 54, &ln); // EMR_LINETO

        // Create a solid blue brush (handle 2) + select it.
        let mut br = Vec::new();
        pu32(&mut br, 2); // ihBrush
        pu32(&mut br, 0); // BS_SOLID
        pu32(&mut br, 0x00FF_0000); // COLORREF blue
        pu32(&mut br, 0); // hatch
        emf_record(&mut recs, 39, &br); // EMR_CREATEBRUSHINDIRECT
        let mut sel2 = Vec::new();
        pu32(&mut sel2, 2);
        emf_record(&mut recs, 37, &sel2);

        // Ellipse in box (20,20)-(60,60).
        let mut el = Vec::new();
        pi32(&mut el, 20);
        pi32(&mut el, 20);
        pi32(&mut el, 60);
        pi32(&mut el, 60);
        emf_record(&mut recs, 42, &el); // EMR_ELLIPSE

        // EOF
        emf_record(&mut recs, 14, &[0, 0, 0, 0]);

        let emf = emf_with(w, h, &recs);
        let m = decode_emf(&emf).expect("decode emf");
        assert_eq!(m.width, 120);
        assert_eq!(m.height, 120);

        // Diagonal: pixel near (60,60) should be reddish.
        let mut found_red = false;
        for d in -2i32..=2 {
            let p = px(&m, (60 + d) as u32, 60);
            if p.3 > 0 && p.0 > 120 && p.2 < 120 {
                found_red = true;
                break;
            }
        }
        assert!(found_red, "red diagonal should cross near (60,60)");

        // Ellipse centre (40,40) should be blue.
        let c = px(&m, 40, 40);
        assert!(c.3 > 0, "ellipse centre painted (a={})", c.3);
        assert!(
            c.2 > c.0 && c.2 > 100,
            "ellipse centre blue-ish, got {:?}",
            c
        );

        // A clear area stays transparent.
        let clear = px(&m, 110, 10);
        assert_eq!(clear.3, 0, "clear area transparent, got {:?}", clear);
    }

    #[test]
    fn wmf_stretchdibits_blits_dib_pixels() {
        // Build a 2×2 24-bpp DIB: TL red, TR green, BL blue, BR white.
        // BITMAPINFOHEADER (40 bytes) + pixel rows (bottom-up, row-padded to 4).
        let mut dib = Vec::new();
        pu32(&mut dib, 40); // biSize
        pi32(&mut dib, 2); // biWidth
        pi32(&mut dib, 2); // biHeight (bottom-up)
        pu16(&mut dib, 1); // biPlanes
        pu16(&mut dib, 24); // biBitCount
        pu32(&mut dib, 0); // biCompression = BI_RGB
        pu32(&mut dib, 0); // biSizeImage
        pi32(&mut dib, 0);
        pi32(&mut dib, 0);
        pu32(&mut dib, 0);
        pu32(&mut dib, 0);
        // Bottom row first (y=1 = BL, BR), each pixel B,G,R; row padded to 4-byte.
        // Bottom row: BL blue, BR white
        dib.extend_from_slice(&[255, 0, 0]); // blue (B=255)
        dib.extend_from_slice(&[255, 255, 255]); // white
        dib.extend_from_slice(&[0, 0]); // pad row to 8 bytes (6→8)
                                        // Top row: TL red, TR green
        dib.extend_from_slice(&[0, 0, 255]); // red (R=255)
        dib.extend_from_slice(&[0, 255, 0]); // green (G=255)
        dib.extend_from_slice(&[0, 0]); // pad

        // META_STRETCHDIB params (words): rop(2), usage(1), srcH(1), srcW(1),
        // srcY(1), srcX(1), destH(1), destW(1), destY(1), destX(1), then DIB.
        let mut params: Vec<u16> = vec![
            0x0020, // rop low
            0x00CC, // rop high (SRCCOPY)
            0,      // usage
            2,      // srcH (word 3)
            2,      // srcW (word 4)
            0,      // srcY
            0,      // srcX
            40,     // destH (word 7) — logical 40
            40,     // destW (word 8)
            10,     // destY (word 9)
            10,     // destX (word 10)
        ];
        // Append the DIB as u16 words (it's an even number of bytes).
        assert_eq!(dib.len() % 2, 0);
        for chunk in dib.chunks(2) {
            params.push(u16::from_le_bytes([chunk[0], chunk[1]]));
        }

        let mut recs = Vec::new();
        wmf_record(&mut recs, 0x020B, &[0, 0]); // SetWindowOrg
        wmf_record(&mut recs, 0x020C, &[100, 100]); // SetWindowExt 100×100
        wmf_record(&mut recs, 0x0F43, &params); // META_STRETCHDIB
        wmf_record(&mut recs, 0x0000, &[]); // EOF

        let wmf = placeable_wmf((0, 0, 100, 100), 100, &recs);
        let m = decode_wmf(&wmf).expect("decode wmf with dib");

        let sx = m.width as f64 / 100.0;
        let sy = m.height as f64 / 100.0;
        // Dest rect logical (10,10)-(50,50). Sample quadrant centres.
        // Top-left quadrant ~ logical (20,20) → red.
        let tl = px(&m, (20.0 * sx) as u32, (20.0 * sy) as u32);
        assert!(tl.3 > 0, "blit TL painted, got {:?}", tl);
        assert!(
            tl.0 > 200 && tl.1 < 80 && tl.2 < 80,
            "blit TL should be red, got {:?}",
            tl
        );
        // Top-right quadrant ~ logical (40,20) → green.
        let tr = px(&m, (40.0 * sx) as u32, (20.0 * sy) as u32);
        assert!(
            tr.1 > 200 && tr.0 < 80,
            "blit TR should be green, got {:?}",
            tr
        );
        // Bottom-left quadrant ~ logical (20,40) → blue.
        let bl = px(&m, (20.0 * sx) as u32, (40.0 * sy) as u32);
        assert!(
            bl.2 > 200 && bl.0 < 80,
            "blit BL should be blue, got {:?}",
            bl
        );
    }

    #[test]
    fn malformed_inputs_return_none_without_panic() {
        assert!(decode_wmf(&[]).is_none());
        assert!(decode_emf(&[]).is_none());
        assert!(decode_metafile(&[]).is_none());
        assert!(decode_wmf(&[0u8; 3]).is_none());
        assert!(decode_emf(&[0xFF; 10]).is_none());
        // Looks like a placeable header but truncated mid-record.
        let mut v = Vec::new();
        pu32(&mut v, 0x9AC6_CDD7);
        v.extend_from_slice(&[0u8; 8]); // partial header
        assert!(decode_wmf(&v).is_none());
        // Valid placeable + header but a record claiming a huge size.
        let mut recs = Vec::new();
        pu32(&mut recs, 0xFFFF_FFFF); // record size way past EOF
        pu16(&mut recs, 0x0324);
        let wmf = placeable_wmf((0, 0, 50, 50), 100, &recs);
        // Must not panic; returns a (possibly empty) raster or None.
        let _ = decode_wmf(&wmf);
        // EMF header ok but body record truncated.
        let mut bad = emf_with(20, 20, &[]);
        bad.extend_from_slice(&[1, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0x7F]); // nSize huge
        let _ = decode_emf(&bad); // no panic
    }

    #[test]
    fn sniffing_dispatches_correctly() {
        // Placeable WMF.
        let mut recs = Vec::new();
        wmf_record(&mut recs, 0x0000, &[]);
        let wmf = placeable_wmf((0, 0, 10, 10), 100, &recs);
        assert!(decode_metafile(&wmf).is_some(), "sniff placeable wmf");

        // EMF.
        let mut erecs = Vec::new();
        emf_record(&mut erecs, 14, &[0, 0, 0, 0]); // EOF
        let emf = emf_with(10, 10, &erecs);
        assert!(decode_metafile(&emf).is_some(), "sniff emf");

        // Bare standard WMF (no placeable header).
        let mut bare = Vec::new();
        pu16(&mut bare, 1); // mtType
        pu16(&mut bare, 9); // mtHeaderSize
        pu16(&mut bare, 0x0300);
        pu32(&mut bare, 0);
        pu16(&mut bare, 1);
        pu32(&mut bare, 0);
        pu16(&mut bare, 0);
        wmf_record(&mut bare, 0x020C, &[10, 10]); // SetWindowExt
        wmf_record(&mut bare, 0x0000, &[]); // EOF
        assert!(decode_metafile(&bare).is_some(), "sniff bare wmf");

        // Random bytes → None.
        assert!(decode_metafile(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11]).is_none());
    }
}
