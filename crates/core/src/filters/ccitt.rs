//! CCITTFaxDecode (ISO 32000-1 §7.4.6 / ITU-T T.4 & T.6). Pure `std`, zero
//! dependencies.
//!
//! Decodes the bilevel (1 bit-per-pixel) fax encodings used by scanned PDFs:
//!
//! * **Group 3, 1-D** (`/K 0`) — every line is modified-Huffman (MH) run-length
//!   coded (T.4 §2).
//! * **Group 3, 2-D** (`/K > 0`) — each line begins with a tag bit selecting 1-D
//!   or 2-D (READ) coding; at most `K - 1` consecutive lines may be 2-D (T.4 §4).
//! * **Group 4** (`/K < 0`) — every line is 2-D coded against the line above
//!   (T.6, the pure two-dimensional scheme, no tag bits, no EOLs).
//!
//! The 2-D coder works on *changing elements* — the pixels where the colour
//! flips. For the line being decoded `a0` is the reference start, `a1`/`a2` the
//! next two changing elements; on the line above, `b1` is the first changing
//! element right of `a0` with the opposite colour to `a0`, and `b2` the next.
//! The seven vertical modes (`V0`, `VR1..3`, `VL1..3`), the pass mode and the
//! horizontal mode (two MH runs) reconstruct each line from those positions.
//!
//! Output is MSB-first packed rows, each padded to a byte boundary — exactly the
//! layout the image sample path expects for a `/BitsPerComponent 1` image. The
//! internal convention is `0 = white, 1 = black`; the packed output emits
//! `0 = black` so a default `/Decode [0 1]` DeviceGray image renders correctly,
//! and `/BlackIs1 true` flips that to `1 = black`.

use crate::error::{EngineError, Result};
use crate::object::{Dictionary, Object};

/// Decoded `/DecodeParms` for a CCITT stream (ISO 32000-1 Table 11).
#[derive(Clone, Copy, Debug)]
pub struct CcittParams {
    /// `/K`: `< 0` pure 2-D (G4), `0` pure 1-D (G3), `> 0` mixed 1-D/2-D (G3 2-D).
    pub k: i64,
    /// `/Columns`: pixels per row (default 1728).
    pub columns: usize,
    /// `/Rows`: rows to decode; `0` means "until end of data" (default 0).
    pub rows: usize,
    /// `/BlackIs1`: if true, 1 bits are black in the output (default false).
    pub black_is_1: bool,
    /// `/EncodedByteAlign`: pad each coded row to a byte boundary (default false).
    pub encoded_byte_align: bool,
    /// `/EndOfLine`: EOL codes precede each line (default false).
    pub end_of_line: bool,
    /// `/EndOfBlock`: data ends with an end-of-block (RTC/EOFB) pattern
    /// (default true).
    pub end_of_block: bool,
}

impl Default for CcittParams {
    fn default() -> Self {
        Self {
            k: 0,
            columns: 1728,
            rows: 0,
            black_is_1: false,
            encoded_byte_align: false,
            end_of_line: false,
            end_of_block: true,
        }
    }
}

impl CcittParams {
    /// Read CCITT parameters from a `/DecodeParms` dictionary, applying the PDF
    /// defaults for any absent key.
    pub fn from_dict(dict: &Dictionary) -> Self {
        let mut p = Self::default();
        if let Some(v) = dict.get(b"K").and_then(Object::as_i64) {
            p.k = v;
        }
        if let Some(v) = dict.get(b"Columns").and_then(Object::as_i64) {
            if v > 0 {
                p.columns = v as usize;
            }
        }
        if let Some(v) = dict.get(b"Rows").and_then(Object::as_i64) {
            if v >= 0 {
                p.rows = v as usize;
            }
        }
        if let Some(v) = dict.get(b"BlackIs1").and_then(Object::as_bool) {
            p.black_is_1 = v;
        }
        if let Some(v) = dict.get(b"EncodedByteAlign").and_then(Object::as_bool) {
            p.encoded_byte_align = v;
        }
        if let Some(v) = dict.get(b"EndOfLine").and_then(Object::as_bool) {
            p.end_of_line = v;
        }
        if let Some(v) = dict.get(b"EndOfBlock").and_then(Object::as_bool) {
            p.end_of_block = v;
        }
        p
    }
}

