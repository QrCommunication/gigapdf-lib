//! JBIG2Decode (ISO 32000-1 §7.4.7 / ITU-T T.88), embedded-in-PDF profile.
//! Pure `std`, zero dependencies.
//!
//! A PDF JBIG2 image is the concatenation of an optional shared globals stream
//! (`/DecodeParms /JBIG2Globals`) and the page stream, each a sequence of
//! **segments**. Every segment has a header (number, flags, referred-to
//! segments, page association, data length) followed by its data. This decoder
//! implements the segment types that cover virtually all real scans:
//!
//! * **page information** (§7.4.8) and **end of page / stripe** markers,
//! * **generic region** (§6.2) — arithmetic (GB templates 0-3 with TPGDON
//!   typical prediction) and **MMR** (reusing the shared CCITT G4 core),
//! * **symbol dictionary** (§6.5) — arithmetic symbol bitmaps with IADH/IADW
//!   height/width class decoding and IAEX export flags,
//! * **text region** (§6.4) — placing dictionary symbols via IADT/IAFS/IADS/
//!   IAIT/IARI/IAID.
//!
//! Regions are composited onto the page bitmap with the segment's external
//! combination operator. The MQ arithmetic decoder and the integer arithmetic
//! decoders (`IAx`, `IAID`) live in the included [`jbig2_mq`](self) tables.
//!
//! **Out-of-scope sub-features** (precisely): generic **refinement** regions
//! (§6.3), **halftone** regions / pattern dictionaries (§6.6/§6.7), and the
//! "transposed / reference-corner / refinement-aggregate" text-region variants
//! are not reached by the common arithmetic generic + symbol-dictionary + text
//! pipeline; an unsupported segment is skipped (its region is left blank) rather
//! than aborting the whole page, so the decodable regions still render.

use crate::error::{EngineError, Result};
use crate::object::Dictionary;

use super::jbig2_mq as mq;
use mq::{ArithContext, IaidContext, IntContext, IntResult, MqDecoder};

/// A bilevel bitmap, one `bool` per pixel (`true` = black / 1), row-major.
#[derive(Clone)]
struct Bitmap {
    width: usize,
    height: usize,
    data: Vec<bool>,
}

impl Bitmap {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![false; width.saturating_mul(height)],
        }
    }

    #[inline]
    fn get(&self, x: i64, y: i64) -> bool {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            return false;
        }
        self.data[y as usize * self.width + x as usize]
    }

    #[inline]
    fn set(&mut self, x: usize, y: usize, v: bool) {
        if x < self.width && y < self.height {
            self.data[y * self.width + x] = v;
        }
    }
}

/// Big-endian bit/byte reader over the segment data area, used for fixed-layout
/// region/segment fields (the arithmetic-coded payload is read by the MQ coder).
struct Reader<'a> {
    data: &'a [u8],
    pos: usize, // byte position
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn u8(&mut self) -> Option<u8> {
        let b = self.data.get(self.pos).copied()?;
        self.pos += 1;
        Some(b)
    }

    fn u16(&mut self) -> Option<u16> {
        let hi = self.u8()? as u16;
        let lo = self.u8()? as u16;
        Some((hi << 8) | lo)
    }

    fn u32(&mut self) -> Option<u32> {
        let a = self.u8()? as u32;
        let b = self.u8()? as u32;
        let c = self.u8()? as u32;
        let d = self.u8()? as u32;
        Some((a << 24) | (b << 16) | (c << 8) | d)
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.pos + n > self.data.len() {
            return None;
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Some(s)
    }
}

/// A parsed segment header (T.88 §7.2).
struct SegmentHeader {
    number: u32,
    seg_type: u8,
    referred_to: Vec<u32>,
    data_length: u32,
}

/// Parse one segment header from `r`, returning it and leaving `r` positioned at
/// the start of the segment's data area. `None` on a malformed/truncated header.
fn parse_segment_header(r: &mut Reader) -> Option<SegmentHeader> {
    let number = r.u32()?;
    let flags = r.u8()?;
    let seg_type = flags & 0x3F;
    let page_assoc_4 = (flags & 0x40) != 0;

    // Referred-to-segment count and retention flags (§7.2.4).
    let rt_byte = *r.data.get(r.pos)?;
    let count_top3 = (rt_byte >> 5) & 0x07;
    let ref_count: u32;
    if count_top3 == 7 {
        // Long form: 29-bit count + retention bit field.
        let long = r.u32()? & 0x1FFF_FFFF;
        ref_count = long;
        // Retention flags: 4 + ceil((count+1)/8)... but we only need to skip
        // them. The field is ceil((ref_count + 1) / 8) bytes after the 4-byte
        // count (the 4-byte count already includes one flag byte's worth in the
        // top bits per spec; we follow the common reader convention).
        let retain_bytes = (ref_count as usize + 8) / 8;
        r.take(retain_bytes)?;
    } else {
        ref_count = count_top3 as u32;
        r.u8()?; // the single short-form retention-flags byte
    }

    // Referred-to segment numbers. Their size depends on this segment's number
    // (§7.2.5): 1 byte if number <= 256, 2 bytes if <= 65536, else 4 bytes.
    let ref_size = if number <= 256 {
        1
    } else if number <= 65536 {
        2
    } else {
        4
    };
    let mut referred_to = Vec::with_capacity(ref_count as usize);
    for _ in 0..ref_count {
        let v = match ref_size {
            1 => r.u8()? as u32,
            2 => r.u16()? as u32,
            _ => r.u32()?,
        };
        referred_to.push(v);
    }

    // Page association: 1 or 4 bytes (§7.2.6).
    if page_assoc_4 {
        r.u32()?;
    } else {
        r.u8()?;
    }

    // Data length (§7.2.7).
    let data_length = r.u32()?;

    Some(SegmentHeader {
        number,
        seg_type,
        referred_to,
        data_length,
    })
}

/// Region segment information field (T.88 §7.4.1): width, height, x, y, combop.
struct RegionInfo {
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    comb_op: u8,
}

fn parse_region_info(r: &mut Reader) -> Option<RegionInfo> {
    let width = r.u32()? as usize;
    let height = r.u32()? as usize;
    let x = r.u32()? as usize;
    let y = r.u32()? as usize;
    let comb_op = r.u8()? & 0x07;
    Some(RegionInfo {
        width,
        height,
        x,
        y,
        comb_op,
    })
}

/// The collected decode state shared across a JBIG2 document's segments.
struct Jbig2State {
    page: Option<Bitmap>,
    page_default_black: bool,
    /// Exported symbols per symbol-dictionary segment number.
    symbol_dicts: std::collections::HashMap<u32, Vec<Bitmap>>,
    /// Decoded pattern dictionaries per pattern-dictionary segment number.
    pattern_dicts: std::collections::HashMap<u32, Vec<Bitmap>>,
    /// Custom Huffman tables per table-segment number.
    custom_tables: std::collections::HashMap<u32, super::jbig2_huffman::HuffTable>,
}

impl Jbig2State {
    fn new() -> Self {
        Self {
            page: None,
            page_default_black: false,
            symbol_dicts: std::collections::HashMap::new(),
            pattern_dicts: std::collections::HashMap::new(),
            custom_tables: std::collections::HashMap::new(),
        }
    }
}

/// Decode a PDF-embedded JBIG2 image (page stream `data` + optional `globals`)
/// into MSB-first packed 1-bpp rows matching the image's pixel grid.
///
/// `parms` is unused beyond globals (already extracted by the caller); the page
/// geometry comes from the JBIG2 page-info segment. Output convention matches the
/// other bilevel filters: `0 = black` (so a default `/Decode [0 1]` DeviceGray
/// image renders black where JBIG2 has a 1/black pixel).
pub fn jbig2_decode(
    data: &[u8],
    globals: Option<&[u8]>,
    _parms: Option<&Dictionary>,
) -> Result<Vec<u8>> {
    let mut state = Jbig2State::new();

    if let Some(g) = globals {
        // Globals carry shared segments (typically symbol dictionaries). Failures
        // there are non-fatal; the page stream may still decode.
        let _ = process_segments(g, &mut state);
    }
    process_segments(data, &mut state)?;

    let page = state
        .page
        .ok_or_else(|| EngineError::Filter("JBIG2 has no page bitmap".to_string()))?;
    Ok(pack_bitmap(&page))
}

/// Walk and decode every segment in one JBIG2 stream (globals or page).
fn process_segments(data: &[u8], state: &mut Jbig2State) -> Result<()> {
    let mut r = Reader::new(data);
    loop {
        if r.remaining() == 0 {
            break;
        }
        let header = match parse_segment_header(&mut r) {
            Some(h) => h,
            None => break,
        };
        // Slice the segment's data area. A data length of 0xFFFFFFFF (unknown
        // length) only occurs for some generic regions; we do not handle that
        // rare case and stop cleanly.
        if header.data_length == 0xFFFF_FFFF {
            break;
        }
        let seg_data = match r.take(header.data_length as usize) {
            Some(s) => s,
            None => break,
        };
        // Decode by type; unsupported types are skipped (region left blank).
        decode_segment(&header, seg_data, state);
    }
    Ok(())
}

/// Dispatch a single segment by its type (T.88 §7.3).
fn decode_segment(header: &SegmentHeader, data: &[u8], state: &mut Jbig2State) {
    match header.seg_type {
        // Symbol dictionary.
        0 => {
            if let Some(symbols) = decode_symbol_dictionary(header, data, state) {
                state.symbol_dicts.insert(header.number, symbols);
            }
        }
        // Intermediate / immediate / immediate-lossless text region.
        4 | 6 | 7 => {
            decode_text_region_segment(header, data, state);
        }
        // Immediate / immediate-lossless generic region.
        36 | 38 | 39 => {
            decode_generic_region_segment(data, state);
        }
        // Intermediate / immediate / immediate-lossless generic refinement region.
        40 | 42 | 43 => {
            decode_refinement_region_segment(data, state);
        }
        // Pattern dictionary.
        16 => {
            if let Some(patterns) = decode_pattern_dictionary(data) {
                state.pattern_dicts.insert(header.number, patterns);
            }
        }
        // Intermediate / immediate / immediate-lossless halftone region.
        20 | 22 | 23 => {
            decode_halftone_region_segment(header, data, state);
        }
        // Custom Huffman table segment (§7.4.13).
        53 => {
            if let Some(table) = super::jbig2_huffman::build_custom_table(data) {
                state.custom_tables.insert(header.number, table);
            }
        }
        // Page information.
        48 => {
            if let Some(p) = decode_page_info(data) {
                state.page_default_black = p.1;
                state.page = Some(p.0);
            }
        }
        // End of page (49), end of stripe (50), end of file (51), profiles (52),
        // tables (53), extension (62): no bitmap effect here.
        _ => {}
    }
}

/// Decode a page-information segment (§7.4.8) into an initialised page bitmap and
/// its default pixel value (true = black default).
fn decode_page_info(data: &[u8]) -> Option<(Bitmap, bool)> {
    let mut r = Reader::new(data);
    let width = r.u32()? as usize;
    let height = r.u32()? as usize;
    let _xres = r.u32()?;
    let _yres = r.u32()?;
    let flags = r.u8()?;
    let _striping = r.u16()?;
    // An unknown/striped height (0xFFFFFFFF) is clamped later by the regions; we
    // need a concrete buffer, so reject absurd geometry.
    if width == 0 || width > 1 << 20 {
        return None;
    }
    let h = if height == 0 || height > (1 << 20) {
        // Height will be grown by stripes; start at 0 and grow on region paint.
        0
    } else {
        height
    };
    let default_black = (flags & 0x04) != 0;
    let mut page = Bitmap::new(width, h);
    if default_black {
        for p in page.data.iter_mut() {
            *p = true;
        }
    }
    Some((page, default_black))
}

/// Ensure the page bitmap is at least `min_height` tall, growing (and filling new
/// rows with the page default) as stripes are painted.
fn ensure_page_height(state: &mut Jbig2State, min_height: usize) {
    if let Some(page) = state.page.as_mut() {
        if page.height < min_height {
            let extra = (min_height - page.height) * page.width;
            page.data
                .extend(std::iter::repeat_n(state.page_default_black, extra));
            page.height = min_height;
        }
    }
}

