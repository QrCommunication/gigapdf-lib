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

#[path = "jbig2_mq.rs"]
mod mq;
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
}

impl Jbig2State {
    fn new() -> Self {
        Self {
            page: None,
            page_default_black: false,
            symbol_dicts: std::collections::HashMap::new(),
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
// Symbol dictionary (§6.5)
// ---------------------------------------------------------------------------

/// Decode a symbol-dictionary segment into its exported symbol bitmaps, pulling
/// any input symbols from referred-to dictionaries.
fn decode_symbol_dictionary(
    header: &SegmentHeader,
    data: &[u8],
    state: &Jbig2State,
) -> Option<Vec<Bitmap>> {
    let mut r = Reader::new(data);
    let flags = r.u16()?;
    let sdhuff = flags & 0x0001;
    let sdrefagg = (flags >> 1) & 0x0001;
    let template = ((flags >> 10) & 0x0003) as u8;
    // Huffman-coded and refinement/aggregate symbol dictionaries are not handled
    // by the arithmetic pipeline; skip rather than misdecode.
    if sdhuff != 0 || sdrefagg != 0 {
        return None;
    }
    // Adaptive template pixels for the generic symbol-bitmap decode.
    let mut at: [(i8, i8); 4] = [(0, 0); 4];
    let pairs = if template == 0 { 4 } else { 1 };
    for slot in at.iter_mut().take(pairs) {
        let ax = r.u8().map(|b| b as i8).unwrap_or(0);
        let ay = r.u8().map(|b| b as i8).unwrap_or(0);
        *slot = (ax, ay);
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

    let mut mqd = MqDecoder::new(&r.data[r.pos..]);
    let mut iadh = IntContext::default();
    let mut iadw = IntContext::default();
    let mut iaex = IntContext::default();
    let mut iaai = IntContext::default();
    let mut gb_cx = vec![ArithContext::default(); 1 << 16];

    let mut new_symbols: Vec<Bitmap> = Vec::with_capacity(num_new as usize);
    let mut hc_height: i64 = 0;
    // Decode new symbols grouped into height classes (§6.5.5).
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
                    // IAAI (aggregate count) is read only for refinement/aggregate
                    // coding, which we rejected above; here every symbol is a
                    // plain arithmetic generic bitmap.
                    let _ = &mut iaai;
                    let bm = decode_generic_bitmap(
                        &mut mqd,
                        sym_width as usize,
                        hc_height as usize,
                        template,
                        false,
                        &at,
                        &mut gb_cx,
                    );
                    new_symbols.push(bm);
                }
            }
        }
    }