fn filter_err(msg: &str) -> EngineError {
    EngineError::Filter(msg.to_string())
}

/// MSB-first bit reader over the coded data, tracking the absolute bit position
/// so callers can byte-align.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // bit position
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Peek the next `n` bits (`n <= 16`) MSB-first without consuming, padding
    /// with zero bits past the end of input. Returns the value left-justified is
    /// *not* done here: the value is right-justified in the low `n` bits.
    fn peek(&self, n: u32) -> u32 {
        let mut v = 0u32;
        for i in 0..n {
            let bit_index = self.pos + i as usize;
            let byte = bit_index / 8;
            let bit = if byte < self.data.len() {
                (self.data[byte] >> (7 - (bit_index % 8))) & 1
            } else {
                0
            };
            v = (v << 1) | bit as u32;
        }
        v
    }

    /// Advance by `n` bits.
    fn advance(&mut self, n: u32) {
        self.pos += n as usize;
    }

    /// Read a single bit, or `None` at end of input.
    fn read_bit(&mut self) -> Option<u8> {
        let byte = self.pos / 8;
        if byte >= self.data.len() {
            return None;
        }
        let bit = (self.data[byte] >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(bit)
    }

    /// Whether the reader is fully past the coded data.
    fn at_end(&self) -> bool {
        self.pos >= self.data.len() * 8
    }

    /// Align the position up to the next byte boundary.
    fn byte_align(&mut self) {
        let rem = self.pos % 8;
        if rem != 0 {
            self.pos += 8 - rem;
        }
    }

    /// Total number of coded bits.
    fn bit_len(&self) -> usize {
        self.data.len() * 8
    }
}

/// Outcome of decoding one run-length code from the bitstream.
enum RunCode {
    /// A run length (terminating, possibly preceded by accumulated make-up runs).
    Run(usize),
    /// An end-of-line code (`000000000001`) was consumed.
    Eol,
    /// No valid code matched — corrupt or exhausted data.
    Error,
}

/// Decode one complete run (make-up codes + a terminating code) of the given
/// colour from `reader`, returning the total run length. `white` selects the
/// MH code table. Make-up codes (>= 64) chain until a terminating code (< 64).
fn decode_run(reader: &mut BitReader, white: bool) -> RunCode {
    let mut total = 0usize;
    loop {
        match decode_one_code(reader, white) {
            RunCode::Run(n) => {
                total += n;
                if n < 64 {
                    return RunCode::Run(total);
                }
                // Make-up code: continue accumulating. A make-up code of exactly
                // 64 still needs a terminating code to follow.
            }
            RunCode::Eol => return RunCode::Eol,
            RunCode::Error => return RunCode::Error,
        }
    }
}

/// Decode a single MH code (one terminating *or* one make-up *or* an EOL) of the
/// given colour, consuming its bits. The MH codes are a prefix code: this walks
/// the bit-length-indexed tables.
fn decode_one_code(reader: &mut BitReader, white: bool) -> RunCode {
    // EOL = 000000000001 (11 zeros then a 1, i.e. the 12-bit code 0x001). Some
    // streams use fill bits (extra zeros) before the 1; that is handled by the
    // caller's explicit EOL scan, but a bare EOL appearing mid-line is detected
    // here too.
    let table = if white { &WHITE_CODES } else { &BLACK_CODES };
    // Try code lengths from shortest to longest (2..=13 for white, 2..=13 for
    // black, plus the shared make-up codes up to 13 bits).
    for &(bits, len, run) in table.iter() {
        let got = reader.peek(len);
        if got == bits {
            reader.advance(len);
            return RunCode::Run(run as usize);
        }
    }
    // Shared (colour-independent) make-up codes for runs 1792..=2560.
    for &(bits, len, run) in SHARED_MAKEUP.iter() {
        let got = reader.peek(len);
        if got == bits {
            reader.advance(len);
            return RunCode::Run(run as usize);
        }
    }
    // EOL: 12-bit 0x001.
    if reader.peek(12) == 0x001 {
        reader.advance(12);
        return RunCode::Eol;
    }
    RunCode::Error
}