/// Composite `region` onto the page at `(x, y)` with combination operator `op`
/// (0 OR, 1 AND, 2 XOR, 3 XNOR, 4 REPLACE) — T.88 §8.2.
fn composite(state: &mut Jbig2State, region: &Bitmap, x: usize, y: usize, op: u8) {
    ensure_page_height(state, y + region.height);
    let Some(page) = state.page.as_mut() else {
        return;
    };
    for ry in 0..region.height {
        let py = y + ry;
        if py >= page.height {
            break;
        }
        for rx in 0..region.width {
            let px = x + rx;
            if px >= page.width {
                break;
            }
            let s = region.data[ry * region.width + rx];
            let idx = py * page.width + px;
            let d = page.data[idx];
            page.data[idx] = match op {
                0 => d | s,
                1 => d & s,
                2 => d ^ s,
                3 => !(d ^ s),
                _ => s, // REPLACE
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Generic region (§6.2)
// ---------------------------------------------------------------------------

/// Decode an immediate generic-region segment and composite it onto the page.
fn decode_generic_region_segment(data: &[u8], state: &mut Jbig2State) {
    let mut r = Reader::new(data);
    let Some(info) = parse_region_info(&mut r) else {
        return;
    };
    let Some(flags) = r.u8() else { return };
    let mmr = (flags & 0x01) != 0;
    let template = (flags >> 1) & 0x03;
    let tpgdon = (flags & 0x08) != 0;

    // Adaptive template pixels (AT): 1 pair for templates 1-3, 4 pairs for
    // template 0 (each a signed byte). Only present when not MMR.
    let mut at: [(i8, i8); 4] = [(0, 0); 4];
    if !mmr {
        let pairs = if template == 0 { 4 } else { 1 };
        for slot in at.iter_mut().take(pairs) {
            let ax = r.u8().map(|b| b as i8).unwrap_or(0);
            let ay = r.u8().map(|b| b as i8).unwrap_or(0);
            *slot = (ax, ay);
        }
    }

    if info.width == 0 || info.height == 0 || info.width > (1 << 20) {
        return;
    }
    let bitmap = if mmr {
        let rows =
            crate::filters::ccitt::mmr_decode_bitmap(&r.data[r.pos..], info.width, info.height);
        bitmap_from_rows(&rows, info.width, info.height)
    } else {
        let mut mqd = MqDecoder::new(&r.data[r.pos..]);
        decode_generic_bitmap(
            &mut mqd,
            info.width,
            info.height,
            template,
            tpgdon,
            &at,
            &mut vec![ArithContext::default(); 1 << 16],
        )
    };
    composite(state, &bitmap, info.x, info.y, info.comb_op);
}

/// Build a `Bitmap` from rows of booleans.
fn bitmap_from_rows(rows: &[Vec<bool>], width: usize, height: usize) -> Bitmap {
    let mut bm = Bitmap::new(width, height);
    for (y, row) in rows.iter().enumerate().take(height) {
        for (x, &b) in row.iter().enumerate().take(width) {
            bm.set(x, y, b);
        }
    }
    bm
}

/// Decode an arithmetic generic bitmap (§6.2.5.7) of `width × height` using GB
/// `template` (0-3), optional `tpgdon` typical prediction, adaptive pixels `at`,
/// and the caller-provided context array `cx` (reused across calls so a region's
/// contexts persist as the spec requires).
#[allow(clippy::too_many_arguments)]
fn decode_generic_bitmap(
    mq: &mut MqDecoder,
    width: usize,
    height: usize,
    template: u8,
    tpgdon: bool,
    at: &[(i8, i8); 4],
    cx: &mut [ArithContext],
) -> Bitmap {
    let mut bm = Bitmap::new(width, height);
    // LTP (line typical prediction) state for TPGDON.
    let mut ltp = false;
    for y in 0..height {
        if tpgdon {
            // The "SLTP" context value depends on the template (§6.2.5.7).
            let ctx = match template {
                0 => 0x9B25,
                1 => 0x0795,
                2 => 0x00E5,
                _ => 0x0195,
            };
            let bit = mq.decode(&mut cx[ctx]);
            ltp ^= bit == 1;
            if ltp {
                // Typical line: copy the row above verbatim.
                if y > 0 {
                    for x in 0..width {
                        let v = bm.get(x as i64, y as i64 - 1);
                        bm.set(x, y, v);
                    }
                }
                continue;
            }
        }
        for x in 0..width {
            let ctx = generic_context(&bm, x as i64, y as i64, template, at);
            let pixel = mq.decode(&mut cx[ctx as usize]);
            bm.set(x, y, pixel == 1);
        }
    }
    bm
}

/// Compute the arithmetic-coding context for the generic-region pixel at
/// `(x, y)` for GB `template` with adaptive pixels `at` (§6.2.5.3, Figures 4-7).
fn generic_context(bm: &Bitmap, x: i64, y: i64, template: u8, at: &[(i8, i8); 4]) -> u16 {
    let p = |dx: i64, dy: i64| -> u16 { bm.get(x + dx, y + dy) as u16 };
    let a = |i: usize| -> u16 { bm.get(x + at[i].0 as i64, y + at[i].1 as i64) as u16 };
    match template {
        0 => {
            // 16-pixel template (Figure 4). Bit order per the standard's CONTEXT
            // assembly.
            (p(-1, -2) << 15)
                | (p(0, -2) << 14)
                | (p(1, -2) << 13)
                | (p(-2, -1) << 12)
                | (p(-1, -1) << 11)
                | (p(0, -1) << 10)
                | (p(1, -1) << 9)
                | (p(2, -1) << 8)
                | (a(1) << 7)
                | (a(2) << 6)
                | (a(3) << 5)
                | (p(-4, 0) << 4)
                | (p(-3, 0) << 3)
                | (p(-2, 0) << 2)
                | (p(-1, 0) << 1)
                | a(0)
        }
        1 => {
            // 13-pixel template (Figure 5).
            (p(-1, -2) << 12)
                | (p(0, -2) << 11)
                | (p(1, -2) << 10)
                | (p(2, -2) << 9)
                | (p(-2, -1) << 8)
                | (p(-1, -1) << 7)
                | (p(0, -1) << 6)
                | (p(1, -1) << 5)
                | (p(2, -1) << 4)
                | (p(-3, 0) << 3)
                | (p(-2, 0) << 2)
                | (p(-1, 0) << 1)
                | a(0)
        }
        2 => {
            // 10-pixel template (Figure 6).
            (p(-1, -2) << 9)
                | (p(0, -2) << 8)
                | (p(1, -2) << 7)
                | (p(-2, -1) << 6)
                | (p(-1, -1) << 5)
                | (p(0, -1) << 4)
                | (p(1, -1) << 3)
                | (p(-2, 0) << 2)
                | (p(-1, 0) << 1)
                | a(0)
        }
        _ => {
            // Template 3: 10-pixel, single line of context above (Figure 7).
            (p(-3, -1) << 9)
                | (p(-2, -1) << 8)
                | (p(-1, -1) << 7)
                | (p(0, -1) << 6)
                | (p(1, -1) << 5)
                | (p(-4, 0) << 4)
                | (p(-3, 0) << 3)
                | (p(-2, 0) << 2)
                | (p(-1, 0) << 1)
                | a(0)
        }
    }
}

// ---------------------------------------------------------------------------
// Generic refinement region (§6.3)
// ---------------------------------------------------------------------------

/// Compute the refinement arithmetic-coding context for the pixel at `(x, y)`
/// of the output bitmap `bm`, refining `reference` offset by `(dx, dy)`
/// (§6.3.5.3, Figures 12/14). `template` is GRTEMPLATE (0 or 1); `at` holds the
/// two adaptive pixels used by template 0 (`at[0]` over the coding bitmap,
/// `at[1]` over the reference bitmap). The bit layout matches the reference
/// decoders (jbig2dec / pdf.js) so real streams decode correctly.
#[allow(clippy::too_many_arguments)]
fn refinement_context(
    bm: &Bitmap,
    reference: &Bitmap,
    x: i64,
    y: i64,
    dx: i64,
    dy: i64,
    template: u8,
    at: &[(i8, i8); 2],
) -> u32 {
    let c = |px: i64, py: i64| -> u32 { bm.get(px, py) as u32 };
    let rf = |px: i64, py: i64| -> u32 { reference.get(px - dx, py - dy) as u32 };
    if template == 0 {
        let a0 = bm.get(x + at[0].0 as i64, y + at[0].1 as i64) as u32;
        let a1 = reference.get(x - dx + at[1].0 as i64, y - dy + at[1].1 as i64) as u32;
        c(x - 1, y)
            | (c(x + 1, y - 1) << 1)
            | (c(x, y - 1) << 2)
            | (a0 << 3)
            | (rf(x + 1, y + 1) << 4)
            | (rf(x, y + 1) << 5)
            | (rf(x - 1, y + 1) << 6)
            | (rf(x + 1, y) << 7)
            | (rf(x, y) << 8)
            | (rf(x - 1, y) << 9)
            | (rf(x + 1, y - 1) << 10)
            | (rf(x, y - 1) << 11)
            | (a1 << 12)
    } else {
        c(x - 1, y)
            | (c(x + 1, y - 1) << 1)
            | (c(x, y - 1) << 2)
            | (c(x - 1, y - 1) << 3)
            | (rf(x + 1, y + 1) << 4)
            | (rf(x, y + 1) << 5)
            | (rf(x + 1, y) << 6)
            | (rf(x, y) << 7)
            | (rf(x - 1, y) << 8)
            | (rf(x, y - 1) << 9)
    }
}

/// Decode a refinement bitmap (§6.3.5.7): refine `reference` (offset by
/// `(dx, dy)`) into a fresh `width × height` bitmap using GR `template`, optional
/// `tpgron` typical prediction, adaptive pixels `at`, and the caller-provided
/// context array `cx` (reused across calls so contexts persist as required).
#[allow(clippy::too_many_arguments)]
fn decode_refinement_bitmap(
    mq: &mut MqDecoder,
    width: usize,
    height: usize,
    reference: &Bitmap,
    dx: i64,
    dy: i64,
    template: u8,
    tpgron: bool,
    at: &[(i8, i8); 2],
    cx: &mut [ArithContext],
) -> Bitmap {
    let mut bm = Bitmap::new(width, height);
    // SLTP context value for the TPGRON line-typical bit (§6.3.5.6).
    let ltp_ctx: usize = if template == 0 { 0x0100 } else { 0x0080 };
    let mut ltp = false;
    for y in 0..height as i64 {
        if tpgron {
            let bit = mq.decode(&mut cx[ltp_ctx]);
            ltp ^= bit == 1;
        }
        for x in 0..width as i64 {
            if ltp {
                // Typical prediction: if the 3×3 reference neighbourhood around
                // the (offset) pixel is uniform, copy that value without coding.
                let rx = x - dx;
                let ry = y - dy;
                let s: u32 = (reference.get(rx - 1, ry - 1) as u32)
                    + (reference.get(rx, ry - 1) as u32)
                    + (reference.get(rx + 1, ry - 1) as u32)
                    + (reference.get(rx - 1, ry) as u32)
                    + (reference.get(rx, ry) as u32)
                    + (reference.get(rx + 1, ry) as u32)
                    + (reference.get(rx - 1, ry + 1) as u32)
                    + (reference.get(rx, ry + 1) as u32)
                    + (reference.get(rx + 1, ry + 1) as u32);
                if s == 0 {
                    bm.set(x as usize, y as usize, false);
                    continue;
                } else if s == 9 {
                    bm.set(x as usize, y as usize, true);
                    continue;
                }
            }
            let ctx = refinement_context(&bm, reference, x, y, dx, dy, template, at);
            let pixel = mq.decode(&mut cx[ctx as usize]);
            bm.set(x as usize, y as usize, pixel == 1);
        }
    }
    bm
}

/// Decode a generic-refinement-region segment (§7.4.7, types 40/42/43) and
/// composite the refined result onto the page. The reference bitmap is the
/// current page contents under the region rectangle (the embedded-in-PDF profile
/// refines the page in place).
fn decode_refinement_region_segment(data: &[u8], state: &mut Jbig2State) {
    let mut r = Reader::new(data);
    let Some(info) = parse_region_info(&mut r) else {
        return;
    };
    let Some(flags) = r.u8() else { return };
    let template = flags & 0x01;
    let tpgron = (flags & 0x02) != 0;
    // Adaptive pixels: 2 pairs for template 0, none for template 1.
    let mut at: [(i8, i8); 2] = [(-1, -1), (-1, -1)];
    if template == 0 {
        for slot in at.iter_mut() {
            let ax = r.u8().map(|b| b as i8).unwrap_or(-1);
            let ay = r.u8().map(|b| b as i8).unwrap_or(-1);
            *slot = (ax, ay);
        }
    }
    if info.width == 0 || info.height == 0 || info.width > (1 << 20) {
        return;
    }
    // Extract the reference bitmap from the page under the region rectangle.
    ensure_page_height(state, info.y + info.height);
    let reference = extract_region(state, info.x, info.y, info.width, info.height);
    let mut mqd = MqDecoder::new(&r.data[r.pos..]);
    let refined = decode_refinement_bitmap(
        &mut mqd,
        info.width,
        info.height,
        &reference,
        0,
        0,
        template,
        tpgron,
        &at,
        &mut vec![ArithContext::default(); 1 << 13],
    );
    // The refinement region replaces the page rectangle with the refined result.
    composite(state, &refined, info.x, info.y, 4);
}

/// Copy a `width × height` rectangle of the page bitmap at `(x, y)` into a new
/// bitmap (pixels outside the page read as the page default).
fn extract_region(state: &Jbig2State, x: usize, y: usize, width: usize, height: usize) -> Bitmap {
    let mut bm = Bitmap::new(width, height);
    if let Some(page) = state.page.as_ref() {
        for ry in 0..height {
            for rx in 0..width {
                let v = page.get((x + rx) as i64, (y + ry) as i64);
                bm.set(rx, ry, v);
            }
        }
    } else if state.page_default_black {
        for p in bm.data.iter_mut() {
            *p = true;
        }
    }
    bm
}

// ---------------------------------------------------------------------------
// Pattern dictionary (§6.7) and halftone region (§6.6)
// ---------------------------------------------------------------------------

/// Decode a pattern-dictionary segment (§6.7.5): one collective bitmap of
/// `(GRAYMAX+1)` side-by-side `HDPW × HDPH` cells, generic-decoded (MMR or
/// arithmetic with fixed AT pixels), then sliced into the individual patterns.
fn decode_pattern_dictionary(data: &[u8]) -> Option<Vec<Bitmap>> {
    let mut r = Reader::new(data);
    let flags = r.u8()?;
    let hdmmr = (flags & 0x01) != 0;
    let template = (flags >> 1) & 0x03;
    let hdpw = r.u8()? as usize;
    let hdph = r.u8()? as usize;
    let graymax = r.u32()?;
    if hdpw == 0 || hdph == 0 || graymax > (1 << 16) {
        return None;
    }
    let n_patterns = graymax as usize + 1;
    let coll_w = n_patterns.checked_mul(hdpw)?;
    if coll_w > (1 << 22) || coll_w.checked_mul(hdph)? > (1 << 26) {
        return None;
    }
    // Fixed AT pixels for the collective bitmap (§6.7.5): AT1 = (-HDPW, 0),
    // and the standard generic defaults for the rest (template 0 only).
    let at: [(i8, i8); 4] = [(-(hdpw as i8), 0), (-3, -1), (2, -2), (-2, -2)];
    let collective = if hdmmr {
        let rows = crate::filters::ccitt::mmr_decode_bitmap(&r.data[r.pos..], coll_w, hdph);
        bitmap_from_rows(&rows, coll_w, hdph)
    } else {
        let mut mqd = MqDecoder::new(&r.data[r.pos..]);
        decode_generic_bitmap(
            &mut mqd,
            coll_w,
            hdph,
            template,
            false,
            &at,
            &mut vec![ArithContext::default(); 1 << 16],
        )
    };
    // Slice the collective bitmap into individual patterns.
    let mut patterns = Vec::with_capacity(n_patterns);
    for i in 0..n_patterns {
        let mut p = Bitmap::new(hdpw, hdph);
        let x0 = i * hdpw;
        for y in 0..hdph {
            for x in 0..hdpw {
                p.set(x, y, collective.get((x0 + x) as i64, y as i64));
            }
        }
        patterns.push(p);
    }
    Some(patterns)
}

/// Decode a halftone-region segment (§6.6.5) and composite it onto the page. The
/// grayscale image (§C.5) is decoded as `HBPP` Gray-coded bitplanes; each cell's
/// gray value indexes into the referred-to pattern dictionary, placed on the
/// halftone grid with the region combination operator.
fn decode_halftone_region_segment(header: &SegmentHeader, data: &[u8], state: &mut Jbig2State) {
    let mut r = Reader::new(data);
    let Some(info) = parse_region_info(&mut r) else {
        return;
    };
    let Some(flags) = r.u8() else { return };
    let hmmr = (flags & 0x01) != 0;
    let template = (flags >> 1) & 0x03;
    let henableskip = (flags & 0x08) != 0;
    let hcombop = (flags >> 4) & 0x07;
    let hdefpixel = (flags & 0x80) != 0;
    let Some(hgw) = r.u32().map(|v| v as usize) else {
        return;
    };
    let Some(hgh) = r.u32().map(|v| v as usize) else {
        return;
    };
    let Some(hgx) = r.u32().map(|v| v as i32) else {
        return;
    };
    let Some(hgy) = r.u32().map(|v| v as i32) else {
        return;
    };
    let Some(hrx) = r.u16().map(|v| v as i64) else {
        return;
    };
    let Some(hry) = r.u16().map(|v| v as i64) else {
        return;
    };
    if info.width == 0 || info.height == 0 || info.width > (1 << 20) {
        return;
    }
    if hgw == 0 || hgh == 0 || hgw > (1 << 16) || hgh > (1 << 16) {
        return;
    }

    // Gather the pattern set from the referred-to pattern dictionary.
    let mut patterns: Vec<&Bitmap> = Vec::new();
    for refn in &header.referred_to {
        if let Some(dict) = state.pattern_dicts.get(refn) {
            patterns.extend(dict.iter());
        }
    }
    if patterns.is_empty() {
        return;
    }
    let n_patterns = patterns.len();
    // HBPP = ceil(log2(number of patterns)).
    let hbpp = {
        let mut bits = 0u32;
        while (1usize << bits) < n_patterns {
            bits += 1;
        }
        bits.max(1)
    };

    // The skip bitmap (§6.6.5.1): cells whose placement falls entirely outside
    // the region are skipped when HENABLESKIP is set.
    let skip = if henableskip {
        let mut s = Bitmap::new(hgw, hgh);
        let pw = patterns[0].width as i64;
        let ph = patterns[0].height as i64;
        for m in 0..hgh {
            for n in 0..hgw {
                let x = (hgx as i64 + m as i64 * hry + n as i64 * hrx) >> 8;
                let y = (hgy as i64 + m as i64 * hrx - n as i64 * hry) >> 8;
                if x + pw <= 0 || x >= info.width as i64 || y + ph <= 0 || y >= info.height as i64 {
                    s.set(n, m, true);
                }
            }
        }
        Some(s)
    } else {
        None
    };

    // Decode the grayscale image: HBPP Gray-coded bitplanes (§C.5).
    let gray = decode_grayscale_image(
        &r.data[r.pos..],
        hgw,
        hgh,
        hbpp,
        template,
        hmmr,
        skip.as_ref(),
    );

    // Build the region, initialised to HDEFPIXEL.
    let mut region = Bitmap::new(info.width, info.height);
    if hdefpixel {
        for p in region.data.iter_mut() {
            *p = true;
        }
    }
    // Place each cell's pattern on the grid (§6.6.5.2).
    for m in 0..hgh {
        for n in 0..hgw {
            if let Some(s) = skip.as_ref() {
                if s.get(n as i64, m as i64) {
                    continue;
                }
            }
            let gv = gray[m * hgw + n] as usize;
            let idx = gv.min(n_patterns - 1);
            let pat = patterns[idx];
            let x = (hgx as i64 + m as i64 * hry + n as i64 * hrx) >> 8;
            let y = (hgy as i64 + m as i64 * hrx - n as i64 * hry) >> 8;
            blit_pattern(&mut region, pat, x, y, hcombop);
        }
    }
    composite(state, &region, info.x, info.y, info.comb_op);
}

/// Decode the grayscale image of a halftone region (§C.5): `bitplanes` planes,
/// each a `width × height` generic region, combined as a Gray code into per-cell
/// integer values. Plane `bitplanes-1` (the MSB) is decoded first. Both the
/// arithmetic (one shared MQ decoder) and MMR (one shared, bit-continuous G4
/// bitstream) variants decode all `HBPP` planes.
fn decode_grayscale_image(
    data: &[u8],
    width: usize,
    height: usize,
    bitplanes: u32,
    template: u8,
    mmr: bool,
    skip: Option<&Bitmap>,
) -> Vec<u32> {
    // AT pixels for the bitplane generic regions (§C.5 / §6.2 defaults).
    let at: [(i8, i8); 4] = [
        (if template <= 1 { 3 } else { 2 }, -1),
        (-3, -1),
        (2, -2),
        (-2, -2),
    ];
    let mut planes: Vec<Bitmap> = Vec::with_capacity(bitplanes as usize);
    if mmr {
        // MMR grayscale: the bitplanes form one continuous MMR bitstream shared
        // across planes (§C.5) — the MSB plane first, each subsequent plane
        // resuming from the **same** bit position (there is no byte realignment
        // between planes). The resumable CCITT MMR core decodes one plane and
        // returns the continuing bit offset, so all `HBPP` planes are recovered
        // (not just the first); `skip` cells are masked to 0 after decode.
        let mut bit_pos = 0usize;
        for _ in 0..bitplanes {
            let (rows, next) =
                crate::filters::ccitt::mmr_decode_bitmap_resumable(data, width, height, bit_pos);
            bit_pos = next;
            let mut plane = bitmap_from_rows(&rows, width, height);
            if let Some(s) = skip {
                for y in 0..height {
                    for x in 0..width {
                        if s.get(x as i64, y as i64) {
                            plane.set(x, y, false);
                        }
                    }
                }
            }
            planes.push(plane);
        }
        planes.reverse(); // decoded MSB first → reverse so plane[0] is LSB
    } else {
        // One MQ decoder drives all planes in sequence (shared context array).
        let mut mqd = MqDecoder::new(data);
        let mut cx = vec![ArithContext::default(); 1 << 16];
        for _ in 0..bitplanes {
            let plane = decode_generic_bitmap_skip(
                &mut mqd, width, height, template, false, &at, &mut cx, skip,
            );
            planes.push(plane);
        }
        planes.reverse(); // decoded MSB first → reverse so plane[0] is LSB
    }
    // Gray-code combine: value bit j = plane[j] XOR bit[j+1] (from MSB down).
    let mut values = vec![0u32; width * height];
    for y in 0..height {
        for x in 0..width {
            let mut bit = 0u32;
            let mut value = 0u32;
            for j in (0..bitplanes as usize).rev() {
                let plane_bit = planes[j].get(x as i64, y as i64) as u32;
                bit ^= plane_bit;
                value = (value << 1) | bit;
            }
            values[y * width + x] = value;
        }
    }
    values
}

/// As [`decode_generic_bitmap`], but honours a `skip` bitmap: skipped pixels are
/// forced to 0 without consuming a coded bit (§6.2.5.7 with USESKIP).
#[allow(clippy::too_many_arguments)]
fn decode_generic_bitmap_skip(
    mq: &mut MqDecoder,
    width: usize,
    height: usize,
    template: u8,
    tpgdon: bool,
    at: &[(i8, i8); 4],
    cx: &mut [ArithContext],
    skip: Option<&Bitmap>,
) -> Bitmap {
    let mut bm = Bitmap::new(width, height);
    let mut ltp = false;
    for y in 0..height {
        if tpgdon {
            let ctx = match template {
                0 => 0x9B25,
                1 => 0x0795,
                2 => 0x00E5,
                _ => 0x0195,
            };
            let bit = mq.decode(&mut cx[ctx]);
            ltp ^= bit == 1;
            if ltp {
                if y > 0 {
                    for x in 0..width {
                        let v = bm.get(x as i64, y as i64 - 1);
                        bm.set(x, y, v);
                    }
                }
                continue;
            }
        }
        for x in 0..width {
            if let Some(s) = skip {
                if s.get(x as i64, y as i64) {
                    bm.set(x, y, false);
                    continue;
                }
            }
            let ctx = generic_context(&bm, x as i64, y as i64, template, at);
            let pixel = mq.decode(&mut cx[ctx as usize]);
            bm.set(x, y, pixel == 1);
        }
    }
    bm
}

/// Place pattern `pat` into the halftone `region` at `(x, y)` with combination
/// operator `op`, clipping to the region.
fn blit_pattern(region: &mut Bitmap, pat: &Bitmap, x: i64, y: i64, op: u8) {
    for sy in 0..pat.height {
        for sx in 0..pat.width {
            let px = x + sx as i64;
            let py = y + sy as i64;
            if px < 0 || py < 0 || px >= region.width as i64 || py >= region.height as i64 {
                continue;
            }
            let s = pat.data[sy * pat.width + sx];
            let idx = py as usize * region.width + px as usize;
            let d = region.data[idx];
            region.data[idx] = match op {
                0 => d | s,
                1 => d & s,
                2 => d ^ s,
                3 => !(d ^ s),
                _ => s,
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Symbol dictionary (§6.5)
// ---------------------------------------------------------------------------

/// Decode a symbol-dictionary segment into its exported symbol bitmaps, pulling
/// any input symbols from referred-to dictionaries. Dispatches to the arithmetic
/// or Huffman path; both support generic and refinement/aggregate (REFAGG)
/// symbol coding.
fn decode_symbol_dictionary(
    header: &SegmentHeader,
    data: &[u8],
    state: &Jbig2State,
) -> Option<Vec<Bitmap>> {
    let mut r = Reader::new(data);
    let flags = r.u16()?;
    let sdhuff = (flags & 0x0001) != 0;
    let sdrefagg = (flags >> 1) & 0x0001 != 0;
    let huff_dh_sel = ((flags >> 2) & 0x0003) as u8;
    let huff_dw_sel = ((flags >> 4) & 0x0003) as u8;
    let huff_bmsize_sel = ((flags >> 6) & 0x0001) as u8;
    let huff_agg_sel = ((flags >> 7) & 0x0001) as u8;
    let template = ((flags >> 10) & 0x0003) as u8;
    let rtemplate = ((flags >> 12) & 0x0001) as u8;

    // Generic AT pixels (only when arithmetic-coded; §7.4.3.1.2).
    let mut at: [(i8, i8); 4] = [(0, 0); 4];
    if !sdhuff {
        let pairs = if template == 0 { 4 } else { 1 };
        for slot in at.iter_mut().take(pairs) {
            let ax = r.u8().map(|b| b as i8).unwrap_or(0);
            let ay = r.u8().map(|b| b as i8).unwrap_or(0);
            *slot = (ax, ay);
        }
    }
    // Refinement AT pixels (only when REFAGG with refinement template 0).
    let mut rat: [(i8, i8); 2] = [(-1, -1), (-1, -1)];
    if sdrefagg && rtemplate == 0 {
        for slot in rat.iter_mut() {
            let ax = r.u8().map(|b| b as i8).unwrap_or(-1);
            let ay = r.u8().map(|b| b as i8).unwrap_or(-1);
            *slot = (ax, ay);
        }
    }
    let num_ex = r.u32()?; // SDNUMEXSYMS
    let num_new = r.u32()?; // SDNUMNEWSYMS
    if num_new > (1 << 20) || num_ex > (1 << 20) {
        return None;
    }

    // Input symbols come from referred-to symbol dictionaries (in order).
    let mut input_symbols: Vec<Bitmap> = Vec::new();
    for refn in &header.referred_to {
        if let Some(dict) = state.symbol_dicts.get(refn) {
            input_symbols.extend(dict.iter().cloned());
        }
    }

    let params = SymbolDictParams {
        sdrefagg,
        template,
        rtemplate,
        at,
        rat,
        huff_dh_sel,
        huff_dw_sel,
        huff_bmsize_sel,
        huff_agg_sel,
        num_new,
        num_ex,
    };

    if sdhuff {
        decode_symbol_dictionary_huffman(header, &r.data[r.pos..], state, input_symbols, &params)
    } else {
        decode_symbol_dictionary_arith(&r.data[r.pos..], input_symbols, &params)
    }
}

/// Parsed symbol-dictionary parameters shared by the arithmetic and Huffman
/// decode paths.
struct SymbolDictParams {
    sdrefagg: bool,
    template: u8,
    rtemplate: u8,
    at: [(i8, i8); 4],
    rat: [(i8, i8); 2],
    huff_dh_sel: u8,
    huff_dw_sel: u8,
    huff_bmsize_sel: u8,
    huff_agg_sel: u8,
    num_new: u32,
    num_ex: u32,
}

/// Apply the export-flag run list (§6.5.10) to select exported symbols from the
/// combined input+new set. `next_run` yields successive run lengths (from IAEX
/// arithmetic or the B.1 Huffman table); runs alternate exclude/include.
fn apply_export_flags(
    all: &[Bitmap],
    num_ex: u32,
    mut next_run: impl FnMut() -> Option<i32>,
) -> Vec<Bitmap> {
    let mut exported: Vec<Bitmap> = Vec::with_capacity(num_ex as usize);
    let mut i = 0usize;
    let mut cur_exported = false;
    while i < all.len() && (exported.len() as u32) < num_ex {
        let run = match next_run() {
            Some(v) if v >= 0 => v as usize,
            _ => break,
        };
        if cur_exported {
            for sym in all.iter().skip(i).take(run) {
                exported.push(sym.clone());
            }
        }
        i += run;
        cur_exported = !cur_exported;
        if run == 0 && i >= all.len() {
            break;
        }
    }
    // Degenerate run-list fallback: export the trailing `num_ex` symbols.
    if exported.is_empty() && !all.is_empty() {
        let start = all.len().saturating_sub(num_ex as usize);
        exported = all[start..].to_vec();
    }
    exported
}

/// The arithmetic symbol-dictionary decode (§6.5): height classes of generic or
/// refinement/aggregate-coded symbols, then the IAEX export run list.
fn decode_symbol_dictionary_arith(
    data: &[u8],
    input_symbols: Vec<Bitmap>,
    params: &SymbolDictParams,
) -> Option<Vec<Bitmap>> {
    let num_new = params.num_new;
    let num_ex = params.num_ex;
    let mut mqd = MqDecoder::new(data);
    let mut iadh = IntContext::default();
    let mut iadw = IntContext::default();
    let mut iaex = IntContext::default();
    let mut iaai = IntContext::default();
    let mut iardx = IntContext::default();
    let mut iardy = IntContext::default();
    let mut iadt = IntContext::default();
    let mut iafs = IntContext::default();
    let mut iads = IntContext::default();
    let mut iait = IntContext::default();
    let mut iari = IntContext::default();
    let mut gb_cx = vec![ArithContext::default(); 1 << 16];
    let mut gr_cx = vec![ArithContext::default(); 1 << 13];

    // Symbol-ID code length for REFAGG references (over input + all new symbols).
    let total_syms = input_symbols.len() + num_new as usize;
    let code_len = ceil_log2(total_syms.max(1)).max(1);
    let mut iaid = IaidContext::new(code_len);

    let mut new_symbols: Vec<Bitmap> = Vec::with_capacity(num_new as usize);
    let mut hc_height: i64 = 0;
    while (new_symbols.len() as u32) < num_new {
        let dh = match iadh.decode(&mut mqd) {
            IntResult::Value(v) => v as i64,
            IntResult::Oob => break,
        };
        hc_height += dh;
        if hc_height <= 0 || hc_height > (1 << 20) {
            return None;
        }
        let mut sym_width: i64 = 0;
        loop {
            match iadw.decode(&mut mqd) {
                IntResult::Oob => break, // end of this height class
                IntResult::Value(dw) => {
                    sym_width += dw as i64;
                    if sym_width <= 0 || sym_width > (1 << 20) {
                        return None;
                    }
                    if (new_symbols.len() as u32) >= num_new {
                        break;
                    }
                    let bm = if params.sdrefagg {
                        // Refinement/aggregate-coded symbol (§6.5.8.2).
                        decode_refagg_symbol(
                            &mut mqd,
                            sym_width as usize,
                            hc_height as usize,
                            &input_symbols,
                            &new_symbols,
                            params,
                            code_len,
                            &mut iaai,
                            &mut iaid,
                            &mut iardx,
                            &mut iardy,
                            &mut iadt,
                            &mut iafs,
                            &mut iads,
                            &mut iait,
                            &mut iari,
                            &mut gb_cx,
                            &mut gr_cx,
                        )
                    } else {
                        // Plain generic-coded symbol bitmap.
                        decode_generic_bitmap(
                            &mut mqd,
                            sym_width as usize,
                            hc_height as usize,
                            params.template,
                            false,
                            &params.at,
                            &mut gb_cx,
                        )
                    };
                    new_symbols.push(bm);
                }
            }
        }
    }

    let all: Vec<Bitmap> = input_symbols.into_iter().chain(new_symbols).collect();
    let exported = apply_export_flags(&all, num_ex, || match iaex.decode(&mut mqd) {
        IntResult::Value(v) => Some(v),
        IntResult::Oob => None,
    });
    Some(exported)
}

/// Decode one refinement/aggregate-coded symbol (§6.5.8.2). With a single
/// instance (the common case) the symbol is a refinement of one referenced
/// symbol; with several it is an aggregate text region over the symbols decoded
/// so far. `symbols_so_far` = input ++ new (new being those already decoded).
#[allow(clippy::too_many_arguments)]
fn decode_refagg_symbol(
    mq: &mut MqDecoder,
    width: usize,
    height: usize,
    input_symbols: &[Bitmap],
    new_symbols: &[Bitmap],
    params: &SymbolDictParams,
    code_len: u32,
    iaai: &mut IntContext,
    iaid: &mut IaidContext,
    iardx: &mut IntContext,
    iardy: &mut IntContext,
    iadt: &mut IntContext,
    iafs: &mut IntContext,
    iads: &mut IntContext,
    iait: &mut IntContext,
    iari: &mut IntContext,
    gb_cx: &mut [ArithContext],
    gr_cx: &mut [ArithContext],
) -> Bitmap {
    let ninst = match iaai.decode(mq) {
        IntResult::Value(v) if v > 0 => v as usize,
        _ => 1,
    };
    // The reference symbol set is input ++ new-so-far.
    let refs: Vec<&Bitmap> = input_symbols.iter().chain(new_symbols.iter()).collect();
    if ninst == 1 {
        // Single-symbol refinement (§6.5.8.2.2).
        let id = iaid.decode(mq) as usize;
        let rdx = match iardx.decode(mq) {
            IntResult::Value(v) => v as i64,
            IntResult::Oob => 0,
        };
        let rdy = match iardy.decode(mq) {
            IntResult::Value(v) => v as i64,
            IntResult::Oob => 0,
        };
        let blank = Bitmap::new(width, height);
        let reference = refs.get(id).copied().unwrap_or(&blank);
        decode_refinement_bitmap(
            mq,
            width,
            height,
            reference,
            rdx,
            rdy,
            params.rtemplate,
            false,
            &params.rat,
            gr_cx,
        )
    } else {
        // Aggregate: a text region over the reference symbols (§6.5.8.2.1).
        let tparams = TextRegionParams {
            width,
            height,
            num_instances: ninst as u32,
            strips: 1,
            ref_corner: 1, // TOPLEFT
            transposed: false,
            comb_op: 0,
            ds_offset: 0,
            refine: true,
            rtemplate: params.rtemplate,
            rat: params.rat,
            log_strips: 0,
        };
        decode_text_region_arith(
            mq, &refs, &tparams, code_len, iadt, iafs, iads, iait, iari, iaid, gb_cx, gr_cx,
        )
    }
}

/// Decode one refinement/aggregate-coded symbol of a **Huffman** symbol
/// dictionary (§6.5.8.2 with SDHUFF = 1) — the Huffman counterpart of
/// [`decode_refagg_symbol`]. `r` is the shared Huffman bitstream; `agg_table` is
/// SDHUFFAGGINST (REFAGGNINST); `rdx`/`rdy`/`rsize` are the standard refinement
/// tables (B.15/B.15/B.1). The refinement bitmap itself is MQ-arithmetic-coded,
/// byte-aligned within the stream (RSIZE/BMSIZE bytes), exactly as in the Huffman
/// text-region refinement path.
#[allow(clippy::too_many_arguments)]
fn decode_refagg_symbol_huffman(
    r: &mut BitReader,
    width: usize,
    height: usize,
    input_symbols: &[Bitmap],
    new_symbols: &[Bitmap],
    params: &SymbolDictParams,
    sym_code_len: u32,
    agg_table: &HuffTableRef,
    rdx_table: &HuffTable,
    rdy_table: &HuffTable,
    rsize_table: &HuffTable,
    gr_cx: &mut [ArithContext],
) -> Option<Bitmap> {
    let ninst = match agg_table.decode(r)? {
        HuffResult::Value(v) if v > 0 => v as usize,
        _ => 1,
    };
    // The reference symbol set is input ++ new-so-far.
    let refs: Vec<&Bitmap> = input_symbols.iter().chain(new_symbols.iter()).collect();

    if ninst == 1 {
        // Single-symbol refinement (§6.5.8.2.2, Huffman variant): the symbol-ID is
        // a fixed-length code (SBSYMCODELEN bits), then RDX/RDY (B.15) and BMSIZE
        // (B.1); the refinement region is MQ-coded and byte-aligned.
        let id = r.bits(sym_code_len)? as usize;
        let rdx = match rdx_table.decode(r)? {
            HuffResult::Value(v) => v as i64,
            HuffResult::Oob => 0,
        };
        let rdy = match rdy_table.decode(r)? {
            HuffResult::Value(v) => v as i64,
            HuffResult::Oob => 0,
        };
        let bmsize = match rsize_table.decode(r)? {
            HuffResult::Value(v) if v >= 0 => v as usize,
            _ => 0,
        };
        r.byte_align();
        let blank = Bitmap::new(width, height);
        let reference = refs.get(id).copied().unwrap_or(&blank);
        let start = r.byte_pos();
        let mut mqd = MqDecoder::new(&r.data()[start..]);
        let refined = decode_refinement_bitmap(
            &mut mqd,
            width,
            height,
            reference,
            rdx,
            rdy,
            params.rtemplate,
            false,
            &params.rat,
            gr_cx,
        );
        // Skip the consumed refinement bytes (BMSIZE) when provided so the next
        // symbol's Huffman codes start at the right place.
        if bmsize > 0 {
            for _ in 0..(bmsize * 8) {
                let _ = r.bit();
            }
        }
        Some(refined)
    } else {
        // Aggregate (§6.5.8.2.1, Huffman variant): a Huffman text region over the
        // reference symbols renders the new symbol. Standard tables: FS=B.6,
        // DS=B.8, DT=B.11, RDW/RDH/RDX/RDY=B.15, RSIZE=B.1; the symbol-ID table is
        // the run-code-built table read from the shared stream.
        let fs_table = huff::standard_table(6)?;
        let ds_table = huff::standard_table(8)?;
        let dt_table = huff::standard_table(11)?;
        let rdw_table = huff::standard_table(15)?;
        let rdh_table = huff::standard_table(15)?;
        let id_table = huff::build_symbol_id_table(r, refs.len())?;
        let tparams = TextRegionParams {
            width,
            height,
            num_instances: ninst as u32,
            strips: 1,
            ref_corner: 1, // TOPLEFT
            transposed: false,
            comb_op: 0,
            ds_offset: 0,
            refine: true,
            rtemplate: params.rtemplate,
            rat: params.rat,
            log_strips: 0,
        };
        Some(decode_aggregate_text_region_huffman(
            r,
            &refs,
            &tparams,
            &id_table,
            &fs_table,
            &ds_table,
            &dt_table,
            &rdw_table,
            &rdh_table,
            rdx_table,
            rdy_table,
            rsize_table,
            gr_cx,
        ))
    }
}

/// Render an aggregate text region (§6.4 / §6.5.8.2.1) from a **Huffman**
/// bitstream `r` already positioned at the strip data, using the supplied
/// standard tables. Mirrors the placement loop of [`decode_text_region_huffman`]
/// but operates on the shared symbol-dictionary reader (no per-segment table
/// selection). Per-symbol refinement is always honoured (REFINE = 1 in the
/// aggregate case).
#[allow(clippy::too_many_arguments)]
fn decode_aggregate_text_region_huffman(
    r: &mut BitReader,
    symbols: &[&Bitmap],
    params: &TextRegionParams,
    id_table: &HuffTable,
    fs_table: &HuffTable,
    ds_table: &HuffTable,
    dt_table: &HuffTable,
    rdw_table: &HuffTable,
    rdh_table: &HuffTable,
    rdx_table: &HuffTable,
    rdy_table: &HuffTable,
    rsize_table: &HuffTable,
    gr_cx: &mut [ArithContext],
) -> Bitmap {
    let strips = params.strips.max(1) as i64;
    let mut region = Bitmap::new(params.width, params.height);
    let num_instances = params.num_instances;

    let dec = |t: &HuffTable, r: &mut BitReader| -> Option<i64> {
        match t.decode(r)? {
            HuffResult::Value(v) => Some(v as i64),
            HuffResult::Oob => None,
        }
    };

    // Initial STRIPT.
    let mut stript: i64 = dec(dt_table, r).map(|v| -v * strips).unwrap_or(0);
    let mut first_s: i64 = 0;
    let mut inst: u32 = 0;
    let mut guard = 0usize;
    let guard_max = (num_instances as usize + 64) * 4 + 1024;

    while inst < num_instances {
        guard += 1;
        if guard > guard_max {
            break;
        }
        let Some(dt) = dec(dt_table, r) else { break };
        stript += dt * strips;
        let Some(dfs) = dec(fs_table, r) else { break };
        first_s += dfs;
        let mut cur_s = first_s;
        let mut first_in_strip = true;

        loop {
            if inst >= num_instances {
                break;
            }
            if !first_in_strip {
                match ds_table.decode(r) {
                    Some(HuffResult::Value(ids)) => {
                        cur_s += ids as i64 + params.ds_offset as i64;
                    }
                    _ => break, // OOB / underflow ends the strip
                }
            }
            first_in_strip = false;

            let curt = if strips == 1 {
                0
            } else {
                match r.bits(params.log_strips) {
                    Some(v) => v as i64,
                    None => break,
                }
            };
            let t = stript + curt;

            let id = match id_table.decode(r) {
                Some(HuffResult::Value(v)) if v >= 0 => v as usize,
                _ => break,
            };
            guard += 1;
            if guard > guard_max {
                break;
            }
            let Some(&sym) = symbols.get(id) else {
                inst += 1;
                continue;
            };

            // Per-symbol refinement (§6.4.11) — Huffman variant.
            let refined_owned: Option<Bitmap> = if params.refine {
                let ri = match r.bit() {
                    Some(b) => b as i32,
                    None => break,
                };
                if ri != 0 {
                    let rdw = dec(rdw_table, r).unwrap_or(0);
                    let rdh = dec(rdh_table, r).unwrap_or(0);
                    let rdx = dec(rdx_table, r).unwrap_or(0);
                    let rdy = dec(rdy_table, r).unwrap_or(0);
                    let rsize = match rsize_table.decode(r) {
                        Some(HuffResult::Value(v)) if v >= 0 => v as usize,
                        _ => 0,
                    };
                    r.byte_align();
                    let nw = (sym.width as i64 + rdw).max(1) as usize;
                    let nh = (sym.height as i64 + rdh).max(1) as usize;
                    let gdx = (rdw >> 1) + rdx;
                    let gdy = (rdh >> 1) + rdy;
                    let start = r.byte_pos();
                    let mut mqd = MqDecoder::new(&r.data()[start..]);
                    let refined = decode_refinement_bitmap(
                        &mut mqd,
                        nw,
                        nh,
                        sym,
                        gdx,
                        gdy,
                        params.rtemplate,
                        false,
                        &params.rat,
                        gr_cx,
                    );
                    if rsize > 0 {
                        for _ in 0..(rsize * 8) {
                            let _ = r.bit();
                        }
                    }
                    Some(refined)
                } else {
                    None
                }
            } else {
                None
            };

            let place = refined_owned.as_ref().unwrap_or(sym);
            place_symbol(
                &mut region,
                place,
                cur_s,
                t,
                params.ref_corner,
                params.transposed,
                params.comb_op,
            );
            let adv = if params.transposed {
                place.height as i64
            } else {
                place.width as i64
            };
            cur_s += adv.saturating_sub(1);
            inst += 1;
        }
    }
    region
}

/// `ceil(log2(n))` for `n >= 1` (the JBIG2 symbol-ID / gray code length helper).
fn ceil_log2(n: usize) -> u32 {
    let mut bits = 0u32;
    while (1usize << bits) < n {
        bits += 1;
    }
    bits
}

// ---------------------------------------------------------------------------
// Huffman symbol dictionary (§6.5.8.1 / §6.5.9, Annex B)
// ---------------------------------------------------------------------------

use super::jbig2_huffman::{self as huff, BitReader, HuffResult, HuffTable};

/// Resolve the referred-to custom Huffman tables of a segment, in reference
/// order (only table segments that were decoded into `state.custom_tables`).
fn referred_custom_tables<'a>(header: &SegmentHeader, state: &'a Jbig2State) -> Vec<&'a HuffTable> {
    header
        .referred_to
        .iter()
        .filter_map(|n| state.custom_tables.get(n))
        .collect()
}

/// Pick a Huffman table for a two-way selector: `sel == 0` → standard table
/// `std_a`, `sel == 1` → standard table `std_b`, `sel == 3` → the next custom
/// table from `customs` (advancing `cidx`). Falls back to `std_a` if a custom
/// table is unavailable.
fn select_table<'a>(
    sel: u8,
    std_a: u8,
    std_b: u8,
    customs: &[&'a HuffTable],
    cidx: &mut usize,
) -> Option<HuffTableRef<'a>> {
    match sel {
        0 => huff::standard_table(std_a).map(HuffTableRef::Owned),
        1 => huff::standard_table(std_b).map(HuffTableRef::Owned),
        3 => {
            let t = customs.get(*cidx).copied();
            *cidx += 1;
            t.map(HuffTableRef::Borrowed)
                .or_else(|| huff::standard_table(std_a).map(HuffTableRef::Owned))
        }
        _ => huff::standard_table(std_a).map(HuffTableRef::Owned),
    }
}

/// Either an owned standard table or a borrowed custom table; both expose
/// `decode`.
enum HuffTableRef<'a> {
    Owned(HuffTable),
    Borrowed(&'a HuffTable),
}

impl HuffTableRef<'_> {
    fn decode(&self, r: &mut BitReader) -> Option<HuffResult> {
        match self {
            HuffTableRef::Owned(t) => t.decode(r),
            HuffTableRef::Borrowed(t) => t.decode(r),
        }
    }
}

/// The Huffman symbol-dictionary decode path (§6.5.8.1 / §6.5.9). Height-class
/// symbols are read as a collective bitmap (uncompressed or MMR), split by their
/// Huffman-decoded widths; export flags use the standard table B.1. When SDREFAGG
/// is set each symbol is instead refinement/aggregate-coded individually
/// (§6.5.8.2) via [`decode_refagg_symbol_huffman`].
fn decode_symbol_dictionary_huffman(
    header: &SegmentHeader,
    data: &[u8],
    state: &Jbig2State,
    input_symbols: Vec<Bitmap>,
    params: &SymbolDictParams,
) -> Option<Vec<Bitmap>> {
    let customs = referred_custom_tables(header, state);
    let mut cidx = 0usize;
    let dh_table = select_table(params.huff_dh_sel, 4, 5, &customs, &mut cidx)?;
    let dw_table = select_table(params.huff_dw_sel, 2, 3, &customs, &mut cidx)?;
    let bmsize_table = select_table(params.huff_bmsize_sel, 1, 1, &customs, &mut cidx)?;
    // SDHUFFAGGINST selects the REFAGGNINST table (used only when SDREFAGG); still
    // advance the custom index if it selects a custom table to keep alignment.
    let agg_table = select_table(params.huff_agg_sel, 1, 1, &customs, &mut cidx)?;

    // For the SDREFAGG single-symbol refinement / aggregate decode (§6.5.8.2 with
    // SDHUFF), the refinement deltas and bitmap size use the *standard* tables
    // (B.15 for RDX/RDY, B.1 for RSIZE) — the SD flags carry no selectors for
    // them — and the symbol-ID code length is fixed at ceil(log2(total syms)).
    let total_syms = input_symbols.len() + params.num_new as usize;
    let sym_code_len = ceil_log2(total_syms.max(1)).max(1);
    let rdx_table = huff::standard_table(15)?;
    let rdy_table = huff::standard_table(15)?;
    let rsize_table = huff::standard_table(1)?;

    let num_new = params.num_new;
    let num_ex = params.num_ex;
    let mut r = BitReader::new(data);
    let mut new_symbols: Vec<Bitmap> = Vec::with_capacity(num_new as usize);
    // Refinement contexts persist across all REFAGG symbols (§6.5.8.2.2).
    let mut gr_cx = vec![ArithContext::default(); 1 << 13];
    let mut hc_height: i64 = 0;

    while (new_symbols.len() as u32) < num_new {
        let dh = match dh_table.decode(&mut r)? {
            HuffResult::Value(v) => v as i64,
            HuffResult::Oob => break,
        };
        hc_height += dh;
        if hc_height <= 0 || hc_height > (1 << 20) {
            return None;
        }

        if params.sdrefagg {
            // REFAGG height class (§6.5.8.2): each symbol is refinement/aggregate
            // coded individually. Read its DW (OOB ends the class), then decode
            // its bitmap directly (no collective bitmap, no BMSIZE-per-class).
            let mut sym_width: i64 = 0;
            loop {
                match dw_table.decode(&mut r)? {
                    HuffResult::Oob => break,
                    HuffResult::Value(dw) => {
                        sym_width += dw as i64;
                        if sym_width <= 0 || sym_width > (1 << 20) {
                            return None;
                        }
                        if (new_symbols.len() as u32) >= num_new {
                            break;
                        }
                        let bm = decode_refagg_symbol_huffman(
                            &mut r,
                            sym_width as usize,
                            hc_height as usize,
                            &input_symbols,
                            &new_symbols,
                            params,
                            sym_code_len,
                            &agg_table,
                            &rdx_table,
                            &rdy_table,
                            &rsize_table,
                            &mut gr_cx,
                        )?;
                        new_symbols.push(bm);
                    }
                }
            }
            continue;
        }

        // Collect this height class's symbol widths.
        let mut widths: Vec<usize> = Vec::new();
        let mut sym_width: i64 = 0;
        let mut totwidth: i64 = 0;
        loop {
            match dw_table.decode(&mut r)? {
                HuffResult::Oob => break,
                HuffResult::Value(dw) => {
                    sym_width += dw as i64;
                    if sym_width <= 0 || sym_width > (1 << 20) {
                        return None;
                    }
                    if (new_symbols.len() + widths.len()) as u32 >= num_new {
                        break;
                    }
                    widths.push(sym_width as usize);
                    totwidth += sym_width;
                }
            }
        }
        if widths.is_empty() {
            continue;
        }
        if totwidth <= 0 || totwidth > (1 << 22) {
            return None;
        }
        // BMSIZE then the height-class collective bitmap (§6.5.9).
        let bmsize = match bmsize_table.decode(&mut r)? {
            HuffResult::Value(v) if v >= 0 => v as usize,
            _ => 0,
        };
        r.byte_align();
        let coll_w = totwidth as usize;
        let coll_h = hc_height as usize;
        let collective = if bmsize == 0 {
            // Uncompressed: packed MSB-first rows directly in the bitstream.
            read_uncompressed_bitmap(&mut r, coll_w, coll_h)?
        } else {
            // MMR-coded collective bitmap of exactly `bmsize` bytes.
            let start = r.byte_pos();
            let end = (start + bmsize).min(r.data().len());
            let rows =
                crate::filters::ccitt::mmr_decode_bitmap(&r.data()[start..end], coll_w, coll_h);
            // Advance the reader past the MMR bytes.
            for _ in 0..(bmsize * 8) {
                let _ = r.bit();
            }
            bitmap_from_rows(&rows, coll_w, coll_h)
        };
        // Split the collective bitmap into per-symbol bitmaps by width.
        let mut x0 = 0usize;
        for &w in &widths {
            let mut sym = Bitmap::new(w, coll_h);
            for y in 0..coll_h {
                for x in 0..w {
                    sym.set(x, y, collective.get((x0 + x) as i64, y as i64));
                }
            }
            new_symbols.push(sym);
            x0 += w;
        }
    }

    // Export flags via standard table B.1.
    let ex_table = huff::standard_table(1)?;
    let all: Vec<Bitmap> = input_symbols.into_iter().chain(new_symbols).collect();
    let exported = apply_export_flags(&all, num_ex, || match ex_table.decode(&mut r) {
        Some(HuffResult::Value(v)) => Some(v),
        _ => None,
    });
    Some(exported)
}

/// Read an uncompressed bilevel bitmap of `width × height` from `r` as MSB-first
/// packed rows, each row padded to a byte boundary (§6.5.9, BMSIZE == 0).
fn read_uncompressed_bitmap(r: &mut BitReader, width: usize, height: usize) -> Option<Bitmap> {
    let mut bm = Bitmap::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let bit = r.bit()?;
            bm.set(x, y, bit == 1);
        }
        r.byte_align();
    }
    Some(bm)
}

// ---------------------------------------------------------------------------
// Text region (§6.4)
// ---------------------------------------------------------------------------

/// Geometry / coding parameters of a text region, shared by the segment decoder
/// and the symbol-dictionary aggregate (REFAGG) path.
struct TextRegionParams {
    width: usize,
    height: usize,
    num_instances: u32,
    strips: u32,
    log_strips: u32,
    ref_corner: u8,
    transposed: bool,
    comb_op: u8,
    ds_offset: i32,
    refine: bool,
    rtemplate: u8,
    rat: [(i8, i8); 2],
}

/// Decode an immediate text-region segment and composite it onto the page.
/// Dispatches to the arithmetic or Huffman text-region core.
fn decode_text_region_segment(header: &SegmentHeader, data: &[u8], state: &mut Jbig2State) {
    let mut r = Reader::new(data);
    let Some(info) = parse_region_info(&mut r) else {
        return;
    };
    let Some(flags) = r.u16() else { return };
    let sbhuff = flags & 0x0001 != 0;
    let refine = (flags >> 1) & 0x0001 != 0;
    let log_strips = ((flags >> 2) & 0x0003) as u32;
    let strips = 1u32 << log_strips;
    let ref_corner = ((flags >> 4) & 0x0003) as u8;
    let transposed = (flags >> 6) & 0x0001 != 0;
    let comb_op_sym = ((flags >> 7) & 0x0003) as u8;
    let _def_pixel = (flags >> 9) & 0x0001;
    let ds_offset_raw = (flags >> 10) & 0x001F;
    // sign-extend 5-bit DS offset
    let ds_offset = if ds_offset_raw & 0x10 != 0 {
        (ds_offset_raw as i32) - 32
    } else {
        ds_offset_raw as i32
    };

    // Huffman flags halfword (§7.4.3.1.2), present only when SBHUFF.
    let huff_flags = if sbhuff {
        match r.u16() {
            Some(v) => v,
            None => return,
        }
    } else {
        0
    };
    // Refinement AT pixels follow when SBREFINE with refinement template 0.
    let mut rat: [(i8, i8); 2] = [(-1, -1), (-1, -1)];
    let rtemplate = ((flags >> 15) & 0x0001) as u8;
    if refine && rtemplate == 0 {
        for slot in rat.iter_mut() {
            let ax = r.u8().map(|b| b as i8).unwrap_or(-1);
            let ay = r.u8().map(|b| b as i8).unwrap_or(-1);
            *slot = (ax, ay);
        }
    }

    let num_instances = match r.u32() {
        Some(v) => v,
        None => return,
    };
    if num_instances > (1 << 22) {
        return;
    }
    if info.width == 0 || info.height == 0 || info.width > (1 << 20) {
        return;
    }

    // Gather the symbol set from referred-to symbol dictionaries (in order).
    let mut symbols: Vec<&Bitmap> = Vec::new();
    for refn in &header.referred_to {
        if let Some(dict) = state.symbol_dicts.get(refn) {
            symbols.extend(dict.iter());
        }
    }
    if symbols.is_empty() {
        return;
    }
    let code_len = ceil_log2(symbols.len()).max(1);

    let params = TextRegionParams {
        width: info.width,
        height: info.height,
        num_instances,
        strips,
        log_strips,
        ref_corner,
        transposed,
        comb_op: comb_op_sym,
        ds_offset,
        refine,
        rtemplate,
        rat,
    };

    let region = if sbhuff {
        // Huffman-coded text region (§6.4 with Annex B tables).
        match decode_text_region_huffman(&r.data[r.pos..], &symbols, &params, huff_flags, state) {
            Some(bm) => bm,
            None => return,
        }
    } else {
        // Arithmetic-coded text region.
        let mut mqd = MqDecoder::new(&r.data[r.pos..]);
        let mut iadt = IntContext::default();
        let mut iafs = IntContext::default();
        let mut iads = IntContext::default();
        let mut iait = IntContext::default();
        let mut iari = IntContext::default();
        let mut iaid = IaidContext::new(code_len);
        let mut gb_cx = vec![ArithContext::default(); 1 << 16];
        let mut gr_cx = vec![ArithContext::default(); 1 << 13];
        decode_text_region_arith(
            &mut mqd, &symbols, &params, code_len, &mut iadt, &mut iafs, &mut iads, &mut iait,
            &mut iari, &mut iaid, &mut gb_cx, &mut gr_cx,
        )
    };

    composite(state, &region, info.x, info.y, info.comb_op);
}

/// The arithmetic text-region decoding procedure (§6.4.5), returning the region
/// bitmap. Shared by the text-region segment and the symbol-dictionary aggregate
/// path. Per-symbol refinement (IARI / §6.4.11) is honoured when `params.refine`.
#[allow(clippy::too_many_arguments)]
fn decode_text_region_arith(
    mq: &mut MqDecoder,
    symbols: &[&Bitmap],
    params: &TextRegionParams,
    _code_len: u32,
    iadt: &mut IntContext,
    iafs: &mut IntContext,
    iads: &mut IntContext,
    iait: &mut IntContext,
    iari: &mut IntContext,
    iaid: &mut IaidContext,
    _gb_cx: &mut [ArithContext],
    gr_cx: &mut [ArithContext],
) -> Bitmap {
    let strips = params.strips.max(1) as i64;
    let mut region = Bitmap::new(params.width, params.height);
    let num_instances = params.num_instances;

    // Refinement integer contexts (only used when params.refine).
    let mut iardw = IntContext::default();
    let mut iardh = IntContext::default();
    let mut iardx = IntContext::default();
    let mut iardy = IntContext::default();

    let mut stript: i64 = match iadt.decode(mq) {
        IntResult::Value(v) => -(v as i64) * strips,
        IntResult::Oob => 0,
    };
    let mut first_s: i64 = 0;
    let mut inst: u32 = 0;
    let mut guard = 0usize;
    let guard_max = (num_instances as usize + 64) * 4 + 1024;

    while inst < num_instances {
        guard += 1;
        if guard > guard_max {
            break;
        }
        let dt = match iadt.decode(mq) {
            IntResult::Value(v) => v as i64,
            IntResult::Oob => break,
        };
        stript += dt * strips;

        let dfs = match iafs.decode(mq) {
            IntResult::Value(v) => v as i64,
            IntResult::Oob => break,
        };
        first_s += dfs;
        let mut cur_s = first_s;

        let mut first_in_strip = true;
        loop {
            if inst >= num_instances {
                break;
            }
            if !first_in_strip {
                match iads.decode(mq) {
                    IntResult::Oob => break,
                    IntResult::Value(ids) => {
                        cur_s += ids as i64 + params.ds_offset as i64;
                    }
                }
            }
            first_in_strip = false;

            let curt = if strips == 1 {
                0
            } else {
                match iait.decode(mq) {
                    IntResult::Value(v) => v as i64,
                    IntResult::Oob => 0,
                }
            };
            let t = stript + curt;

            let id = iaid.decode(mq) as usize;
            guard += 1;
            if guard > guard_max {
                break;
            }
            let Some(&sym) = symbols.get(id) else {
                inst += 1;
                continue;
            };

            // Per-symbol refinement (§6.4.11): if SBREFINE and the IARI bit is
            // set, refine the symbol bitmap before placing it.
            let refined_owned: Option<Bitmap> = if params.refine {
                let ri = match iari.decode(mq) {
                    IntResult::Value(v) => v,
                    IntResult::Oob => 0,
                };
                if ri != 0 {
                    let rdw = int_or_zero(iardw.decode(mq));
                    let rdh = int_or_zero(iardh.decode(mq));
                    let rdx = int_or_zero(iardx.decode(mq));
                    let rdy = int_or_zero(iardy.decode(mq));
                    let nw = (sym.width as i64 + rdw).max(1) as usize;
                    let nh = (sym.height as i64 + rdh).max(1) as usize;
                    // Reference offset per §6.4.11: floor(RDW/2)+RDX, floor(RDH/2)+RDY.
                    let gdx = (rdw >> 1) + rdx;
                    let gdy = (rdh >> 1) + rdy;
                    let r = decode_refinement_bitmap(
                        mq,
                        nw,
                        nh,
                        sym,
                        gdx,
                        gdy,
                        params.rtemplate,
                        false,
                        &params.rat,
                        gr_cx,
                    );
                    Some(r)
                } else {
                    None
                }
            } else {
                None
            };
            let place = refined_owned.as_ref().unwrap_or(sym);
            place_symbol(
                &mut region,
                place,
                cur_s,
                t,
                params.ref_corner,
                params.transposed,
                params.comb_op,
            );

            let adv = if params.transposed {
                place.height as i64
            } else {
                place.width as i64
            };
            cur_s += adv.saturating_sub(1);
            inst += 1;
        }
    }
    region
}

/// `Value` → the integer, `Oob` → 0 (used for optional refinement deltas).
fn int_or_zero(r: IntResult) -> i64 {
    match r {
        IntResult::Value(v) => v as i64,
        IntResult::Oob => 0,
    }
}

/// Pick a Huffman table for a four-way selector (`0/1/2` standard, `3` custom),
/// advancing the custom index; falls back to `std_a` if a custom is missing.
fn select_table3<'a>(
    sel: u8,
    std_a: u8,
    std_b: u8,
    std_c: u8,
    customs: &[&'a HuffTable],
    cidx: &mut usize,
) -> Option<HuffTableRef<'a>> {
    match sel {
        0 => huff::standard_table(std_a).map(HuffTableRef::Owned),
        1 => huff::standard_table(std_b).map(HuffTableRef::Owned),
        2 => huff::standard_table(std_c).map(HuffTableRef::Owned),
        3 => {
            let t = customs.get(*cidx).copied();
            *cidx += 1;
            t.map(HuffTableRef::Borrowed)
                .or_else(|| huff::standard_table(std_a).map(HuffTableRef::Owned))
        }
        _ => huff::standard_table(std_a).map(HuffTableRef::Owned),
    }
}

/// The Huffman text-region decode path (§6.4.5 with Annex B tables). Builds the
/// run-code symbol-ID table, then decodes strip/symbol placements via the
/// selected FS/DS/DT tables. Per-symbol refinement is honoured when `refine`.
fn decode_text_region_huffman(
    data: &[u8],
    symbols: &[&Bitmap],
    params: &TextRegionParams,
    huff_flags: u16,
    state: &Jbig2State,
) -> Option<Bitmap> {
    // Note: table-segment references for the text region's custom Huffman tables
    // are uncommon; the standard tables cover the overwhelming majority. We
    // resolve customs from any referred-to table segments still in order.
    let customs: Vec<&HuffTable> = state.custom_tables.values().collect();
    let mut cidx = 0usize;

    let fs_sel = (huff_flags & 0x0003) as u8;
    let ds_sel = ((huff_flags >> 2) & 0x0003) as u8;
    let dt_sel = ((huff_flags >> 4) & 0x0003) as u8;
    let rdw_sel = ((huff_flags >> 6) & 0x0003) as u8;
    let rdh_sel = ((huff_flags >> 8) & 0x0003) as u8;
    let rdx_sel = ((huff_flags >> 10) & 0x0003) as u8;
    let rdy_sel = ((huff_flags >> 12) & 0x0003) as u8;
    let rsize_sel = ((huff_flags >> 14) & 0x0001) as u8;

    let fs_table = select_table3(fs_sel, 6, 7, 6, &customs, &mut cidx)?;
    let ds_table = select_table3(ds_sel, 8, 9, 10, &customs, &mut cidx)?;
    let dt_table = select_table3(dt_sel, 11, 12, 13, &customs, &mut cidx)?;
    let rdw_table = select_table3(rdw_sel, 14, 15, 14, &customs, &mut cidx)?;
    let rdh_table = select_table3(rdh_sel, 14, 15, 14, &customs, &mut cidx)?;
    let rdx_table = select_table3(rdx_sel, 14, 15, 14, &customs, &mut cidx)?;
    let rdy_table = select_table3(rdy_sel, 14, 15, 14, &customs, &mut cidx)?;
    let rsize_table = select_table(rsize_sel, 1, 1, &customs, &mut cidx)?;

    let mut r = BitReader::new(data);
    // The run-code-built symbol-ID Huffman table (§7.4.3.1.7).
    let id_table = huff::build_symbol_id_table(&mut r, symbols.len())?;

    let strips = params.strips.max(1) as i64;
    let mut region = Bitmap::new(params.width, params.height);
    let num_instances = params.num_instances;
    let mut gr_cx = vec![ArithContext::default(); 1 << 13];

    // Initial STRIPT.
    let mut stript: i64 = match dt_table.decode(&mut r)? {
        HuffResult::Value(v) => -(v as i64) * strips,
        HuffResult::Oob => 0,
    };
    let mut first_s: i64 = 0;
    let mut inst: u32 = 0;
    let mut guard = 0usize;
    let guard_max = (num_instances as usize + 64) * 4 + 1024;

    while inst < num_instances {
        guard += 1;
        if guard > guard_max {
            break;
        }
        let dt = match dt_table.decode(&mut r)? {
            HuffResult::Value(v) => v as i64,
            HuffResult::Oob => break,
        };
        stript += dt * strips;

        let dfs = match fs_table.decode(&mut r)? {
            HuffResult::Value(v) => v as i64,
            HuffResult::Oob => break,
        };
        first_s += dfs;
        let mut cur_s = first_s;

        let mut first_in_strip = true;
        loop {
            if inst >= num_instances {
                break;
            }
            if !first_in_strip {
                match ds_table.decode(&mut r)? {
                    HuffResult::Oob => break,
                    HuffResult::Value(ids) => {
                        cur_s += ids as i64 + params.ds_offset as i64;
                    }
                }
            }
            first_in_strip = false;

            // CURT: per-symbol T offset. For Huffman, read log_strips bits when
            // there is more than one strip (§6.4.9).
            let curt = if strips == 1 {
                0
            } else {
                r.bits(params.log_strips)? as i64
            };
            let t = stript + curt;

            // Symbol ID via the run-code symbol-ID table.
            let id = match id_table.decode(&mut r)? {
                HuffResult::Value(v) if v >= 0 => v as usize,
                _ => break,
            };
            guard += 1;
            if guard > guard_max {
                break;
            }
            let Some(&sym) = symbols.get(id) else {
                inst += 1;
                continue;
            };

            // Per-symbol refinement (§6.4.11) — Huffman variant.
            let refined_owned: Option<Bitmap> = if params.refine {
                let ri = r.bit()? as i32;
                if ri != 0 {
                    let rdw = match rdw_table.decode(&mut r)? {
                        HuffResult::Value(v) => v as i64,
                        HuffResult::Oob => 0,
                    };
                    let rdh = match rdh_table.decode(&mut r)? {
                        HuffResult::Value(v) => v as i64,
                        HuffResult::Oob => 0,
                    };
                    let rdx = match rdx_table.decode(&mut r)? {
                        HuffResult::Value(v) => v as i64,
                        HuffResult::Oob => 0,
                    };
                    let rdy = match rdy_table.decode(&mut r)? {
                        HuffResult::Value(v) => v as i64,
                        HuffResult::Oob => 0,
                    };
                    let _rsize = match rsize_table.decode(&mut r)? {
                        HuffResult::Value(v) if v >= 0 => v as usize,
                        _ => 0,
                    };
                    r.byte_align();
                    let nw = (sym.width as i64 + rdw).max(1) as usize;
                    let nh = (sym.height as i64 + rdh).max(1) as usize;
                    let gdx = (rdw >> 1) + rdx;
                    let gdy = (rdh >> 1) + rdy;
                    // The refinement bitmap is MQ-coded (byte-aligned) within the
                    // Huffman stream.
                    let start = r.byte_pos();
                    let mut mqd = MqDecoder::new(&r.data()[start..]);
                    let refined = decode_refinement_bitmap(
                        &mut mqd,
                        nw,
                        nh,
                        sym,
                        gdx,
                        gdy,
                        params.rtemplate,
                        false,
                        &params.rat,
                        &mut gr_cx,
                    );
                    // Skip the consumed refinement bytes (RSIZE) if provided.
                    if _rsize > 0 {
                        for _ in 0..(_rsize * 8) {
                            let _ = r.bit();
                        }
                    }
                    Some(refined)
                } else {
                    None
                }
            } else {
                None
            };

            let place = refined_owned.as_ref().unwrap_or(sym);
            place_symbol(
                &mut region,
                place,
                cur_s,
                t,
                params.ref_corner,
                params.transposed,
                params.comb_op,
            );

            let adv = if params.transposed {
                place.height as i64
            } else {
                place.width as i64
            };
            cur_s += adv.saturating_sub(1);
            inst += 1;
        }
    }
    Some(region)
}

/// Place one symbol bitmap into the text region at strip coordinate `(s, t)`,
/// honouring the reference corner, transposition and symbol combination operator
/// (§6.4.5 step 3c.x). `s` advances along the line; `t` is across it.
fn place_symbol(
    region: &mut Bitmap,
    sym: &Bitmap,
    s: i64,
    t: i64,
    ref_corner: u8,
    transposed: bool,
    op: u8,
) {
    // Map (s, t) + the reference corner to the symbol's top-left in region
    // coordinates. Reference corners: 0 BOTTOMLEFT, 1 TOPLEFT, 2 BOTTOMRIGHT,
    // 3 TOPRIGHT.
    let (ox, oy) = if !transposed {
        let x = s;
        let y = match ref_corner {
            0 | 2 => t - (sym.height as i64 - 1), // bottom corners
            _ => t,                               // top corners
        };
        // Right corners measure S from the right edge; but the common encoders
        // emit left-corner regions, and S already tracks the left edge after the
        // advance rule. Keep x at s for left corners; shift for right corners.
        let x = match ref_corner {
            2 | 3 => x - (sym.width as i64 - 1),
            _ => x,
        };
        (x, y)
    } else {
        let y = s;
        let x = match ref_corner {
            0 | 1 => t,                      // left corners
            _ => t - (sym.width as i64 - 1), // right corners
        };
        let y = match ref_corner {
            0 | 2 => y - (sym.height as i64 - 1),
            _ => y,
        };
        (x, y)
    };

    for sy in 0..sym.height {
        for sx in 0..sym.width {
            // OR with a 0 source bit is a no-op; skip it for speed.
            if op == 0 && !sym.data[sy * sym.width + sx] {
                continue;
            }
            let px = ox + sx as i64;
            let py = oy + sy as i64;
            if px < 0 || py < 0 || px >= region.width as i64 || py >= region.height as i64 {
                continue;
            }
            let s_bit = sym.data[sy * sym.width + sx];
            let idx = py as usize * region.width + px as usize;
            let d = region.data[idx];
            region.data[idx] = match op {
                0 => d | s_bit,
                1 => d & s_bit,
                2 => d ^ s_bit,
                3 => !(d ^ s_bit),
                _ => s_bit,
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Output packing
// ---------------------------------------------------------------------------

/// Pack a bilevel page bitmap into MSB-first rows (each padded to a byte
/// boundary). The output uses `0 = black` (JBIG2 1/black pixel → bit 0) so a
/// default `/Decode [0 1]` DeviceGray image renders correctly.
fn pack_bitmap(bm: &Bitmap) -> Vec<u8> {
    let row_bytes = bm.width.div_ceil(8);
    let mut out = vec![0u8; row_bytes * bm.height];
    for y in 0..bm.height {
        for x in 0..bm.width {
            // black pixel (true) → bit 0; white (false) → bit 1.
            if !bm.data[y * bm.width + x] {
                out[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built JBIG2 stream: page-info segment (48) declaring a 8×2 page,
    /// then a single MMR generic-region segment that paints a known pattern. The
    /// MMR path reuses the CCITT G4 core, so this validates segment parsing +
    /// region compositing + packing end to end without the arithmetic coder.
    #[test]
    fn page_plus_mmr_generic_region() {
        // Build the bytes manually.
        let mut s: Vec<u8> = Vec::new();

        // --- Segment 0: page info (type 48) ---
        // header: number(4)=0, flags(1)=48, refflags(1)=0x00 (0 refs),
        // page_assoc(1)=1, data_length(4)=19
        s.extend_from_slice(&0u32.to_be_bytes());
        s.push(48);
        s.push(0x00);
        s.push(1);
        let page_data_len: u32 = 19;
        s.extend_from_slice(&page_data_len.to_be_bytes());
        // page data: width(4)=8 height(4)=2 xres(4)=0 yres(4)=0 flags(1)=0
        // striping(2)=0  => 4+4+4+4+1+2 = 19
        s.extend_from_slice(&8u32.to_be_bytes());
        s.extend_from_slice(&2u32.to_be_bytes());
        s.extend_from_slice(&0u32.to_be_bytes());
        s.extend_from_slice(&0u32.to_be_bytes());
        s.push(0x00);
        s.extend_from_slice(&0u16.to_be_bytes());

        // --- Segment 1: immediate generic region (type 38), MMR ---
        // Encode the MMR data for an 8×2 bitmap: row0 = WWW BB WWW, row1 = same.
        let mmr_payload = build_g4(&[
            &[false, false, false, true, true, false, false, false],
            &[false, false, false, true, true, false, false, false],
        ]);
        // region info(17) + flags(1) = 18, then mmr payload.
        let region_info_and_flags = {
            let mut v = Vec::new();
            v.extend_from_slice(&8u32.to_be_bytes()); // width
            v.extend_from_slice(&2u32.to_be_bytes()); // height
            v.extend_from_slice(&0u32.to_be_bytes()); // x
            v.extend_from_slice(&0u32.to_be_bytes()); // y
            v.push(0x00); // comb op = OR
            v.push(0x01); // generic flags: MMR=1
            v
        };
        let seg1_data_len = (region_info_and_flags.len() + mmr_payload.len()) as u32;
        s.extend_from_slice(&1u32.to_be_bytes()); // number
        s.push(38); // flags: type 38
        s.push(0x00); // ref flags: 0 refs
        s.push(1); // page assoc
        s.extend_from_slice(&seg1_data_len.to_be_bytes());
        s.extend_from_slice(&region_info_and_flags);
        s.extend_from_slice(&mmr_payload);

        let out = jbig2_decode(&s, None, None).expect("jbig2 decode");
        // 8×2, row_bytes=1. WWW BB WWW with 0=black → 1 1 1 0 0 1 1 1 = 0xE7.
        assert_eq!(out, vec![0xE7, 0xE7]);
    }

    /// Decode an arithmetic generic region built by the matching MQ *encoder*
    /// (round-trip): a tiny 8×4 bitmap encodes and decodes to the same pixels.
    #[test]
    fn arithmetic_generic_region_roundtrip() {
        // Target bitmap.
        let target: Vec<Vec<bool>> = vec![
            vec![true, false, false, false, false, false, false, true],
            vec![false, true, false, false, false, false, true, false],
            vec![false, false, true, false, false, true, false, false],
            vec![false, false, false, true, true, false, false, false],
        ];
        let w = 8;
        let h = 4;
        let template = 0u8;
        let at: [(i8, i8); 4] = [(3, -1), (-3, -1), (2, -2), (-2, -2)];

        // Encode with the MQ encoder using the same context model.
        let coded = encode_generic(&target, w, h, template, &at);

        // Decode and compare.
        let mut mqd = MqDecoder::new(&coded);
        let bm = decode_generic_bitmap(
            &mut mqd,
            w,
            h,
            template,
            false,
            &at,
            &mut vec![ArithContext::default(); 1 << 16],
        );
        for (y, row) in target.iter().enumerate().take(h) {
            for (x, &want) in row.iter().enumerate().take(w) {
                assert_eq!(bm.data[y * w + x], want, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// Generic-region round-trip for GB template 3 (the one-row-above template),
    /// exercising a different context-assembly path than template 0.
    #[test]
    fn arithmetic_generic_region_template3_roundtrip() {
        let target: Vec<Vec<bool>> = vec![
            vec![true, true, false, false, true, true],
            vec![false, false, true, true, false, false],
            vec![true, false, true, false, true, false],
            vec![false, true, false, true, false, true],
            vec![true, true, true, false, false, false],
        ];
        let (w, h) = (6, 5);
        let template = 3u8;
        let at: [(i8, i8); 4] = [(-2, 0), (0, 0), (0, 0), (0, 0)];
        let coded = encode_generic(&target, w, h, template, &at);
        let mut mqd = MqDecoder::new(&coded);
        let bm = decode_generic_bitmap(
            &mut mqd,
            w,
            h,
            template,
            false,
            &at,
            &mut vec![ArithContext::default(); 1 << 16],
        );
        for (y, row) in target.iter().enumerate() {
            for (x, &want) in row.iter().enumerate() {
                assert_eq!(bm.data[y * w + x], want, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// Generic-region round-trip with TPGDON typical prediction: a bitmap whose
    /// middle rows repeat exercises the LTP copy-row path in both coder halves.
    #[test]
    fn arithmetic_generic_region_tpgdon_roundtrip() {
        let target: Vec<Vec<bool>> = vec![
            vec![true, false, true, false, true, false, true, false],
            vec![true, true, false, false, true, true, false, false],
            vec![true, true, false, false, true, true, false, false], // == row 1 (typical)
            vec![true, true, false, false, true, true, false, false], // == row 2 (typical)
            vec![false, false, false, false, false, false, false, false],
        ];
        let (w, h) = (8, 5);
        let template = 0u8;
        let at: [(i8, i8); 4] = [(3, -1), (-3, -1), (2, -2), (-2, -2)];
        let coded = encode_generic_tpgdon(&target, w, h, template, &at);
        let mut mqd = MqDecoder::new(&coded);
        let bm = decode_generic_bitmap(
            &mut mqd,
            w,
            h,
            template,
            true, // TPGDON
            &at,
            &mut vec![ArithContext::default(); 1 << 16],
        );
        for (y, row) in target.iter().enumerate() {
            for (x, &want) in row.iter().enumerate() {
                assert_eq!(bm.data[y * w + x], want, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// Generic *refinement* region round-trip (GR template 1): a reference
    /// bitmap is refined into a slightly different target. The encoder mirrors
    /// `decode_refinement_bitmap` and the decode must reproduce the target
    /// pixel-exact, validating the GR context assembly and refinement decode.
    #[test]
    fn refinement_region_template1_roundtrip() {
        let reference_rows: Vec<Vec<bool>> = vec![
            vec![true, true, false, false, true, true],
            vec![true, false, false, true, false, true],
            vec![false, false, true, true, false, false],
            vec![false, true, true, false, true, false],
            vec![true, false, false, true, false, true],
        ];
        // The target differs from the reference in a few pixels.
        let target: Vec<Vec<bool>> = vec![
            vec![true, true, false, true, true, true],
            vec![true, false, true, true, false, true],
            vec![false, false, true, true, false, false],
            vec![false, true, true, false, true, true],
            vec![true, false, false, false, false, true],
        ];
        let (w, h) = (6, 5);
        let mut reference = Bitmap::new(w, h);
        for (y, row) in reference_rows.iter().enumerate() {
            for (x, &b) in row.iter().enumerate() {
                reference.set(x, y, b);
            }
        }
        let at: [(i8, i8); 2] = [(-1, -1), (-1, -1)];
        let coded = encode_refinement(&target, w, h, &reference, 0, 0, 1, &at);
        let mut mqd = MqDecoder::new(&coded);
        let out = decode_refinement_bitmap(
            &mut mqd,
            w,
            h,
            &reference,
            0,
            0,
            1,
            false,
            &at,
            &mut vec![ArithContext::default(); 1 << 13],
        );
        for (y, row) in target.iter().enumerate() {
            for (x, &want) in row.iter().enumerate() {
                assert_eq!(out.data[y * w + x], want, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// Generic refinement region round-trip with GR template 0 (the 13-pixel,
    /// two-adaptive-pixel template), exercising the other context path.
    #[test]
    fn refinement_region_template0_roundtrip() {
        let reference_rows: Vec<Vec<bool>> = vec![
            vec![false, true, true, false, false, true, false],
            vec![true, false, true, true, false, false, true],
            vec![true, true, false, false, true, true, false],
            vec![false, false, true, true, false, true, true],
        ];
        let target: Vec<Vec<bool>> = vec![
            vec![false, true, true, true, false, true, false],
            vec![true, false, false, true, false, false, true],
            vec![true, true, false, false, true, true, true],
            vec![false, true, true, true, false, true, true],
        ];
        let (w, h) = (7, 4);
        let mut reference = Bitmap::new(w, h);
        for (y, row) in reference_rows.iter().enumerate() {
            for (x, &b) in row.iter().enumerate() {
                reference.set(x, y, b);
            }
        }
        let at: [(i8, i8); 2] = [(-1, -1), (-1, -1)];
        let coded = encode_refinement(&target, w, h, &reference, 0, 0, 0, &at);
        let mut mqd = MqDecoder::new(&coded);
        let out = decode_refinement_bitmap(
            &mut mqd,
            w,
            h,
            &reference,
            0,
            0,
            0,
            false,
            &at,
            &mut vec![ArithContext::default(); 1 << 13],
        );
        for (y, row) in target.iter().enumerate() {
            for (x, &want) in row.iter().enumerate() {
                assert_eq!(out.data[y * w + x], want, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// Generic refinement region with TPGRON typical prediction. The reference is
    /// a solid-black block (its interior has uniform 3×3 neighbourhoods, so those
    /// pixels are *predicted* not coded); the target matches it except one
    /// interior pixel is flipped, forcing that row out of typical prediction. The
    /// decode must reproduce the target pixel-exact, validating the SLTP toggle
    /// and the uniform-neighbourhood prediction in both coder halves.
    #[test]
    fn refinement_region_tpgron_roundtrip() {
        let (w, h) = (5, 5);
        // Reference: solid black 5×5.
        let mut reference = Bitmap::new(w, h);
        for p in reference.data.iter_mut() {
            *p = true;
        }
        // Target: solid black except (2,2) flipped to white.
        let mut target = vec![vec![true; w]; h];
        target[2][2] = false;
        let at: [(i8, i8); 2] = [(-1, -1), (-1, -1)];
        let coded = encode_refinement_tpgron(&target, w, h, &reference, 0, 0, 1, &at);
        let mut mqd = MqDecoder::new(&coded);
        let out = decode_refinement_bitmap(
            &mut mqd,
            w,
            h,
            &reference,
            0,
            0,
            1,
            true, // TPGRON
            &at,
            &mut vec![ArithContext::default(); 1 << 13],
        );
        for (y, row) in target.iter().enumerate() {
            for (x, &want) in row.iter().enumerate() {
                assert_eq!(out.data[y * w + x], want, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// End-to-end pattern-dictionary + halftone-region round-trip. Four 2×2
    /// patterns are arithmetic-coded into a pattern dictionary; a halftone region
    /// then selects them per grid cell (via a Gray-coded 2-bitplane grayscale
    /// image) and tiles them onto the page. The decoded page must show each cell's
    /// chosen pattern at its grid position — exercising the pattern-dict decode,
    /// the grayscale §C.5 bitplane / Gray-code path, and grid placement.
    #[test]
    fn pattern_dict_and_halftone_region_roundtrip() {
        // Four distinct 2×2 patterns (gray values 0..=3).
        let patterns: Vec<Vec<Vec<bool>>> = vec![
            vec![vec![false, false], vec![false, false]], // 0
            vec![vec![true, false], vec![false, true]],   // 1
            vec![vec![false, true], vec![true, false]],   // 2
            vec![vec![true, true], vec![true, true]],     // 3
        ];
        // A 2×2 cell grid selecting patterns 1,2 / 3,0. With HRX=HRY scaled by
        // 256 (the grid is in 1/256-pixel units), placing cells 2px apart.
        let gray = vec![vec![1u32, 2u32], vec![3u32, 0u32]];
        let hbpp = 2u32;

        let pd_data = build_pattern_dict(&patterns, 2, 2);
        // Grid origin (0,0); HRX=512 (=2px to the right per column step),
        // HRY=0 (rows go straight down: y = m*HRX>>8 = m*2). So:
        //   cell(m,n): x = (n*512)>>8 = 2n, y = (m*512)>>8 = 2m.
        let ht_data = build_halftone_region(4, 4, &gray, hbpp, 0, 0, 512, 0);

        let mut s: Vec<u8> = Vec::new();
        push_segment(&mut s, 0, 48, &[], page_info_bytes(4, 4));
        push_segment(&mut s, 1, 16, &[], pd_data); // pattern dictionary
        push_segment(&mut s, 2, 22, &[1], ht_data); // immediate halftone region

        let out = jbig2_decode(&s, None, None).expect("jbig2 halftone decode");

        // Expected 4×4 page: pattern at cell (m,n) placed at (2n, 2m).
        let mut expected = vec![vec![false; 4]; 4];
        for (m, grow) in gray.iter().enumerate() {
            for (n, &gv) in grow.iter().enumerate() {
                let pat = &patterns[gv as usize];
                for (py, prow) in pat.iter().enumerate() {
                    for (px, &b) in prow.iter().enumerate() {
                        let x = 2 * n + px;
                        let y = 2 * m + py;
                        if b {
                            expected[y][x] = true;
                        }
                    }
                }
            }
        }
        let row_bytes = 4usize.div_ceil(8);
        for (y, row) in expected.iter().enumerate() {
            for (x, &black) in row.iter().enumerate() {
                let byte = out[y * row_bytes + x / 8];
                let bit = (byte >> (7 - (x % 8))) & 1;
                let got_black = bit == 0;
                assert_eq!(got_black, black, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// MMR (HMMR = 1) halftone region with **two** grayscale bitplanes (HBPP = 2).
    /// The §C.5 grayscale image is encoded as two Gray-code planes packed into one
    /// continuous Group-4 MMR bitstream (no byte realignment between planes); the
    /// decoder must resume the second plane from the bit position the first left
    /// off (the resumable MMR core) and recover BOTH planes, not just the first.
    /// A 4×4 cell grid selecting all four patterns 0..=3 round-trips pixel-exact —
    /// proof that the multi-bitplane MMR path (previously only first-plane) works.
    #[test]
    fn mmr_multibitplane_halftone_region_roundtrip() {
        // Four distinct 2×2 patterns (gray values 0..=3 → HBPP = 2).
        let patterns: Vec<Vec<Vec<bool>>> = vec![
            vec![vec![false, false], vec![false, false]], // 0
            vec![vec![true, false], vec![false, true]],   // 1
            vec![vec![false, true], vec![true, false]],   // 2
            vec![vec![true, true], vec![true, true]],     // 3
        ];
        // A 4×4 grid mixing all four gray values so BOTH Gray-code bitplanes carry
        // non-trivial content (the MSB plane = value bit 1, the LSB-derived plane
        // = value bit 0 XOR bit 1). If only the first (MSB) plane were recovered
        // the low bit would be lost and cells 1↔2 / 0↔3 would be confused.
        let gray = vec![
            vec![0u32, 1, 2, 3],
            vec![3, 2, 1, 0],
            vec![1, 3, 0, 2],
            vec![2, 0, 3, 1],
        ];
        let hbpp = 2u32;

        let pd_data = build_pattern_dict(&patterns, 2, 2);
        // Grid origin (0,0); HRX=512 (=2px per column), HRY=0 (y = m*2). Region is
        // 8×8 so the 4×4 grid of 2×2 cells tiles it exactly.
        let ht_data = build_halftone_region_mmr(8, 8, &gray, hbpp, 0, 0, 512, 0);

        let mut s: Vec<u8> = Vec::new();
        push_segment(&mut s, 0, 48, &[], page_info_bytes(8, 8));
        push_segment(&mut s, 1, 16, &[], pd_data); // pattern dictionary
        push_segment(&mut s, 2, 22, &[1], ht_data); // immediate halftone region (MMR)

        let out = jbig2_decode(&s, None, None).expect("jbig2 MMR halftone decode");

        // Expected 8×8 page: pattern at cell (m,n) placed at (2n, 2m).
        let mut expected = vec![vec![false; 8]; 8];
        for (m, grow) in gray.iter().enumerate() {
            for (n, &gv) in grow.iter().enumerate() {
                let pat = &patterns[gv as usize];
                for (py, prow) in pat.iter().enumerate() {
                    for (px, &b) in prow.iter().enumerate() {
                        if b {
                            expected[2 * m + py][2 * n + px] = true;
                        }
                    }
                }
            }
        }
        let row_bytes = 8usize.div_ceil(8);
        for (y, row) in expected.iter().enumerate() {
            for (x, &black) in row.iter().enumerate() {
                let byte = out[y * row_bytes + x / 8];
                let bit = (byte >> (7 - (x % 8))) & 1;
                let got_black = bit == 0;
                assert_eq!(got_black, black, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// REFAGG (refinement/aggregate) symbol-dictionary round-trip. An input
    /// dictionary supplies one symbol; a second, REFAGG-coded dictionary defines
    /// a new symbol as a refinement of that input symbol (REFAGGNINST = 1). A
    /// text region then places the refined symbol; the decoded page must match
    /// the refined target, exercising `decode_refagg_symbol` + the refinement
    /// decode driven through the symbol-dictionary's shared contexts.
    #[test]
    fn symbol_dictionary_refagg_roundtrip() {
        // The reference (input) symbol and the refined target (same size).
        let input_sym: Vec<Vec<bool>> = vec![
            vec![true, true, false, false],
            vec![true, false, false, true],
            vec![false, false, true, true],
            vec![false, true, true, false],
        ];
        let refined: Vec<Vec<bool>> = vec![
            vec![true, true, false, true],
            vec![true, false, true, true],
            vec![false, false, true, true],
            vec![false, true, true, false],
        ];
        let mut reference = Bitmap::new(4, 4);
        for (y, row) in input_sym.iter().enumerate() {
            for (x, &b) in row.iter().enumerate() {
                reference.set(x, y, b);
            }
        }

        // Input dict (segment 1) = one generic symbol. REFAGG dict (segment 2)
        // refers to it and defines the refined symbol (total_syms = 2 → IAID
        // code length 1; input id 0 references input_sym).
        let input_dict = build_symbol_dict(&[&input_sym]);
        let refagg_dict = build_refagg_symbol_dict(&refined, 0, 0, 0, &reference, 2);
        // Text region (segment 3) refers to the REFAGG dict and places its one
        // symbol (id 0) at S=0.
        let tr_data = build_text_region(4, 4, &[(0usize, 0i64, 0i64)], 1);

        let mut s: Vec<u8> = Vec::new();
        push_segment(&mut s, 0, 48, &[], page_info_bytes(4, 4));
        push_segment(&mut s, 1, 0, &[], input_dict);
        push_segment(&mut s, 2, 0, &[1], refagg_dict);
        push_segment(&mut s, 3, 6, &[2], tr_data);

        let out = jbig2_decode(&s, None, None).expect("jbig2 refagg decode");

        let row_bytes = 4usize.div_ceil(8);
        for (y, row) in refined.iter().enumerate() {
            for (x, &black) in row.iter().enumerate() {
                let byte = out[y * row_bytes + x / 8];
                let bit = (byte >> (7 - (x % 8))) & 1;
                let got_black = bit == 0;
                assert_eq!(got_black, black, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// End-to-end symbol dictionary + text region round-trip. Two distinct
    /// symbols are arithmetic-coded into a symbol-dictionary segment; a text
    /// region then places them at known positions. The decoded page must show
    /// both glyphs at their placements — exercising the MQ coder, the integer
    /// arithmetic decoders (IADH/IADW/IAEX/IADT/IAFS/IADS/IAIT) and the IAID
    /// symbol-id decoder together.
    #[test]
    fn symbol_dictionary_and_text_region_roundtrip() {
        // Two 4x4 symbols.
        let sym0: Vec<Vec<bool>> = vec![
            vec![true, true, true, true],
            vec![true, false, false, false],
            vec![true, true, true, false],
            vec![true, false, false, false],
        ];
        let sym1: Vec<Vec<bool>> = vec![
            vec![false, true, true, false],
            vec![true, false, false, true],
            vec![true, true, true, true],
            vec![true, false, false, true],
        ];

        // --- Build the symbol-dictionary segment data (arithmetic) ---
        let sd_data = build_symbol_dict(&[&sym0, &sym1]);
        // --- Build the text-region segment placing sym0 at S=0, sym1 at S=6 ---
        // Region is 12x4, one strip; both symbols on the same line.
        let tr_data = build_text_region(12, 4, &[(0usize, 0i64, 0i64), (1usize, 6, 0)], 2);

        // --- Assemble the full JBIG2 stream: page-info, symbol dict, text region ---
        let mut s: Vec<u8> = Vec::new();
        // Segment 0: page info (12x4).
        push_segment(&mut s, 0, 48, &[], page_info_bytes(12, 4));
        // Segment 1: symbol dictionary (type 0), no referred-to segments.
        push_segment(&mut s, 1, 0, &[], sd_data);
        // Segment 2: immediate text region (type 6) referring to segment 1.
        push_segment(&mut s, 2, 6, &[1], tr_data);

        let out = jbig2_decode(&s, None, None).expect("jbig2 symbol+text decode");

        // Reconstruct the expected 12x4 page and compare (0 = black packing).
        let mut expected = vec![vec![false; 12]; 4];
        blit(&mut expected, &sym0, 0, 0);
        blit(&mut expected, &sym1, 6, 0);
        let row_bytes = 12usize.div_ceil(8);
        for (y, row) in expected.iter().enumerate() {
            for (x, &black) in row.iter().enumerate() {
                let byte = out[y * row_bytes + x / 8];
                let bit = (byte >> (7 - (x % 8))) & 1;
                // black pixel → bit 0; white → bit 1.
                let got_black = bit == 0;
                assert_eq!(got_black, black, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// End-to-end Huffman-coded symbol dictionary + Huffman text region
    /// round-trip. Two symbols are coded into a Huffman symbol dictionary
    /// (height/width via standard tables B.4/B.2, an uncompressed collective
    /// bitmap); a Huffman text region (FS/DS/DT via B.6/B.8/B.11, the run-code
    /// symbol-ID table) then places them. The decoded page must show both glyphs
    /// at their placements — exercising the entire Huffman path.
    #[test]
    fn huffman_symbol_dict_and_text_region_roundtrip() {
        // Two 4×4 symbols (same height class).
        let sym0: Vec<Vec<bool>> = vec![
            vec![true, true, true, true],
            vec![true, false, false, false],
            vec![true, true, true, false],
            vec![true, false, false, false],
        ];
        let sym1: Vec<Vec<bool>> = vec![
            vec![false, true, true, false],
            vec![true, false, false, true],
            vec![true, true, true, true],
            vec![true, false, false, true],
        ];

        let sd_data = build_huffman_symbol_dict(&[&sym0, &sym1]);
        // Place sym0 at S=0, sym1 at S=6 in a 12×4 region (one strip), width 4.
        let tr_data = build_huffman_text_region(12, 4, &[(0usize, 0i64), (1, 6)], 2, 4);

        let mut s: Vec<u8> = Vec::new();
        push_segment(&mut s, 0, 48, &[], page_info_bytes(12, 4));
        push_segment(&mut s, 1, 0, &[], sd_data); // symbol dictionary (Huffman)
        push_segment(&mut s, 2, 6, &[1], tr_data); // text region (Huffman)

        let out = jbig2_decode(&s, None, None).expect("jbig2 huffman decode");

        let mut expected = vec![vec![false; 12]; 4];
        blit(&mut expected, &sym0, 0, 0);
        blit(&mut expected, &sym1, 6, 0);
        let row_bytes = 12usize.div_ceil(8);
        for (y, row) in expected.iter().enumerate() {
            for (x, &black) in row.iter().enumerate() {
                let byte = out[y * row_bytes + x / 8];
                let bit = (byte >> (7 - (x % 8))) & 1;
                let got_black = bit == 0;
                assert_eq!(got_black, black, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// Huffman **+ REFAGG** symbol-dictionary round-trip (SDHUFF = 1 AND
    /// SDREFAGG = 1) — previously skipped. An input dictionary supplies one
    /// generic symbol; a second, Huffman REFAGG-coded dictionary defines a new
    /// symbol as a single-instance refinement (REFAGGNINST = 1) of that input
    /// symbol, with the refinement deltas Huffman-coded (ID as fixed
    /// SBSYMCODELEN bits, RDX/RDY via B.15, BMSIZE via B.1) and the refinement
    /// bitmap MQ-arithmetic-coded byte-aligned in the Huffman stream. A Huffman
    /// text region then places the refined symbol; the decoded page must match
    /// the refined target pixel-exact — exercising `decode_refagg_symbol_huffman`.
    #[test]
    fn huffman_refagg_symbol_dict_roundtrip() {
        // Reference (input) symbol and the refined target (same 4×4 size).
        let input_sym: Vec<Vec<bool>> = vec![
            vec![true, true, false, false],
            vec![true, false, false, true],
            vec![false, false, true, true],
            vec![false, true, true, false],
        ];
        let refined: Vec<Vec<bool>> = vec![
            vec![true, true, false, true],
            vec![true, false, true, true],
            vec![false, false, true, true],
            vec![false, true, true, false],
        ];
        let mut reference = Bitmap::new(4, 4);
        for (y, row) in input_sym.iter().enumerate() {
            for (x, &b) in row.iter().enumerate() {
                reference.set(x, y, b);
            }
        }

        // Input dict (segment 1, Huffman) = one generic symbol. Huffman REFAGG
        // dict (segment 2) refers to it and defines the refined symbol
        // (total_syms = 2 → SBSYMCODELEN = 1; input id 0 references input_sym).
        let input_dict = build_huffman_symbol_dict(&[&input_sym]);
        let refagg_dict = build_huffman_refagg_symbol_dict(&refined, 0, 0, 0, &reference, 2);
        // Huffman text region (segment 3) refers to the REFAGG dict and places its
        // one symbol (id 0) at S=0.
        let tr_data = build_huffman_text_region(4, 4, &[(0usize, 0i64)], 1, 4);

        let mut s: Vec<u8> = Vec::new();
        push_segment(&mut s, 0, 48, &[], page_info_bytes(4, 4));
        push_segment(&mut s, 1, 0, &[], input_dict);
        push_segment(&mut s, 2, 0, &[1], refagg_dict);
        push_segment(&mut s, 3, 6, &[2], tr_data);

        let out = jbig2_decode(&s, None, None).expect("jbig2 huffman refagg decode");

        let row_bytes = 4usize.div_ceil(8);
        for (y, row) in refined.iter().enumerate() {
            for (x, &black) in row.iter().enumerate() {
                let byte = out[y * row_bytes + x / 8];
                let bit = (byte >> (7 - (x % 8))) & 1;
                let got_black = bit == 0;
                assert_eq!(got_black, black, "pixel ({x},{y}) mismatch");
            }
        }
    }

    /// Blit a small symbol into an expected-page grid at `(x, y)`.
    fn blit(page: &mut [Vec<bool>], sym: &[Vec<bool>], x: usize, y: usize) {
        for (sy, row) in sym.iter().enumerate() {
            for (sx, &b) in row.iter().enumerate() {
                if b {
                    if let Some(prow) = page.get_mut(y + sy) {
                        if let Some(cell) = prow.get_mut(x + sx) {
                            *cell = true;
                        }
                    }
                }
            }
        }
    }

    /// Page-information segment data for a `w x h` page (no default pixel).
    fn page_info_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // xres
        v.extend_from_slice(&0u32.to_be_bytes()); // yres
        v.push(0x00); // flags
        v.extend_from_slice(&0u16.to_be_bytes()); // striping
        v
    }

    /// Append a segment (header + data) to a JBIG2 stream. `referred` lists the
    /// referred-to segment numbers (short form, <= 4 refs).
    fn push_segment(out: &mut Vec<u8>, number: u32, seg_type: u8, referred: &[u32], data: Vec<u8>) {
        out.extend_from_slice(&number.to_be_bytes());
        out.push(seg_type); // flags: type, page assoc size = 1 byte
                            // Referred-to count + retention flags (short form): top 3 bits = count.
        let count = referred.len() as u8;
        out.push((count << 5) & 0xE0);
        // Referred-to segment numbers (1 byte each, since number <= 256 here).
        for &r in referred {
            out.push(r as u8);
        }
        out.push(1); // page association
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(&data);
    }

    /// An integer-arithmetic *encoder* (inverse of `IntContext::decode`,
    /// T.88 Annex A): emits a signed value through the MQ encoder with the
    /// PREV-context tree.
    struct IntEnc {
        cx: Vec<ArithContext>,
    }
    impl IntEnc {
        fn new() -> Self {
            Self {
                cx: vec![ArithContext::default(); 512],
            }
        }
        fn encode(&mut self, enc: &mut MqEncoder, value: i32) {
            let mut prev: usize = 1;
            let bit = |enc: &mut MqEncoder, cx: &mut [ArithContext], prev: &mut usize, d: u8| {
                enc.encode(&mut cx[*prev], d);
                *prev = if *prev < 256 {
                    (*prev << 1) | d as usize
                } else {
                    ((((*prev << 1) | d as usize) & 511) | 256) & 511
                };
            };
            // Sign.
            let s: u8 = if value < 0 { 1 } else { 0 };
            bit(enc, &mut self.cx, &mut prev, s);
            let mag = value.unsigned_abs();
            // Length class selection mirrors the decode offsets.
            let (n, offset, prefix): (u32, u32, &[u8]) = if mag < 4 {
                (2, 0, &[0])
            } else if mag < 20 {
                (4, 4, &[1, 0])
            } else if mag < 84 {
                (6, 20, &[1, 1, 0])
            } else if mag < 340 {
                (8, 84, &[1, 1, 1, 0])
            } else if mag < 4436 {
                (12, 340, &[1, 1, 1, 1, 0])
            } else {
                (32, 4436, &[1, 1, 1, 1, 1])
            };
            for &b in prefix {
                bit(enc, &mut self.cx, &mut prev, b);
            }
            let v = mag - offset;
            for i in (0..n).rev() {
                let b = ((v >> i) & 1) as u8;
                bit(enc, &mut self.cx, &mut prev, b);
            }
        }
        /// Encode the OOB marker (sign=1, magnitude bits all zero → value 0 neg).
        fn encode_oob(&mut self, enc: &mut MqEncoder) {
            let mut prev: usize = 1;
            let bit = |enc: &mut MqEncoder, cx: &mut [ArithContext], prev: &mut usize, d: u8| {
                enc.encode(&mut cx[*prev], d);
                *prev = if *prev < 256 {
                    (*prev << 1) | d as usize
                } else {
                    ((((*prev << 1) | d as usize) & 511) | 256) & 511
                };
            };
            bit(enc, &mut self.cx, &mut prev, 1); // sign = 1
            bit(enc, &mut self.cx, &mut prev, 0); // length class 2
            bit(enc, &mut self.cx, &mut prev, 0); // magnitude bit 1
            bit(enc, &mut self.cx, &mut prev, 0); // magnitude bit 0 → value 0, sign 1 = OOB
        }
    }

    /// A symbol-id *encoder* (inverse of `IaidContext::decode`).
    struct IaidEnc {
        cx: Vec<ArithContext>,
        code_len: u32,
    }
    impl IaidEnc {
        fn new(code_len: u32) -> Self {
            Self {
                cx: vec![ArithContext::default(); 1usize << (code_len + 1)],
                code_len,
            }
        }
        fn encode(&mut self, enc: &mut MqEncoder, id: u32) {
            let mut prev: usize = 1;
            for i in (0..self.code_len).rev() {
                let b = ((id >> i) & 1) as u8;
                enc.encode(&mut self.cx[prev], b);
                prev = (prev << 1) | b as usize;
            }
        }
    }

    /// Build a symbol-dictionary segment's data for the given symbols (arithmetic,
    /// template 0, default AT). Symbols are grouped into per-height classes.
    fn build_symbol_dict(symbols: &[&Vec<Vec<bool>>]) -> Vec<u8> {
        // Flags: SDHUFF=0, SDREFAGG=0, template bits = 0, etc. (all zero).
        let flags: u16 = 0;
        // AT pixels for template 0: the standard defaults (3,-1)(-3,-1)(2,-2)(-2,-2).
        let at: [(i8, i8); 4] = [(3, -1), (-3, -1), (2, -2), (-2, -2)];
        let num_ex = symbols.len() as u32;
        let num_new = symbols.len() as u32;

        let mut header = Vec::new();
        header.extend_from_slice(&flags.to_be_bytes());
        for (ax, ay) in at {
            header.push(ax as u8);
            header.push(ay as u8);
        }
        header.extend_from_slice(&num_ex.to_be_bytes());
        header.extend_from_slice(&num_new.to_be_bytes());

        // Arithmetic-coded body: height classes, widths, symbol bitmaps, exports.
        let mut enc = MqEncoder::new();
        let mut iadh = IntEnc::new();
        let mut iadw = IntEnc::new();
        let mut iaex = IntEnc::new();
        let mut gb_cx = vec![ArithContext::default(); 1 << 16];

        // Group consecutive symbols by equal height (these test symbols all share
        // height 4 → a single height class).
        let mut prev_height: i64 = 0;
        let mut idx = 0usize;
        while idx < symbols.len() {
            let h = symbols[idx].len() as i64;
            iadh.encode(&mut enc, (h - prev_height) as i32);
            prev_height = h;
            // Symbol width resets to 0 at the start of each height class.
            let mut prev_width: i64 = 0;
            // Emit all symbols of this height (here: all of them).
            while idx < symbols.len() && symbols[idx].len() as i64 == h {
                let w = symbols[idx][0].len() as i64;
                iadw.encode(&mut enc, (w - prev_width) as i32);
                prev_width = w;
                // Encode the symbol bitmap as an arithmetic generic region.
                encode_generic_into(
                    &mut enc,
                    symbols[idx],
                    w as usize,
                    h as usize,
                    0,
                    &at,
                    &mut gb_cx,
                );
                idx += 1;
            }
            // OOB terminates the height class's width list.
            iadw.encode_oob(&mut enc);
        }
        // Export flags: skip 0, then export all symbols (run = num_new).
        iaex.encode(&mut enc, 0);
        iaex.encode(&mut enc, symbols.len() as i32);

        let body = enc.finish();
        let mut out = header;
        out.extend_from_slice(&body);
        out
    }

    /// Encode a bitmap as an arithmetic generic region into an existing encoder,
    /// sharing the generic context array (so the symbol-dictionary decode, which
    /// reuses one GB context array across symbols, matches).
    fn encode_generic_into(
        enc: &mut MqEncoder,
        target: &[Vec<bool>],
        w: usize,
        h: usize,
        template: u8,
        at: &[(i8, i8); 4],
        gb_cx: &mut [ArithContext],
    ) {
        let mut bm = Bitmap::new(w, h);
        for (y, row) in target.iter().enumerate().take(h) {
            for (x, &want) in row.iter().enumerate().take(w) {
                let ctx = generic_context(&bm, x as i64, y as i64, template, at) as usize;
                enc.encode(&mut gb_cx[ctx], want as u8);
                bm.set(x, y, want);
            }
        }
    }

    /// Build a pattern-dictionary segment's data (arithmetic, template 0) from
    /// `patterns` (all `hdpw × hdph`). Mirrors `decode_pattern_dictionary`: the
    /// patterns are laid out into one collective bitmap and generic-encoded with
    /// the fixed AT pixels.
    fn build_pattern_dict(patterns: &[Vec<Vec<bool>>], hdpw: usize, hdph: usize) -> Vec<u8> {
        let graymax = (patterns.len() - 1) as u32;
        let mut header = Vec::new();
        header.push(0x00); // flags: HDMMR=0, HDTEMPLATE=0
        header.push(hdpw as u8);
        header.push(hdph as u8);
        header.extend_from_slice(&graymax.to_be_bytes());

        // Assemble the collective bitmap: pattern i in columns [i*hdpw,...).
        let coll_w = patterns.len() * hdpw;
        let mut collective = vec![vec![false; coll_w]; hdph];
        for (i, p) in patterns.iter().enumerate() {
            for (y, row) in p.iter().enumerate() {
                for (x, &b) in row.iter().enumerate() {
                    collective[y][i * hdpw + x] = b;
                }
            }
        }
        let at: [(i8, i8); 4] = [(-(hdpw as i8), 0), (-3, -1), (2, -2), (-2, -2)];
        let mut enc = MqEncoder::new();
        let mut gb_cx = vec![ArithContext::default(); 1 << 16];
        encode_generic_into(&mut enc, &collective, coll_w, hdph, 0, &at, &mut gb_cx);
        let body = enc.finish();
        let mut out = header;
        out.extend_from_slice(&body);
        out
    }

    /// Build a halftone-region segment's data placing the patterns indexed by
    /// `gray` (an `hgh × hgw` grid of gray values) at grid origin `(hgx, hgy)`
    /// with grid vectors `(hrx, hry)`. Encodes the grayscale image as `hbpp`
    /// Gray-coded bitplanes (MSB first) through one shared MQ encoder.
    #[allow(clippy::too_many_arguments)]
    fn build_halftone_region(
        width: u32,
        height: u32,
        gray: &[Vec<u32>],
        hbpp: u32,
        hgx: i32,
        hgy: i32,
        hrx: u16,
        hry: u16,
    ) -> Vec<u8> {
        let hgh = gray.len();
        let hgw = gray[0].len();
        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&0u32.to_be_bytes()); // x
        header.extend_from_slice(&0u32.to_be_bytes()); // y
        header.push(0x00); // region comb op = OR
        header.push(0x00); // halftone flags: HMMR=0, HTEMPLATE=0, no skip, HCOMBOP=OR, HDEFPIXEL=0
        header.extend_from_slice(&(hgw as u32).to_be_bytes());
        header.extend_from_slice(&(hgh as u32).to_be_bytes());
        header.extend_from_slice(&(hgx as u32).to_be_bytes());
        header.extend_from_slice(&(hgy as u32).to_be_bytes());
        header.extend_from_slice(&hrx.to_be_bytes());
        header.extend_from_slice(&hry.to_be_bytes());

        // Convert gray values into Gray-coded bitplanes (MSB plane first).
        // bit[j] = value-bit j XOR value-bit (j+1); the encoder writes
        // GSPLANES[j] = bit j, decoded MSB..LSB.
        let at: [(i8, i8); 4] = [(3, -1), (-3, -1), (2, -2), (-2, -2)];
        let mut enc = MqEncoder::new();
        let mut gb_cx = vec![ArithContext::default(); 1 << 16];
        // Build planes[j] (j = plane index, 0 = LSB) = Gray code of value bit j.
        let mut planes_gray = vec![vec![vec![false; hgw]; hgh]; hbpp as usize];
        for m in 0..hgh {
            for n in 0..hgw {
                let v = gray[m][n];
                // value bits (binary), then Gray-encode.
                let mut prev = 0u32;
                for j in (0..hbpp as usize).rev() {
                    let vb = (v >> j) & 1;
                    let g = vb ^ prev;
                    planes_gray[j][m][n] = g == 1;
                    prev = vb;
                }
            }
        }
        // Encode MSB plane first (plane index hbpp-1 down to 0), one shared coder.
        for j in (0..hbpp as usize).rev() {
            encode_generic_into(&mut enc, &planes_gray[j], hgw, hgh, 0, &at, &mut gb_cx);
        }
        let body = enc.finish();
        let mut out = header;
        out.extend_from_slice(&body);
        out
    }

    /// Build an **MMR** (HMMR = 1) halftone-region segment: the `hbpp` Gray-code
    /// grayscale bitplanes are packed into ONE continuous Group-4 MMR bitstream
    /// (MSB plane first, each plane an independent G4 bitmap whose READ reference
    /// resets to all-white, with **no** byte realignment between planes). This is
    /// the encoder side of the resumable-MMR multi-bitplane path.
    #[allow(clippy::too_many_arguments)]
    fn build_halftone_region_mmr(
        width: u32,
        height: u32,
        gray: &[Vec<u32>],
        hbpp: u32,
        hgx: i32,
        hgy: i32,
        hrx: u16,
        hry: u16,
    ) -> Vec<u8> {
        let hgh = gray.len();
        let hgw = gray[0].len();
        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&0u32.to_be_bytes()); // x
        header.extend_from_slice(&0u32.to_be_bytes()); // y
        header.push(0x00); // region comb op = OR
        header.push(0x01); // halftone flags: HMMR=1, HTEMPLATE=0, no skip, HCOMBOP=OR
        header.extend_from_slice(&(hgw as u32).to_be_bytes());
        header.extend_from_slice(&(hgh as u32).to_be_bytes());
        header.extend_from_slice(&(hgx as u32).to_be_bytes());
        header.extend_from_slice(&(hgy as u32).to_be_bytes());
        header.extend_from_slice(&hrx.to_be_bytes());
        header.extend_from_slice(&hry.to_be_bytes());

        // Gray-code the value bits: planes_gray[j] (j = value-bit index, 0 = LSB).
        let mut planes_gray = vec![vec![vec![false; hgw]; hgh]; hbpp as usize];
        for m in 0..hgh {
            for n in 0..hgw {
                let v = gray[m][n];
                let mut prev = 0u32;
                for j in (0..hbpp as usize).rev() {
                    let vb = (v >> j) & 1;
                    planes_gray[j][m][n] = (vb ^ prev) == 1;
                    prev = vb;
                }
            }
        }
        // One shared MMR bit-writer; encode MSB plane first, no realignment.
        let mut bw = BitW::default();
        for j in (0..hbpp as usize).rev() {
            let mut reference: Vec<usize> = changing_elements(&vec![false; hgw], hgw);
            for row in &planes_gray[j] {
                encode_g4_line(&mut bw, &reference, row, hgw);
                reference = changing_elements(row, hgw);
            }
        }
        let body = bw.finish();
        let mut out = header;
        out.extend_from_slice(&body);
        out
    }

    /// Build a text-region segment's data placing `placements` (symbol id, S, T)
    /// in a single strip. `num_symbols` sizes the symbol-id code length.
    fn build_text_region(
        width: u32,
        height: u32,
        placements: &[(usize, i64, i64)],
        num_symbols: usize,
    ) -> Vec<u8> {
        let mut header = Vec::new();
        // Region info: width, height, x=0, y=0, comb op = OR.
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&0u32.to_be_bytes());
        header.extend_from_slice(&0u32.to_be_bytes());
        header.push(0x00);
        // Text region flags: SBHUFF=0, REFINE=0, LOGSBSTRIPS=0 (1 strip),
        // REFCORNER=1 (TOPLEFT), TRANSPOSED=0, SBCOMBOP=0, DEFPIXEL=0,
        // DSOFFSET=0, RTEMPLATE=0.
        let flags: u16 = 1 << 4; // ref corner = TOPLEFT (1)
        header.extend_from_slice(&flags.to_be_bytes());
        header.extend_from_slice(&(placements.len() as u32).to_be_bytes());

        let code_len = {
            let mut bits = 0u32;
            while (1usize << bits) < num_symbols {
                bits += 1;
            }
            bits.max(1)
        };

        let mut enc = MqEncoder::new();
        let mut iadt = IntEnc::new();
        let mut iafs = IntEnc::new();
        let mut iads = IntEnc::new();
        let mut iait = IntEnc::new();
        let mut iaid = IaidEnc::new(code_len);

        // Initial STRIPT: DT0 such that STRIPT = -DT0*strips. Use 0.
        iadt.encode(&mut enc, 0); // initial DT (gives STRIPT = 0)
                                  // First strip: DT advances STRIPT by dt*strips. Use dt=0.
        iadt.encode(&mut enc, 0);
        // DFS: first S = placements[0].S.
        let first_s = placements[0].1;
        iafs.encode(&mut enc, first_s as i32);

        let mut cur_s = first_s;
        for (i, &(id, s, _t)) in placements.iter().enumerate() {
            if i > 0 {
                // IDS spacing from the previous advanced S to this symbol's S.
                let ids = s - cur_s;
                iads.encode(&mut enc, ids as i32);
                cur_s = s;
            }
            // CURT (only if >1 strip; here strips=1 so skip).
            let _ = &mut iait;
            // Symbol id.
            iaid.encode(&mut enc, id as u32);
            // Advance S by symbol width - 1 (handled by decoder identically).
            // We need the symbol width; for these test symbols it is 4.
            cur_s += 4 - 1;
        }
        // End the strip with OOB on IADS.
        iads.encode_oob(&mut enc);

        let body = enc.finish();
        let mut out = header;
        out.extend_from_slice(&body);
        out
    }

    /// Encode a refinement bitmap into an existing MQ encoder, sharing the GR
    /// context array (mirror of `decode_refinement_bitmap`, no TPGRON).
    #[allow(clippy::too_many_arguments)]
    fn encode_refinement_into(
        enc: &mut MqEncoder,
        target: &[Vec<bool>],
        w: usize,
        h: usize,
        reference: &Bitmap,
        dx: i64,
        dy: i64,
        template: u8,
        at: &[(i8, i8); 2],
        gr_cx: &mut [ArithContext],
    ) {
        let mut bm = Bitmap::new(w, h);
        for (y, row) in target.iter().enumerate().take(h) {
            for (x, &want) in row.iter().enumerate().take(w) {
                let ctx =
                    refinement_context(&bm, reference, x as i64, y as i64, dx, dy, template, at)
                        as usize;
                enc.encode(&mut gr_cx[ctx], want as u8);
                bm.set(x, y, want);
            }
        }
    }

    /// Build a REFAGG symbol-dictionary segment with a single new symbol coded as
    /// a refinement (REFAGGNINST = 1) of input symbol `ref_id`, with delta
    /// `(rdx, rdy)`. `total_syms` sizes the symbol-ID code length (input + new).
    fn build_refagg_symbol_dict(
        target: &[Vec<bool>],
        ref_id: u32,
        rdx: i32,
        rdy: i32,
        reference: &Bitmap,
        total_syms: usize,
    ) -> Vec<u8> {
        // Flags: SDHUFF=0, SDREFAGG=1 (bit 1), SDTEMPLATE=0, SDRTEMPLATE=0.
        let flags: u16 = 1 << 1;
        let at: [(i8, i8); 4] = [(3, -1), (-3, -1), (2, -2), (-2, -2)];
        let rat: [(i8, i8); 2] = [(-1, -1), (-1, -1)];
        let h = target.len() as i64;
        let w = target[0].len() as i64;

        let mut header = Vec::new();
        header.extend_from_slice(&flags.to_be_bytes());
        // Generic AT pixels (template 0 → 4 pairs).
        for (ax, ay) in at {
            header.push(ax as u8);
            header.push(ay as u8);
        }
        // Refinement AT pixels (rtemplate 0 → 2 pairs).
        for (ax, ay) in rat {
            header.push(ax as u8);
            header.push(ay as u8);
        }
        header.extend_from_slice(&1u32.to_be_bytes()); // SDNUMEXSYMS = 1
        header.extend_from_slice(&1u32.to_be_bytes()); // SDNUMNEWSYMS = 1

        let code_len = {
            let mut bits = 0u32;
            while (1usize << bits) < total_syms {
                bits += 1;
            }
            bits.max(1)
        };
        let mut enc = MqEncoder::new();
        let mut iadh = IntEnc::new();
        let mut iadw = IntEnc::new();
        let mut iaex = IntEnc::new();
        let mut iaai = IntEnc::new();
        let mut iardx = IntEnc::new();
        let mut iardy = IntEnc::new();
        let mut iaid = IaidEnc::new(code_len);
        let mut gr_cx = vec![ArithContext::default(); 1 << 13];

        // One height class of height h.
        iadh.encode(&mut enc, h as i32);
        // One symbol of width w.
        iadw.encode(&mut enc, w as i32);
        // REFAGG: ninst = 1, then symbol id, rdx, rdy, then refinement bitmap.
        iaai.encode(&mut enc, 1);
        iaid.encode(&mut enc, ref_id);
        iardx.encode(&mut enc, rdx);
        iardy.encode(&mut enc, rdy);
        encode_refinement_into(
            &mut enc, target, w as usize, h as usize, reference, rdx as i64, rdy as i64, 0, &rat,
            &mut gr_cx,
        );
        // OOB ends the height class width list.
        iadw.encode_oob(&mut enc);
        // Export: skip the 1 input symbol, then export the 1 new symbol.
        iaex.encode(&mut enc, 1);
        iaex.encode(&mut enc, 1);

        let body = enc.finish();
        let mut out = header;
        out.extend_from_slice(&body);
        out
    }

    /// Build a Huffman-coded symbol-dictionary segment (uncompressed collective
    /// bitmap) for `symbols` (all the same height). Mirrors
    /// `decode_symbol_dictionary_huffman`: DH via B.4, DW via B.2, BMSIZE=0 via
    /// B.1, the uncompressed height-class bitmap, then export via B.1.
    fn build_huffman_symbol_dict(symbols: &[&Vec<Vec<bool>>]) -> Vec<u8> {
        use huff::{standard_table, BitWriter};
        // Flags: SDHUFF=1 (bit0). All selectors 0 (B.4/B.2/B.1/B.1).
        let flags: u16 = 0x0001;
        let num_ex = symbols.len() as u32;
        let num_new = symbols.len() as u32;
        let mut header = Vec::new();
        header.extend_from_slice(&flags.to_be_bytes());
        // No AT pixels (SDHUFF). num_ex, num_new.
        header.extend_from_slice(&num_ex.to_be_bytes());
        header.extend_from_slice(&num_new.to_be_bytes());

        let dh = standard_table(4).unwrap();
        let dw = standard_table(2).unwrap();
        let bmsize = standard_table(1).unwrap();
        let ex = standard_table(1).unwrap();

        let mut w = BitWriter::new();
        // One height class: all symbols share height H.
        let h = symbols[0].len() as i32;
        dh.encode(&mut w, h);
        // Widths via DW (delta from previous, starting at 0).
        let mut prev_w = 0i32;
        let mut totwidth = 0usize;
        for s in symbols {
            let sw = s[0].len() as i32;
            dw.encode(&mut w, sw - prev_w);
            prev_w = sw;
            totwidth += sw as usize;
        }
        dw.encode_oob(&mut w); // end of height class
        bmsize.encode(&mut w, 0); // BMSIZE = 0 → uncompressed
        w.byte_align();
        // Uncompressed collective bitmap: width = totwidth, height = H, each row
        // byte-aligned. Symbols laid out left to right.
        for y in 0..h as usize {
            let mut x = 0usize;
            for s in symbols {
                for &b in &s[y] {
                    w.put(b as u32, 1);
                    x += 1;
                }
            }
            let _ = x;
            let _ = totwidth;
            w.byte_align();
        }
        // Export flags: skip 0, export all.
        ex.encode(&mut w, 0);
        ex.encode(&mut w, symbols.len() as i32);

        let body = w.finish();
        let mut out = header;
        out.extend_from_slice(&body);
        out
    }

    /// Build a Huffman **+ REFAGG** symbol-dictionary segment with a single new
    /// symbol coded as a single-instance refinement (REFAGGNINST = 1) of input
    /// symbol `ref_id`, with delta `(rdx, rdy)`. Mirrors
    /// `decode_refagg_symbol_huffman`: DH via B.4, DW via B.2, REFAGGNINST via
    /// B.1, the ID as fixed SBSYMCODELEN bits, RDX/RDY via B.15, BMSIZE via B.1,
    /// then the byte-aligned MQ-coded refinement bitmap (BMSIZE bytes), then the
    /// height-class DW OOB and the B.1 export flags. `total_syms` sizes the
    /// symbol-ID code length (input + new).
    fn build_huffman_refagg_symbol_dict(
        target: &[Vec<bool>],
        ref_id: u32,
        rdx: i32,
        rdy: i32,
        reference: &Bitmap,
        total_syms: usize,
    ) -> Vec<u8> {
        use huff::{standard_table, BitWriter};
        // Flags: SDHUFF=1 (bit0), SDREFAGG=1 (bit1). Selectors all 0
        // (DH=B.4, DW=B.2, BMSIZE=B.1, AGGINST=B.1); SDRTEMPLATE=0.
        let flags: u16 = 0x0001 | (1 << 1);
        let rat: [(i8, i8); 2] = [(-1, -1), (-1, -1)];
        let h = target.len() as i64;
        let w = target[0].len() as i64;

        let mut header = Vec::new();
        header.extend_from_slice(&flags.to_be_bytes());
        // No generic AT pixels (SDHUFF=1). Refinement AT pixels (rtemplate 0 → 2).
        for (ax, ay) in rat {
            header.push(ax as u8);
            header.push(ay as u8);
        }
        header.extend_from_slice(&1u32.to_be_bytes()); // SDNUMEXSYMS = 1
        header.extend_from_slice(&1u32.to_be_bytes()); // SDNUMNEWSYMS = 1

        let sym_code_len = {
            let mut bits = 0u32;
            while (1usize << bits) < total_syms {
                bits += 1;
            }
            bits.max(1)
        };

        // Encode the refinement bitmap into a standalone MQ stream so its exact
        // byte length can be carried as BMSIZE (the decoder skips BMSIZE bytes to
        // resume the Huffman codes afterwards).
        let mut renc = MqEncoder::new();
        let mut gr_cx = vec![ArithContext::default(); 1 << 13];
        encode_refinement_into(
            &mut renc, target, w as usize, h as usize, reference, rdx as i64, rdy as i64, 0, &rat,
            &mut gr_cx,
        );
        let refine_bytes = renc.finish();

        let dh = standard_table(4).unwrap();
        let dw = standard_table(2).unwrap();
        let agg = standard_table(1).unwrap(); // SDHUFFAGGINST (REFAGGNINST)
        let rdx_t = standard_table(15).unwrap();
        let rdy_t = standard_table(15).unwrap();
        let rsize = standard_table(1).unwrap(); // BMSIZE
        let ex = standard_table(1).unwrap();

        let mut bw = BitWriter::new();
        // One height class of height h.
        dh.encode(&mut bw, h as i32);
        // One symbol of width w (delta from 0).
        dw.encode(&mut bw, w as i32);
        // REFAGGNINST = 1.
        agg.encode(&mut bw, 1);
        // Symbol ID: fixed SBSYMCODELEN bits.
        bw.put(ref_id, sym_code_len);
        // RDX, RDY (B.15), BMSIZE (B.1).
        rdx_t.encode(&mut bw, rdx);
        rdy_t.encode(&mut bw, rdy);
        rsize.encode(&mut bw, refine_bytes.len() as i32);
        // Byte-align, then the MQ-coded refinement bitmap bytes verbatim.
        bw.byte_align();
        for &b in &refine_bytes {
            bw.put(b as u32, 8);
        }
        // OOB ends the height-class width list.
        dw.encode_oob(&mut bw);
        // Export: skip the 1 input symbol, then export the 1 new symbol.
        ex.encode(&mut bw, 1);
        ex.encode(&mut bw, 1);

        let body = bw.finish();
        let mut out = header;
        out.extend_from_slice(&body);
        out
    }

    /// Build a Huffman-coded text-region segment placing `placements`
    /// (symbol id, S) in a single strip. Mirrors `decode_text_region_huffman`:
    /// the run-code symbol-ID table then FS/DS/DT-coded placements (B.6/B.8/B.11).
    fn build_huffman_text_region(
        width: u32,
        height: u32,
        placements: &[(usize, i64)],
        num_symbols: usize,
        sym_width: i64,
    ) -> Vec<u8> {
        use huff::{encode_symbol_id_table, standard_table, BitWriter};
        let mut header = Vec::new();
        // Region info.
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&0u32.to_be_bytes());
        header.extend_from_slice(&0u32.to_be_bytes());
        header.push(0x00); // comb op = OR
                           // Text-region flags: SBHUFF=1 (bit0), REFCORNER=TOPLEFT (bits4-5 = 1),
                           // everything else 0.
        let flags: u16 = 0x0001 | (1 << 4);
        header.extend_from_slice(&flags.to_be_bytes());
        // Huffman flags halfword: all selectors 0 (B.6/B.8/B.11/B.14.../B.1).
        let huff_flags: u16 = 0;
        header.extend_from_slice(&huff_flags.to_be_bytes());
        // num_instances.
        header.extend_from_slice(&(placements.len() as u32).to_be_bytes());

        let fs = standard_table(6).unwrap();
        let ds = standard_table(8).unwrap();
        let dt = standard_table(11).unwrap();

        let code_len = {
            let mut bits = 0u32;
            while (1usize << bits) < num_symbols {
                bits += 1;
            }
            bits.max(1)
        };
        let mut w = BitWriter::new();
        // Symbol-ID table: every symbol gets code length `code_len`.
        let code_lengths = vec![code_len as u8; num_symbols];
        encode_symbol_id_table(&mut w, &code_lengths);
        // Build the symbol-ID table on this side to obtain each symbol's code.
        let id_table = huff::id_table_from_lengths(&code_lengths);

        // Initial DT then the first strip's DT. The standard SBHUFFDT table
        // (B.11) covers values >= 1, so use DT = 1 for both: STRIPT starts at
        // -1*strips and the strip advances it by +1*strips, landing at T = 0.
        dt.encode(&mut w, 1);
        dt.encode(&mut w, 1);
        // DFS: first S.
        let first_s = placements[0].1;
        fs.encode(&mut w, first_s as i32);

        let mut cur_s = first_s;
        for (i, &(id, s)) in placements.iter().enumerate() {
            if i > 0 {
                let ids = s - cur_s;
                ds.encode(&mut w, ids as i32);
                cur_s = s;
            }
            // Symbol ID (no CURT since 1 strip).
            id_table.encode(&mut w, id as i32);
            cur_s += sym_width - 1;
        }
        // End the strip with OOB on DS.
        ds.encode_oob(&mut w);

        let body = w.finish();
        let mut out = header;
        out.extend_from_slice(&body);
        out
    }

    // --- Test helpers: a minimal CCITT G4 encoder and MQ generic encoder ---

    /// Encode rows into raw G4 (MMR) using vertical/horizontal/pass modes. Only
    /// the subset needed to build test vectors; mirrors the decoder's model.
    fn build_g4(rows: &[&[bool]]) -> Vec<u8> {
        let cols = rows[0].len();
        let mut bw = BitW::default();
        let mut reference: Vec<usize> = changing_elements(&vec![false; cols], cols);
        for row in rows {
            let row_vec: Vec<bool> = row.to_vec();
            encode_g4_line(&mut bw, &reference, &row_vec, cols);
            reference = changing_elements(&row_vec, cols);
        }
        bw.finish()
    }

    fn changing_elements(row: &[bool], cols: usize) -> Vec<usize> {
        let mut ch = Vec::new();
        let mut prev = false;
        for (i, &b) in row.iter().enumerate() {
            if b != prev {
                ch.push(i);
                prev = b;
            }
        }
        ch.push(cols);
        ch
    }

    /// Encode one G4 line using vertical modes where possible, falling back to
    /// horizontal. Sufficient for the simple test patterns.
    fn encode_g4_line(bw: &mut BitW, reference: &[usize], row: &[bool], cols: usize) {
        let cur = changing_elements(row, cols);
        let mut a0: i64 = -1;
        let mut color = false; // white
        let mut ci = 0usize; // index into cur
        loop {
            if a0 >= cols as i64 {
                break;
            }
            let a1 = cur.get(ci).copied().unwrap_or(cols) as i64;
            // find b1, b2
            let (b1, _b2) = find_b1b2_enc(reference, a0, color, cols);
            let diff = a1 - b1 as i64;
            if (-3..=3).contains(&diff) {
                // vertical mode
                match diff {
                    0 => bw.put(0b1, 1),
                    1 => bw.put(0b011, 3),
                    -1 => bw.put(0b010, 3),
                    2 => bw.put(0b000011, 6),
                    -2 => bw.put(0b000010, 6),
                    3 => bw.put(0b0000011, 7),
                    -3 => bw.put(0b0000010, 7),
                    _ => unreachable!(),
                }
                a0 = a1;
                color = !color;
                ci += 1;
            } else {
                // horizontal mode: 001 + run(current colour) + run(opposite).
                // `color` is false for white; `encode_run` takes `white` (true =
                // white), so the current colour maps to `white = !color`.
                bw.put(0b001, 3);
                let start = if a0 < 0 { 0 } else { a0 as usize };
                let a1u = a1 as usize;
                let a2u = cur.get(ci + 1).copied().unwrap_or(cols);
                encode_run(bw, a1u - start, !color);
                encode_run(bw, a2u - a1u, color);
                a0 = a2u as i64;
                ci += 2;
            }
        }
    }

    fn find_b1b2_enc(reference: &[usize], a0: i64, color: bool, cols: usize) -> (usize, usize) {
        let want_black = !color;
        let mut i = 0;
        while i < reference.len() {
            let pos = reference[i];
            let starts_black = i % 2 == 0;
            if pos as i64 > a0 && starts_black == want_black {
                let b1 = pos;
                let b2 = reference.get(i + 1).copied().unwrap_or(cols);
                return (b1, b2);
            }
            i += 1;
        }
        (cols, cols)
    }

    fn encode_run(bw: &mut BitW, mut run: usize, white: bool) {
        // Emit make-up codes then a terminating code, from the decoder's tables.
        // Test patterns stay well under 1728, so a single make-up suffices.
        while run >= 64 {
            let makeup = ((run / 64) * 64).min(1728);
            let (bits, len) = lookup_code(white, makeup);
            bw.put(bits, len);
            run -= makeup;
        }
        let (bits, len) = lookup_code(white, run);
        bw.put(bits, len);
    }

    fn lookup_code(white: bool, run: usize) -> (u32, u32) {
        let table = if white {
            super::super::ccitt::white_codes_for_test()
        } else {
            super::super::ccitt::black_codes_for_test()
        };
        for &(bits, len, r) in table.iter() {
            if r as usize == run {
                return (bits, len);
            }
        }
        panic!("no code for run {run} white={white}");
    }

    #[derive(Default)]
    struct BitW {
        bytes: Vec<u8>,
        nbits: usize,
    }
    impl BitW {
        fn put(&mut self, value: u32, len: u32) {
            for i in (0..len).rev() {
                let bit = ((value >> i) & 1) as u8;
                if self.nbits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if bit == 1 {
                    let idx = self.bytes.len() - 1;
                    self.bytes[idx] |= 0x80 >> (self.nbits % 8);
                }
                self.nbits += 1;
            }
        }
        fn finish(self) -> Vec<u8> {
            self.bytes
        }
    }

    /// A minimal MQ *encoder* mirroring T.88 Annex E, used only to build the
    /// arithmetic generic-region test vector.
    fn encode_generic(
        target: &[Vec<bool>],
        w: usize,
        h: usize,
        template: u8,
        at: &[(i8, i8); 4],
    ) -> Vec<u8> {
        let mut enc = MqEncoder::new();
        let mut bm = Bitmap::new(w, h);
        let mut cx = vec![ArithContext::default(); 1 << 16];
        for (y, row) in target.iter().enumerate().take(h) {
            for (x, &want) in row.iter().enumerate().take(w) {
                let ctx = generic_context(&bm, x as i64, y as i64, template, at) as usize;
                let bit = want as u8;
                enc.encode(&mut cx[ctx], bit);
                bm.set(x, y, bit == 1);
            }
        }
        enc.finish()
    }

    /// Encode an arithmetic generic region with TPGDON typical prediction, the
    /// mirror of the `tpgdon` branch in `decode_generic_bitmap`. Each row first
    /// emits an SLTP bit (toggling LTP); a typical row (identical to the row
    /// above) is coded as LTP=1 with no pixel bits, otherwise every pixel is coded.
    fn encode_generic_tpgdon(
        target: &[Vec<bool>],
        w: usize,
        h: usize,
        template: u8,
        at: &[(i8, i8); 4],
    ) -> Vec<u8> {
        let mut enc = MqEncoder::new();
        let mut bm = Bitmap::new(w, h);
        let mut cx = vec![ArithContext::default(); 1 << 16];
        let sltp_ctx = match template {
            0 => 0x9B25usize,
            1 => 0x0795,
            2 => 0x00E5,
            _ => 0x0195,
        };
        let mut ltp = false;
        for y in 0..h {
            // A row equal to the one above can be coded as "typical".
            let typical = y > 0 && target[y] == target[y - 1];
            // SLTP toggles LTP; emit the bit that makes LTP match `typical`.
            let sltp = (ltp != typical) as u8;
            enc.encode(&mut cx[sltp_ctx], sltp);
            ltp = typical;
            if ltp {
                // Copy the row above; no pixel bits coded.
                for x in 0..w {
                    let v = bm.get(x as i64, y as i64 - 1);
                    bm.set(x, y, v);
                }
                continue;
            }
            for (x, &want) in target[y].iter().enumerate().take(w) {
                let ctx = generic_context(&bm, x as i64, y as i64, template, at) as usize;
                enc.encode(&mut cx[ctx], want as u8);
                bm.set(x, y, want);
            }
        }
        enc.finish()
    }

    /// Encode a refinement region (mirror of `decode_refinement_bitmap`): refine
    /// `reference` (offset `(dx,dy)`) into `target` using GR `template` and AT
    /// pixels `at`. No TPGRON (the round-trip test exercises the coded path).
    #[allow(clippy::too_many_arguments)]
    fn encode_refinement(
        target: &[Vec<bool>],
        w: usize,
        h: usize,
        reference: &Bitmap,
        dx: i64,
        dy: i64,
        template: u8,
        at: &[(i8, i8); 2],
    ) -> Vec<u8> {
        let mut enc = MqEncoder::new();
        let mut bm = Bitmap::new(w, h);
        let mut cx = vec![ArithContext::default(); 1 << 13];
        for (y, row) in target.iter().enumerate().take(h) {
            for (x, &want) in row.iter().enumerate().take(w) {
                let ctx =
                    refinement_context(&bm, reference, x as i64, y as i64, dx, dy, template, at)
                        as usize;
                enc.encode(&mut cx[ctx], want as u8);
                bm.set(x, y, want);
            }
        }
        enc.finish()
    }

    /// Encode a refinement region with TPGRON typical prediction (mirror of the
    /// `tpgron` branch in `decode_refinement_bitmap`): per row, an SLTP bit
    /// toggles LTP; while LTP is on, a pixel whose 3×3 reference neighbourhood is
    /// uniform is predicted (not coded), otherwise it is coded.
    #[allow(clippy::too_many_arguments)]
    fn encode_refinement_tpgron(
        target: &[Vec<bool>],
        w: usize,
        h: usize,
        reference: &Bitmap,
        dx: i64,
        dy: i64,
        template: u8,
        at: &[(i8, i8); 2],
    ) -> Vec<u8> {
        let mut enc = MqEncoder::new();
        let mut bm = Bitmap::new(w, h);
        let mut cx = vec![ArithContext::default(); 1 << 13];
        let ltp_ctx: usize = if template == 0 { 0x0100 } else { 0x0080 };
        // Per row, decide whether typical prediction holds for the WHOLE row (a
        // row is "typical" iff every pixel matches the uniform-neighbourhood
        // prediction wherever the neighbourhood is uniform). For the test we set
        // LTP whenever it keeps the row exactly reproducible.
        let mut ltp = false;
        for y in 0..h as i64 {
            // Determine whether this row can be coded with LTP on (every pixel
            // with a uniform reference neighbourhood equals the predicted value).
            let row_typical = (0..w as i64).all(|x| {
                let rx = x - dx;
                let ry = y - dy;
                let s = neigh_sum(reference, rx, ry);
                if s == 0 {
                    !target[y as usize][x as usize]
                } else if s == 9 {
                    target[y as usize][x as usize]
                } else {
                    true
                }
            });
            let want_ltp = row_typical;
            let sltp = (ltp != want_ltp) as u8;
            enc.encode(&mut cx[ltp_ctx], sltp);
            ltp = want_ltp;
            for x in 0..w as i64 {
                let want = target[y as usize][x as usize];
                if ltp {
                    let rx = x - dx;
                    let ry = y - dy;
                    let s = neigh_sum(reference, rx, ry);
                    if s == 0 {
                        bm.set(x as usize, y as usize, false);
                        continue;
                    } else if s == 9 {
                        bm.set(x as usize, y as usize, true);
                        continue;
                    }
                }
                let ctx = refinement_context(&bm, reference, x, y, dx, dy, template, at) as usize;
                enc.encode(&mut cx[ctx], want as u8);
                bm.set(x as usize, y as usize, want);
            }
        }
        enc.finish()
    }

    /// Sum of the 3×3 reference neighbourhood around `(rx, ry)` (helper for the
    /// TPGRON encoder / decoder uniform-neighbourhood test).
    fn neigh_sum(reference: &Bitmap, rx: i64, ry: i64) -> u32 {
        (reference.get(rx - 1, ry - 1) as u32)
            + (reference.get(rx, ry - 1) as u32)
            + (reference.get(rx + 1, ry - 1) as u32)
            + (reference.get(rx - 1, ry) as u32)
            + (reference.get(rx, ry) as u32)
            + (reference.get(rx + 1, ry) as u32)
            + (reference.get(rx - 1, ry + 1) as u32)
            + (reference.get(rx, ry + 1) as u32)
            + (reference.get(rx + 1, ry + 1) as u32)
    }

    /// MQ arithmetic *encoder* (ITU-T T.88 Annex E.3.1), used only to build the
    /// arithmetic generic-region test vector. Follows the ENCODE → CODEMPS /
    /// CODELPS → RENORME → BYTEOUT → FLUSH flowcharts exactly, with the same Qe
    /// table the decoder uses (via [`mq::qe_entry_for_test`]). The byte stream it
    /// produces is the canonical input the [`MqDecoder`] consumes.
    struct MqEncoder {
        a: u32,
        c: u32,
        ct: i32,
        b: u8,          // current output byte buffer (BP register)
        bp_valid: bool, // whether `b` holds a real byte yet (BP >= 0)
        out: Vec<u8>,
    }
    impl MqEncoder {
        fn new() -> Self {
            // INITENC (E.3.1): A=0x8000, C=0, CT=12, BP points before the buffer.
            MqEncoder {
                a: 0x8000,
                c: 0,
                ct: 12,
                b: 0,
                bp_valid: false,
                out: Vec::new(),
            }
        }
        fn encode(&mut self, cx: &mut ArithContext, d: u8) {
            let (qe, nmps, nlps, switch) = mq::qe_entry_for_test(cx.index);
            self.a = self.a.wrapping_sub(qe);
            if d == cx.mps {
                // CODEMPS.
                if self.a & 0x8000 == 0 {
                    if self.a < qe {
                        self.a = qe;
                        // d stays MPS; index -> NMPS
                        cx.index = nmps;
                    } else {
                        self.c += qe;
                        cx.index = nmps;
                    }
                    self.renorme();
                } else {
                    self.c += qe;
                }
            } else {
                // CODELPS.
                if self.a < qe {
                    self.c += qe;
                } else {
                    self.a = qe;
                }
                if switch == 1 {
                    cx.mps = 1 - cx.mps;
                }
                cx.index = nlps;
                self.renorme();
            }
        }
        fn renorme(&mut self) {
            loop {
                if self.ct == 0 {
                    self.byteout();
                }
                self.a <<= 1;
                self.c <<= 1;
                self.ct -= 1;
                if self.a & 0x8000 != 0 {
                    break;
                }
            }
        }
        fn byteout(&mut self) {
            // BYTEOUT (E.3.1): emit a byte from C, handling carry and bit stuffing.
            if self.b == 0xFF {
                // Previous byte was 0xFF: stuff (emit, then take 7 bits).
                self.emit_b();
                self.b = ((self.c >> 20) & 0xFF) as u8;
                self.c &= 0xF_FFFF;
                self.ct = 7;
            } else if self.c & 0x0800_0000 != 0 {
                // Carry out of bit 27 propagates into the buffered byte.
                let nb = self.b.wrapping_add(1);
                self.b = nb;
                if nb == 0xFF {
                    self.emit_b();
                    self.b = ((self.c >> 20) & 0xFF) as u8;
                    self.c &= 0xF_FFFF;
                    self.ct = 7;
                } else {
                    self.emit_b();
                    self.b = ((self.c >> 19) & 0xFF) as u8;
                    self.c &= 0x7_FFFF;
                    self.ct = 8;
                }
            } else {
                self.emit_b();
                self.b = ((self.c >> 19) & 0xFF) as u8;
                self.c &= 0x7_FFFF;
                self.ct = 8;
            }
        }
        /// Push the buffered byte to the output (skipping the not-yet-valid
        /// initial buffer, matching the BP < start guard in the standard).
        fn emit_b(&mut self) {
            if self.bp_valid {
                self.out.push(self.b);
            }
            self.bp_valid = true;
        }
        fn finish(mut self) -> Vec<u8> {
            // FLUSH (E.3.1): set the trailing bits and emit the last two bytes.
            let tempc = self.c + self.a;
            self.c |= 0xFFFF;
            if self.c >= tempc {
                self.c -= 0x8000;
            }
            self.c <<= self.ct;
            self.byteout();
            self.c <<= self.ct;
            self.byteout();
            self.emit_b();
            // Terminating marker (0xFF 0xAC); the decoder treats >0x8F after 0xFF
            // as end-of-data.
            self.out.push(0xFF);
            self.out.push(0xAC);
            self.out
        }
    }
}