    // Export flags select which of (input ++ new) symbols are exported
    // (§6.5.10), as a run-length list of alternating exclude/include flags.
    let all: Vec<Bitmap> = input_symbols.into_iter().chain(new_symbols).collect();
    let mut exported: Vec<Bitmap> = Vec::with_capacity(num_ex as usize);
    let mut i = 0usize;
    let mut cur_exported = false;
    while i < all.len() && (exported.len() as u32) < num_ex {
        let run = match iaex.decode(&mut mqd) {
            IntResult::Value(v) if v >= 0 => v as usize,
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
    // If the export run-list was degenerate, fall back to exporting the trailing
    // `num_ex` symbols (a robust approximation that keeps text regions working).
    if exported.is_empty() && !all.is_empty() {
        let start = all.len().saturating_sub(num_ex as usize);
        exported = all[start..].to_vec();
    }
    Some(exported)
}

// ---------------------------------------------------------------------------
// Text region (§6.4)
// ---------------------------------------------------------------------------

/// Decode an immediate text-region segment and composite it onto the page.
fn decode_text_region_segment(header: &SegmentHeader, data: &[u8], state: &mut Jbig2State) {
    let mut r = Reader::new(data);
    let Some(info) = parse_region_info(&mut r) else {
        return;
    };
    let Some(flags) = r.u16() else { return };
    let sbhuff = flags & 0x0001;
    let refine = (flags >> 1) & 0x0001;
    let log_strips = ((flags >> 2) & 0x0003) as u32;
    let strips = 1u32 << log_strips;
    let ref_corner = ((flags >> 4) & 0x0003) as u8;
    let transposed = (flags >> 6) & 0x0001;
    let comb_op_sym = ((flags >> 7) & 0x0003) as u8;
    let def_pixel = (flags >> 9) & 0x0001;
    let ds_offset_raw = (flags >> 10) & 0x001F;
    // sign-extend 5-bit DS offset
    let ds_offset = if ds_offset_raw & 0x10 != 0 {
        (ds_offset_raw as i32) - 32
    } else {
        ds_offset_raw as i32
    };
    let _ = def_pixel;

    // Huffman and refinement/transposed text regions are out of the arithmetic
    // pipeline; skip them (region left blank) rather than misdecode.
    if sbhuff != 0 {
        // A Huffman flags halfword follows; nothing we can decode.
        return;
    }
    if refine != 0 {
        // Refinement AT pixels would follow; refinement not supported.
        return;
    }

    let num_instances = match r.u32() {
        Some(v) => v,
        None => return,
    };
    if num_instances > (1 << 22) {
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
    // Symbol-ID code length = ceil(log2(num_symbols)).
    let sym_count = symbols.len();
    let code_len = {
        let mut bits = 0u32;
        while (1usize << bits) < sym_count {
            bits += 1;
        }
        bits.max(1)
    };

    if info.width == 0 || info.height == 0 || info.width > (1 << 20) {
        return;
    }

    let mut mqd = MqDecoder::new(&r.data[r.pos..]);
    let mut iadt = IntContext::default();
    let mut iafs = IntContext::default();
    let mut iads = IntContext::default();
    let mut iait = IntContext::default();
    let mut iari = IntContext::default();
    let mut iaid = IaidContext::new(code_len);
    let _ = &mut iari;

    let mut region = Bitmap::new(info.width, info.height);

    // §6.4.5 the text-region decoding procedure.
    let mut stript: i64 = match iadt.decode(&mut mqd) {
        IntResult::Value(v) => -(v as i64) * strips as i64,
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
        // DT: advance the strip T coordinate.
        let dt = match iadt.decode(&mut mqd) {
            IntResult::Value(v) => v as i64,
            IntResult::Oob => break,
        };
        stript += dt * strips as i64;

        // DFS: first symbol S coordinate of this strip.
        let dfs = match iafs.decode(&mut mqd) {
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
                // IDS: spacing to the next symbol; OOB ends the strip.
                match iads.decode(&mut mqd) {
                    IntResult::Oob => break,
                    IntResult::Value(ids) => {
                        cur_s += ids as i64 + ds_offset as i64;
                    }
                }
            }
            first_in_strip = false;

            // CURT: per-symbol T offset within the strip.
            let curt = if strips == 1 {
                0
            } else {
                match iait.decode(&mut mqd) {
                    IntResult::Value(v) => v as i64,
                    IntResult::Oob => 0,
                }
            };
            let t = stript + curt;

            // IAID: the symbol id.
            let id = iaid.decode(&mut mqd) as usize;
            guard += 1;
            if guard > guard_max {
                break;
            }
            let Some(&sym) = symbols.get(id) else {
                inst += 1;
                continue;
            };

            place_symbol(
                &mut region,
                sym,
                cur_s,
                t,
                ref_corner,
                transposed != 0,
                comb_op_sym,
            );

            // Advance S past the placed symbol (§6.4.5 step 3c.xi).
            let adv = if transposed != 0 {
                sym.height as i64
            } else {
                sym.width as i64
            };
            cur_s += adv.saturating_sub(1);
            inst += 1;
        }
    }

    composite(state, &region, info.x, info.y, info.comb_op);
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