/// Skip an EOL code (with optional leading fill bits) at the current position if
/// one is present. Returns true if an EOL was consumed. A T.4 EOL is any number
/// of 0 fill bits followed by `000000000001`.
fn try_consume_eol(reader: &mut BitReader) -> bool {
    let start = reader.pos;
    // Consume leading zero fill bits, but cap the scan to avoid runaway on an
    // all-zero tail.
    let mut zeros = 0usize;
    while zeros < 64 {
        match reader.peek(1) {
            0 => {
                // Look ahead: is this the 11th+ zero followed by a 1 forming an
                // EOL? We detect the full 0*1 pattern by scanning for the 1.
                reader.advance(1);
                zeros += 1;
            }
            _ => break,
        }
    }
    // After the zero run, expect a single 1 to complete the EOL, and require at
    // least 11 leading zeros (the canonical EOL is 11 zeros + 1).
    if zeros >= 11 && reader.peek(1) == 1 {
        reader.advance(1);
        true
    } else {
        // Not an EOL — rewind.
        reader.pos = start;
        false
    }
}

/// Decode a CCITT-coded stream into MSB-first packed 1-bpp rows.
pub fn ccitt_decode(data: &[u8], params: &CcittParams) -> Result<Vec<u8>> {
    if params.columns == 0 {
        return Err(filter_err("CCITT /Columns must be positive"));
    }
    let cols = params.columns;
    let mut reader = BitReader::new(data);
    let row_bytes = cols.div_ceil(8);
    let mut out: Vec<u8> = Vec::new();

    // The reference line: positions of changing elements. A "changing element"
    // is a pixel index where the colour differs from the pixel to its left. The
    // imaginary line above the first row is all white (no changing elements but
    // the sentinels at `cols`).
    let mut ref_changes: Vec<usize> = Vec::new();
    let mut rows_done = 0usize;

    // For pure 2-D (G4) the first line's reference is an all-white line; for
    // mixed/1-D, line tag bits drive per-line mode.
    loop {
        if params.rows != 0 && rows_done >= params.rows {
            break;
        }
        // End conditions: out of data. For decode-until-EOFB streams we also stop
        // on RTC / EOFB below.
        if reader.at_end() {
            break;
        }

        // Optional per-line byte alignment (TIFF-style "EndOfByteAlign").
        if params.encoded_byte_align {
            reader.byte_align();
            if reader.at_end() {
                break;
            }
        }

        // EOL handling. In G3 (K >= 0) each line may be preceded by an EOL; in
        // G4 (K < 0) there are none. We consume an EOL if present regardless,
        // since some G3 streams always emit them.
        if params.end_of_line || params.k >= 0 {
            // Detect an end-of-block (consecutive EOLs / RTC) before a line.
            if detect_rtc(&mut reader, params) {
                break;
            }
            let _ = try_consume_eol(&mut reader);
        }

        // Decide 1-D vs 2-D for this line.
        let two_d = if params.k < 0 {
            true // G4: every line 2-D
        } else if params.k == 0 {
            false // G3 1-D: every line 1-D
        } else {
            // G3 2-D: a tag bit follows the (optional) EOL. 1 = 1-D, 0 = 2-D.
            match reader.read_bit() {
                Some(1) => false,
                Some(_) => true,
                None => break,
            }
        };

        let line = if two_d {
            decode_2d_line(&mut reader, &ref_changes, cols)
        } else {
            decode_1d_line(&mut reader, cols)
        };

        let changes = match line {
            Some(c) => c,
            None => {
                // A decode failure on the very first row of a row-count-known
                // image is a hard error (lets callers fall back / skip); on a
                // decode-until-EOFB stream we stop and keep what we have.
                if rows_done == 0 && params.rows != 0 {
                    return Err(filter_err("CCITT line decode failed"));
                }
                break;
            }
        };

        // Render the changing elements into a packed row and append.
        emit_row(&mut out, &changes, cols, row_bytes, params.black_is_1);
        ref_changes = changes;
        rows_done += 1;

        // Guard against pathological inputs that never terminate.
        if params.rows == 0 && rows_done > (reader.bit_len() + 1) {
            break;
        }
    }

    if rows_done == 0 {
        return Err(filter_err("CCITT produced no rows"));
    }
    Ok(out)
}

/// Detect a Return-To-Control / End-Of-Facsimile-Block marker at the current
/// position (six consecutive EOLs for G3, or the `EOFB` = two EOLs for G4). When
/// `end_of_block` is set this terminates decoding. Non-destructive unless a
/// marker is found.
fn detect_rtc(reader: &mut BitReader, params: &CcittParams) -> bool {
    if !params.end_of_block {
        return false;
    }
    let start = reader.pos;
    // EOFB for G4 is two consecutive EOLs; RTC for G3 is six. We require at least
    // two EOLs to call it a block end.
    let mut count = 0;
    while try_consume_eol(reader) {
        count += 1;
        if count >= 6 {
            break;
        }
    }
    if count >= 2 {
        true
    } else {
        reader.pos = start;
        false
    }
}

/// Decode one 1-D (MH) coded line into the sorted list of changing-element
/// positions. The line starts white. Each decoded run advances the cursor; a
/// changing element is recorded at every colour flip. `None` on a corrupt code.
fn decode_1d_line(reader: &mut BitReader, cols: usize) -> Option<Vec<usize>> {
    let mut changes = Vec::new();
    let mut a0 = 0usize;
    let mut white = true;
    while a0 < cols {
        match decode_run(reader, white) {
            RunCode::Run(run) => {
                let a1 = (a0 + run).min(cols);
                if a1 != a0 || !changes.is_empty() || run != 0 {
                    // Record the transition position (where the *next* colour
                    // begins). A run of 0 still flips colour.
                }
                a0 = a1;
                changes.push(a0);
                white = !white;
            }
            RunCode::Eol => {
                // An EOL inside a line means the line ended early; pad to cols.
                break;
            }
            RunCode::Error => return None,
        }
    }
    // Ensure the line is terminated at `cols`.
    normalize_changes(&mut changes, cols);
    Some(changes)
}

/// Decode one 2-D (READ / T.6) coded line against `ref_changes` (the previous
/// line's changing elements) into this line's changing elements. `None` on a
/// corrupt mode code.
fn decode_2d_line(
    reader: &mut BitReader,
    ref_changes: &[usize],
    cols: usize,
) -> Option<Vec<usize>> {
    let mut changes: Vec<usize> = Vec::new();
    // `a0` starts at -1 conceptually (just left of the first pixel). We model it
    // as an i64 so the "strictly greater than a0" search for b1 works at the
    // line start.
    let mut a0: i64 = -1;
    let mut color_white = true; // colour of the run starting at a0

    loop {
        if a0 >= cols as i64 {
            break;
        }
        let (b1, b2) = find_b1_b2(ref_changes, a0, color_white, cols);
        match decode_mode(reader) {
            Some(Mode::Pass) => {
                // a0' = b2; colour unchanged; no changing element recorded for a0
                // (the run extends through b2).
                a0 = b2 as i64;
            }
            Some(Mode::Horizontal) => {
                // Two runs of the current then opposite colour starting at a0
                // (clamped to >= 0).
                let start = if a0 < 0 { 0 } else { a0 as usize };
                let r1 = match decode_run(reader, color_white) {
                    RunCode::Run(n) => n,
                    _ => return None,
                };
                let r2 = match decode_run(reader, !color_white) {
                    RunCode::Run(n) => n,
                    _ => return None,
                };
                let a1 = (start + r1).min(cols);
                let a2 = (a1 + r2).min(cols);
                changes.push(a1);
                changes.push(a2);
                a0 = a2 as i64;
                // colour is unchanged after an even number of flips.
            }
            Some(Mode::Vertical(delta)) => {
                let a1 = (b1 as i64 + delta).clamp(0, cols as i64) as usize;
                changes.push(a1);
                a0 = a1 as i64;
                color_white = !color_white;
            }
            Some(Mode::Extension) | None => {
                // Unsupported 2-D extension (uncompressed mode) or a corrupt
                // code: bail. Returning what we have lets decode-until-EOFB keep
                // earlier rows.
                return None;
            }
        }
    }
    normalize_changes(&mut changes, cols);
    Some(changes)
}

/// The 2-D coding modes (T.6 Table 1 / T.4 Table 4).
enum Mode {
    Pass,
    Horizontal,
    /// Vertical mode with delta in `-3..=3` (V0, VR1-3, VL1-3).
    Vertical(i64),
    /// 2-D extension (uncompressed) mode — not supported.
    Extension,
}

/// Decode the next 2-D mode code from the bitstream.
fn decode_mode(reader: &mut BitReader) -> Option<Mode> {
    // Mode codes (MSB-first):
    //   V0       : 1
    //   VR1      : 011        VL1      : 010
    //   H        : 001
    //   Pass     : 0001
    //   VR2      : 000011     VL2      : 000010
    //   VR3      : 0000011    VL3      : 0000010
    //   Extension: 0000001xxx
    if reader.peek(1) == 0b1 {
        reader.advance(1);
        return Some(Mode::Vertical(0));
    }
    match reader.peek(3) {
        0b011 => {
            reader.advance(3);
            return Some(Mode::Vertical(1));
        }
        0b010 => {
            reader.advance(3);
            return Some(Mode::Vertical(-1));
        }
        0b001 => {
            reader.advance(3);
            return Some(Mode::Horizontal);
        }
        _ => {}
    }
    if reader.peek(4) == 0b0001 {
        reader.advance(4);
        return Some(Mode::Pass);
    }
    match reader.peek(6) {
        0b000011 => {
            reader.advance(6);
            return Some(Mode::Vertical(2));
        }
        0b000010 => {
            reader.advance(6);
            return Some(Mode::Vertical(-2));
        }
        _ => {}
    }
    match reader.peek(7) {
        0b0000011 => {
            reader.advance(7);
            return Some(Mode::Vertical(3));
        }
        0b0000010 => {
            reader.advance(7);
            return Some(Mode::Vertical(-3));
        }
        0b0000001 => {
            reader.advance(7);
            return Some(Mode::Extension);
        }
        _ => {}
    }
    None
}

/// Find `b1` and `b2` on the reference line for the current `a0`/colour.
///
/// `b1` is the first changing element on the reference line that is strictly to
/// the right of `a0` **and** has the opposite colour to the current colour at
/// `a0`; `b2` is the next changing element after `b1`. Changing elements are at
/// `ref_changes`, where the colour to the *left* of `ref_changes[i]` alternates
/// starting white at position 0 (so `ref_changes[0]` is the first white→black or
/// the first transition). When no such element exists both default to `cols`.
fn find_b1_b2(ref_changes: &[usize], a0: i64, color_white: bool, cols: usize) -> (usize, usize) {
    // The colour to the left of ref_changes[i]:
    //   i even  -> the run that just ended was white  -> ref_changes[i] is a
    //              white→black transition (colour to the right is black)
    // Equivalently, ref_changes[i] has "colour starting at it" = black if i even,
    // white if i odd. b1 must be a changing element whose colour (the colour of
    // the run starting at it) is opposite to `color_white`.
    //
    // We want the first changing element > a0 with colour opposite to a0's
    // colour. A changing element at index i starts a run of colour:
    //   white if i is odd, black if i is even  (line starts white).
    // a0's current colour is `color_white`. We need the element whose starting
    // colour != color_white, i.e. its run colour equals the *opposite* of a0.
    let want_black_run = color_white; // opposite colour to a0
    let mut i = 0;
    while i < ref_changes.len() {
        let pos = ref_changes[i];
        let starts_black = i % 2 == 0;
        if pos as i64 > a0 && starts_black == want_black_run {
            let b1 = pos;
            let b2 = ref_changes.get(i + 1).copied().unwrap_or(cols);
            return (b1, b2);
        }
        i += 1;
    }
    (cols, cols)
}

/// Make sure a changing-element list is strictly increasing, clamped to `cols`,
/// and ends with a sentinel at `cols`. Duplicate/overshoot entries from clamping
/// are collapsed.
fn normalize_changes(changes: &mut Vec<usize>, cols: usize) {
    changes.retain(|&c| c <= cols);
    // Drop a trailing exact-`cols` so we can re-add a single canonical sentinel.
    let mut cleaned: Vec<usize> = Vec::with_capacity(changes.len() + 1);
    let mut last: i64 = -1;
    for &c in changes.iter() {
        if c as i64 > last && c < cols {
            cleaned.push(c);
            last = c as i64;
        }
    }
    cleaned.push(cols);
    *changes = cleaned;
}

/// Emit a packed 1-bpp row from changing elements. The line starts white;
/// `changes[i]` is the pixel index where the i-th colour flip happens. Internal
/// convention: white = 0, black = 1. The output byte stream packs MSB-first;
/// `black_is_1` selects whether black is encoded as 1 (true) or 0 (false, the
/// PDF default so a `/Decode [0 1]` DeviceGray maps 0→black).
fn emit_row(out: &mut Vec<u8>, changes: &[usize], cols: usize, row_bytes: usize, black_is_1: bool) {
    let base = out.len();
    out.resize(base + row_bytes, 0u8);
    // A pixel is emitted as a 1 bit iff its colour matches the "1" colour:
    // black → 1 when `black_is_1`, white → 1 when `!black_is_1`. So the bit is 1
    // exactly when `black == black_is_1`. Walk the runs and set those bits.
    let mut x = 0usize;
    let mut black = false; // the line starts white
    for &c in changes.iter() {
        let end = c.min(cols);
        if black == black_is_1 {
            for px in x..end {
                out[base + px / 8] |= 0x80 >> (px % 8);
            }
        }
        x = end;
        black = !black;
        if x >= cols {
            break;
        }
    }
    // Fill any pixels past the last changing element with the trailing colour.
    if x < cols && black == black_is_1 {
        for px in x..cols {
            out[base + px / 8] |= 0x80 >> (px % 8);
        }
    }
}

/// Decode raw MMR (Group 4 / T.6) coded data into a `width × height` bilevel
/// bitmap of booleans (`true` = black / 1). This is the shared two-dimensional
/// core reused by JBIG2's MMR-coded generic and symbol regions (T.88 §6.2.6 /
/// Annex C), keeping a single implementation of the READ algorithm.
///
/// Unlike [`ccitt_decode`] this works purely on changing elements (no packed
/// byte output, no `/BlackIs1`), produces exactly `height` rows, and never emits
/// EOL/RTC handling (JBIG2 MMR has none). A short/corrupt stream yields whatever
/// rows decoded, padded with all-white rows to `height`.
pub fn mmr_decode_bitmap(data: &[u8], width: usize, height: usize) -> Vec<Vec<bool>> {
    let mut reader = BitReader::new(data);
    let mut rows: Vec<Vec<bool>> = Vec::with_capacity(height);
    let mut ref_changes: Vec<usize> = Vec::new();
    for _ in 0..height {
        let changes = match decode_2d_line(&mut reader, &ref_changes, width) {
            Some(c) => c,
            None => break,
        };
        rows.push(changes_to_row(&changes, width));
        ref_changes = changes;
    }
    while rows.len() < height {
        rows.push(vec![false; width]);
    }
    rows
}

/// Expand a changing-element list into a row of booleans (`true` = black). The
/// line starts white; colour flips at each changing element.
fn changes_to_row(changes: &[usize], width: usize) -> Vec<bool> {
    let mut row = vec![false; width];
    let mut x = 0usize;
    let mut black = false;
    for &c in changes.iter() {
        let end = c.min(width);
        if black {
            for px in row.iter_mut().take(end).skip(x) {
                *px = true;
            }
        }
        x = end;
        black = !black;
        if x >= width {
            break;
        }
    }
    if black && x < width {
        for px in row.iter_mut().take(width).skip(x) {
            *px = true;
        }
    }
    row
}

include!("ccitt_tables.rs");

/// White MH code table, exposed for sibling-module test vector construction.
#[cfg(test)]
pub(crate) fn white_codes_for_test() -> &'static [(u32, u32, u32)] {
    WHITE_CODES
}

/// Black MH code table, exposed for sibling-module test vector construction.
#[cfg(test)]
pub(crate) fn black_codes_for_test() -> &'static [(u32, u32, u32)] {
    BLACK_CODES
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a CCITT bit-encoder for tests: append individual bits MSB-first.
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        nbits: usize,
    }
    impl BitWriter {
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

    /// Look up the white/black terminating MH code for a short run from the
    /// tables (used to build test vectors).
    fn term_code(white: bool, run: usize) -> (u32, u32) {
        let table = if white { &WHITE_CODES } else { &BLACK_CODES };
        for &(bits, len, r) in table.iter() {
            if r as usize == run {
                return (bits, len);
            }
        }
        panic!("no terminating code for run {run} (white={white})");
    }

    #[test]
    fn one_d_simple_runs() {
        // A 1-D line, 8 columns: white 3, black 2, white 3.
        // Codes from the MH table: W3, B2, W3.
        let mut w = BitWriter::default();
        let (c, l) = term_code(true, 3);
        w.put(c, l);
        let (c, l) = term_code(false, 2);
        w.put(c, l);
        let (c, l) = term_code(true, 3);
        w.put(c, l);
        let data = w.finish();

        let params = CcittParams {
            k: 0,
            columns: 8,
            rows: 1,
            end_of_block: false,
            ..Default::default()
        };
        let out = ccitt_decode(&data, &params).unwrap();
        // Row is 1 byte. Default BlackIs1=false → black=0, white=1.
        // Pattern WWW BB WWW = 1 1 1 0 0 1 1 1 = 0b11100111 = 0xE7.
        assert_eq!(out, vec![0xE7]);
    }

    #[test]
    fn one_d_black_is_1() {
        // Same line but BlackIs1 true → black=1, white=0.
        let mut w = BitWriter::default();
        let (c, l) = term_code(true, 3);
        w.put(c, l);
        let (c, l) = term_code(false, 2);
        w.put(c, l);
        let (c, l) = term_code(true, 3);
        w.put(c, l);
        let data = w.finish();

        let params = CcittParams {
            k: 0,
            columns: 8,
            rows: 1,
            black_is_1: true,
            end_of_block: false,
            ..Default::default()
        };
        let out = ccitt_decode(&data, &params).unwrap();
        // WWW BB WWW = 0 0 0 1 1 0 0 0 = 0b00011000 = 0x18.
        assert_eq!(out, vec![0x18]);
    }

    #[test]
    fn one_d_makeup_code_run() {
        // A 1-D line, 100 columns: white 100. A 100-run needs a make-up code
        // (64) followed by a terminating code (36) — validates run accumulation
        // across make-up + terminating codes.
        let mut w = BitWriter::default();
        let (c, l) = term_code(true, 64); // make-up 64
        w.put(c, l);
        let (c, l) = term_code(true, 36); // terminating 36
        w.put(c, l);
        let data = w.finish();

        let params = CcittParams {
            k: 0,
            columns: 100,
            rows: 1,
            end_of_block: false,
            ..Default::default()
        };
        let out = ccitt_decode(&data, &params).unwrap();
        // 100 white pixels with BlackIs1=false → all 1 bits. 100 bits = 13 bytes
        // (104 bits); the first 100 are 1, the trailing 4 pad bits are 0.
        assert_eq!(out.len(), 13);
        // First 96 pixels all white (1); pixels 96..100 white (1), 100..104
        // padding (0) → 0xF0.
        assert_eq!(&out[..12], &[0xFF; 12]);
        assert_eq!(out[12], 0xF0);
    }

    #[test]
    fn two_d_g4_vertical_modes() {
        // Build a 2-line G4 image, 8 columns.
        // Line 0 (reference is all-white): use Horizontal to make W3 B2 W3.
        //   a0=-1, colour white. H code 001, then runs W3, B2. That gives
        //   changes at 3 and 5; the rest white to 8.
        // Line 1: identical to line 0 via V0 at each transition.
        let mut w = BitWriter::default();
        // Line 0: Pass? No — use H. Mode H = 001.
        w.put(0b001, 3);
        let (c, l) = term_code(true, 3);
        w.put(c, l);
        let (c, l) = term_code(false, 2);
        w.put(c, l);
        // After H we are at a0=5, colour white. Need to reach col 8 white.
        // Another H with W3 B0? Simpler: V mode to the imaginary b1 at cols.
        // Use Pass to extend white run to end: but Pass needs b2. With all-white
        // ref and a0=5 colour white, b1 = first opposite-colour change > 5 = cols
        // (8), b2 = cols. Pass sets a0=b2=8. Mode Pass = 0001.
        w.put(0b0001, 4);
        // Line 1: reproduce line 0's transitions with vertical V0.
        // a0=-1 white. b1 = first black-run change on ref line (line0) > -1 = 3.
        // V0 → a1=3, record, colour black. b1 next = 5 (white run start). V0 →
        // a1=5, colour white. b1 next = 8 (cols). V0 → a1=8 -> done.
        w.put(0b1, 1); // V0
        w.put(0b1, 1); // V0
        w.put(0b1, 1); // V0
        let data = w.finish();

        let params = CcittParams {
            k: -1,
            columns: 8,
            rows: 2,
            end_of_block: false,
            ..Default::default()
        };
        let out = ccitt_decode(&data, &params).unwrap();
        // Each row WWW BB WWW = 0xE7 (BlackIs1 false).
        assert_eq!(out, vec![0xE7, 0xE7]);
    }

    #[test]
    fn insufficient_data_errors() {
        // 4 zero bytes, default columns 1728, rows known (1). Cannot fill a row.
        let params = CcittParams {
            k: 0,
            columns: 1728,
            rows: 1,
            end_of_block: false,
            ..Default::default()
        };
        // 0x00000000 → first white code lookup: 8 zero bits = make-up? White
        // 0000000 isn't a valid terminating/make-up prefix that fills a 1728 row.
        let res = ccitt_decode(&[0, 0, 0, 0], &params);
        assert!(res.is_err(), "4 zero bytes must not fill a 1728-col row");
    }

    #[test]
    fn params_from_dict_reads_keys() {
        let mut d = Dictionary::new();
        d.set(b"K".to_vec(), Object::Integer(-1));
        d.set(b"Columns".to_vec(), Object::Integer(1000));
        d.set(b"Rows".to_vec(), Object::Integer(5));
        d.set(b"BlackIs1".to_vec(), Object::Boolean(true));
        let p = CcittParams::from_dict(&d);
        assert_eq!(p.k, -1);
        assert_eq!(p.columns, 1000);
        assert_eq!(p.rows, 5);
        assert!(p.black_is_1);
        assert!(!p.encoded_byte_align);
    }
}
