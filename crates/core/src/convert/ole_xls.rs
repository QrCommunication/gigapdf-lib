//! From-scratch reader for legacy **Excel 97–2003 `.xls`** workbooks
//! (MS-XLS / BIFF8), with zero third-party dependencies.
//!
//! A `.xls` file is an OLE2 compound file (see [`crate::convert::cfb`]) whose
//! `Workbook` stream (BIFF5 used the name `Book`) is a flat sequence of
//! **records**: a `u16` type, a `u16` byte length, then `length` payload bytes.
//! Records longer than `8224` payload bytes are split, the tail carried in
//! `CONTINUE` (0x003C) records; this reader concatenates them transparently.
//!
//! The output is the *same* editable [`Document`] the modern XLSX importer
//! (`xlsx_to_model`) produces — typed [`SheetCell`]s in a rectangular grid,
//! shared strings, per-cell font styling and number format, merge ranges and
//! per-column widths — not a rasterised page. Charts, drawings and pivot
//! caches are **out of scope** (their records are skipped).
//!
//! Every read is length-bounded: a truncated or garbage stream yields whatever
//! parsed so far (or `None` when nothing did), never a panic or an infinite
//! loop.

use crate::convert::style::parse_base_font;
use crate::model::{
    Align, Block, BlockKind, BorderStyle, CellVAlign, CellValue, CharStyle, Document, Margins,
    MergeRange, Page, PageGeometry, Section, Sheet, SheetBlock, SheetCell, SheetRow,
};

// ── BIFF record type codes (the subset this reader understands) ──

const REC_FORMULA: u16 = 0x0006;
const REC_EOF: u16 = 0x000A;
const REC_CONTINUE: u16 = 0x003C;
const REC_FONT: u16 = 0x0031;
const REC_NUMBER: u16 = 0x0203;
const REC_BLANK: u16 = 0x0201;
const REC_LABEL: u16 = 0x0204;
const REC_BOOLERR: u16 = 0x0205;
const REC_STRING: u16 = 0x0207;
const REC_ROW: u16 = 0x0208;
const REC_INDEX: u16 = 0x020B;
const REC_BOF: u16 = 0x0809;
const REC_MULRK: u16 = 0x00BD;
const REC_MULBLANK: u16 = 0x00BE;
const REC_COLINFO: u16 = 0x007D;
const REC_PALETTE: u16 = 0x0092;
const REC_BOUNDSHEET: u16 = 0x0085;
const REC_MERGEDCELLS: u16 = 0x00E5;
const REC_FORMAT: u16 = 0x041E;
const REC_XF: u16 = 0x00E0;
const REC_LABELSST: u16 = 0x00FD;
const REC_RK: u16 = 0x027E;
const REC_SST: u16 = 0x00FC;

/// BIFF substream kind, taken from the `dt` field of a `BOF` record.
const BOF_GLOBALS: u16 = 0x0005;
const BOF_WORKSHEET: u16 = 0x0010;

/// Single record extracted from the stream, with any `CONTINUE` payload already
/// concatenated onto `data`.
struct Record {
    /// Record type code.
    typ: u16,
    /// Concatenated payload (record body + every following `CONTINUE` body).
    data: Vec<u8>,
    /// For records merged from `CONTINUE`s, the byte length of each contributing
    /// payload segment. A BIFF8 unicode string that straddles a `CONTINUE`
    /// boundary re-reads its compression flag at the split, so the decoder must
    /// know exactly where the joins fall. Empty ⇒ a single, un-split record.
    segments: Vec<usize>,
}

/// Cursor over the workbook stream that yields whole [`Record`]s. All slicing is
/// bounds-checked, so a truncated stream simply ends the iteration.
struct RecordReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> RecordReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        RecordReader { buf, pos: 0 }
    }

    /// Peek the type of the record at the current position without consuming it.
    fn peek_type(&self) -> Option<u16> {
        if self.pos + 4 > self.buf.len() {
            return None;
        }
        Some(u16::from_le_bytes([self.buf[self.pos], self.buf[self.pos + 1]]))
    }

    /// Read the next record (folding in any trailing `CONTINUE` records), or
    /// `None` at end-of-stream / on a truncated header.
    fn next(&mut self) -> Option<Record> {
        if self.pos + 4 > self.buf.len() {
            return None;
        }
        let typ = u16::from_le_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        let len = u16::from_le_bytes([self.buf[self.pos + 2], self.buf[self.pos + 3]]) as usize;
        let start = self.pos + 4;
        let end = start.saturating_add(len).min(self.buf.len());
        let mut data = self.buf[start..end].to_vec();
        let mut segments = vec![data.len()];
        self.pos = end;

        // Fold any immediately-following CONTINUE records into this one. A
        // CONTINUE never directly follows BOF/EOF, so this only triggers for
        // genuinely split records (SST, big strings, drawings…).
        while self.peek_type() == Some(REC_CONTINUE) {
            let clen =
                u16::from_le_bytes([self.buf[self.pos + 2], self.buf[self.pos + 3]]) as usize;
            let cstart = self.pos + 4;
            let cend = cstart.saturating_add(clen).min(self.buf.len());
            segments.push(cend - cstart);
            data.extend_from_slice(&self.buf[cstart..cend]);
            self.pos = cend;
            if cend == cstart {
                break; // zero-length CONTINUE: nothing more to gain, avoid spin
            }
        }
        if segments.len() == 1 {
            segments.clear();
        }
        Some(Record { typ, data, segments })
    }
}

// ── Little-endian readers over a record payload (all bounds-checked) ──

fn u8_at(b: &[u8], o: usize) -> Option<u8> {
    b.get(o).copied()
}
fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?]))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes([*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?]))
}
fn f64_at(b: &[u8], o: usize) -> Option<f64> {
    let s = b.get(o..o + 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(f64::from_le_bytes(a))
}

// ── BIFF8 unicode string decoding ──────────────────────────────────────────

/// A reader for a BIFF8 `XLUnicodeRichExtendedString` that may span the
/// `CONTINUE` boundaries of an SST record. The crucial subtlety: when a string's
/// character data is split across a `CONTINUE`, the **first byte of the new
/// segment is a fresh compression flag** (`grbit`), *not* string data. This
/// reader tracks segment boundaries to honour that rule.
struct SstCursor<'a> {
    buf: &'a [u8],
    /// Sorted absolute offsets at which a new payload segment begins (i.e. the
    /// byte right after a `CONTINUE` join). Empty ⇒ no splits.
    breaks: &'a [usize],
    pos: usize,
}

impl<'a> SstCursor<'a> {
    fn new(buf: &'a [u8], breaks: &'a [usize]) -> Self {
        SstCursor { buf, breaks, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn read_u8(&mut self) -> Option<u8> {
        let v = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }

    fn read_u16(&mut self) -> Option<u16> {
        let v = u16_at(self.buf, self.pos)?;
        self.pos += 2;
        Some(v)
    }

    fn read_u32(&mut self) -> Option<u32> {
        let v = u32_at(self.buf, self.pos)?;
        self.pos += 4;
        Some(v)
    }

    /// Is `self.pos` exactly at the start of a continued segment? When `true`,
    /// the next byte is a fresh `grbit` flag for the spilled-over characters.
    fn at_break(&self) -> bool {
        self.breaks.binary_search(&self.pos).is_ok()
    }

    /// Decode one full BIFF8 string starting at the cursor: a 16-bit char count,
    /// then the body. Advances past character data, the rich-text run table and
    /// the far-east extended block.
    fn read_string(&mut self) -> Option<String> {
        let nchars = self.read_u16()? as usize;
        self.read_string_body(nchars)
    }

    /// Decode the body of a string whose character count `nchars` was already
    /// consumed by the caller.
    fn read_string_body(&mut self, nchars: usize) -> Option<String> {
        let mut flags = self.read_u8()?;
        let mut wide = flags & 0x01 != 0; // 0x01: 16-bit chars
        let ext = flags & 0x04 != 0; // 0x04: phonetic (far-east) block follows
        let rich = flags & 0x08 != 0; // 0x08: rich-text run table follows

        let run_count = if rich { self.read_u16()? as usize } else { 0 };
        let ext_size = if ext { self.read_u32()? as usize } else { 0 };

        // Character data: read `nchars` code units, re-reading the compression
        // flag at every CONTINUE boundary that falls *inside* the run.
        let mut units: Vec<u16> = Vec::with_capacity(nchars.min(1 << 20));
        let mut read = 0usize;
        while read < nchars {
            if self.at_break() {
                // A new segment begins: its first byte is a fresh grbit, and a
                // run may switch between compressed and wide here.
                flags = self.read_u8()?;
                wide = flags & 0x01 != 0;
            }
            if wide {
                units.push(self.read_u16()?);
            } else {
                units.push(self.read_u8()? as u16);
            }
            read += 1;
            // Guard against a corrupt count that outruns the buffer.
            if self.remaining() == 0 && read < nchars {
                break;
            }
        }

        // Skip the rich-text run table (each run is 4 bytes: char index + font
        // index) and the far-east extension block — neither carries text we
        // surface. These may also span CONTINUE breaks, but contain no grbit.
        self.skip(run_count.saturating_mul(4));
        self.skip(ext_size);

        Some(String::from_utf16_lossy(&units))
    }

    /// Advance the cursor by `n` bytes, clamped to the buffer end.
    fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.buf.len());
    }
}

/// Parse the shared-string table (`SST`, 0x00FC). Layout: total-count `u32`,
/// unique-count `u32`, then `unique` BIFF8 strings back-to-back, possibly
/// straddling the record's `CONTINUE` joins (encoded in `breaks`).
fn parse_sst(data: &[u8], breaks: &[usize]) -> Vec<String> {
    let unique = match u32_at(data, 4) {
        Some(n) => n as usize,
        None => return Vec::new(),
    };
    // Cap the announced count against the buffer so a corrupt header can't
    // pre-allocate absurdly (1 string costs ≥ 3 bytes).
    let cap = unique.min(data.len().saturating_add(1));
    let mut out = Vec::with_capacity(cap.min(1 << 20));
    let mut cur = SstCursor::new(data, breaks);
    cur.pos = 8;
    for _ in 0..unique {
        match cur.read_string() {
            Some(s) => out.push(s),
            None => break,
        }
    }
    out
}

// ── RK number decoding (the 4 encodings) ─────────────────────────────────────

/// Decode an `RKNumber` (`u32` little-endian). Bit 0: divide-by-100; bit 1:
/// integer (vs the top 30 bits of an IEEE-754 double's mantissa/exponent).
fn decode_rk(rk: u32) -> f64 {
    let div100 = rk & 0x01 != 0;
    let is_int = rk & 0x02 != 0;
    let val = if is_int {
        // Top 30 bits are a signed 30-bit integer (arithmetic shift keeps sign).
        ((rk as i32) >> 2) as f64
    } else {
        // Top 30 bits are the high 30 bits of a 64-bit double; low 34 are zero.
        let bits = ((rk & 0xFFFF_FFFC) as u64) << 32;
        f64::from_bits(bits)
    };
    if div100 {
        val / 100.0
    } else {
        val
    }
}

// ── Globals (workbook-level) tables ──────────────────────────────────────────

/// A FONT record's surfaced attributes (the ones that map onto [`CharStyle`]).
#[derive(Clone)]
struct FontRec {
    name: String,
    /// Height in twips (1/20 pt).
    height_twips: u16,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    /// Palette colour index, resolved against [`Globals::palette`] later.
    color_idx: u16,
}

impl Default for FontRec {
    fn default() -> Self {
        FontRec {
            name: String::new(),
            height_twips: 200, // 10 pt
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            color_idx: 0x7FFF, // automatic
        }
    }
}

/// An XF (extended format) record's surfaced fields.
#[derive(Clone, Default)]
struct XfRec {
    /// Index into the FONT table.
    font_idx: u16,
    /// Index into the FORMAT table (or a built-in id).
    fmt_idx: u16,
    /// Horizontal alignment code (low 3 bits of the alignment byte).
    halign: u8,
    /// Vertical alignment code (bits 4–6 of the alignment byte).
    valign: u8,
    /// Wrap-text flag (bit 3 of the alignment byte).
    wrap: bool,
    /// Whether any of the four borders is present (for [`BorderStyle`]).
    has_border: bool,
    /// Fill foreground palette index when a solid pattern is set, else `None`.
    fill_idx: Option<u16>,
}

/// One worksheet's directory entry, from a `BOUNDSHEET` record.
struct BoundSheet {
    /// Absolute byte offset, in the workbook stream, of this sheet's `BOF`.
    bof_offset: usize,
    name: String,
}

/// Everything collected from the globals substream, consumed when materialising
/// each worksheet.
#[derive(Default)]
struct Globals {
    sst: Vec<String>,
    fonts: Vec<FontRec>,
    xfs: Vec<XfRec>,
    /// Number-format code by format id (built-ins are filled lazily).
    formats: std::collections::HashMap<u16, String>,
    bound_sheets: Vec<BoundSheet>,
    /// Custom palette colours (`RRGGBB` as `[f64;3]`), indexed from palette
    /// index 8 upward per the BIFF convention. Empty ⇒ use the default palette.
    palette: Vec<[f64; 3]>,
}

/// Parse a FONT record body into a [`FontRec`].
fn parse_font(d: &[u8]) -> FontRec {
    let mut f = FontRec::default();
    if let Some(h) = u16_at(d, 0) {
        f.height_twips = h;
    }
    let grbit = u16_at(d, 2).unwrap_or(0);
    f.italic = grbit & 0x02 != 0;
    f.strike = grbit & 0x08 != 0;
    f.color_idx = u16_at(d, 4).unwrap_or(0x7FFF);
    // Bold weight: ≥ 0x02BC (700) is bold; the grbit "bold" bit is legacy.
    let weight = u16_at(d, 6).unwrap_or(400);
    f.bold = weight >= 0x02BC || grbit & 0x01 != 0;
    f.underline = u8_at(d, 10).unwrap_or(0) != 0;
    // Font name: 1-byte char count at offset 14, 1-byte flags at 15, then the
    // (compressed or wide) name characters.
    if let Some(nchars) = u8_at(d, 14) {
        let wide = u8_at(d, 15).map(|fl| fl & 0x01 != 0).unwrap_or(false);
        f.name = read_short_string(d.get(16..).unwrap_or(&[]), nchars as usize, wide);
    }
    f
}

/// Decode a short (1-byte-count) BIFF8 string body of `nchars` code units that
/// does **not** span a CONTINUE (font/format names always fit one record).
fn read_short_string(d: &[u8], nchars: usize, wide: bool) -> String {
    let mut units = Vec::with_capacity(nchars.min(1 << 16));
    let mut o = 0usize;
    for _ in 0..nchars {
        if wide {
            match u16_at(d, o) {
                Some(u) => units.push(u),
                None => break,
            }
            o += 2;
        } else {
            match u8_at(d, o) {
                Some(u) => units.push(u as u16),
                None => break,
            }
            o += 1;
        }
    }
    String::from_utf16_lossy(&units)
}

/// Parse an XF record body into an [`XfRec`].
fn parse_xf(d: &[u8]) -> XfRec {
    let mut xf = XfRec {
        font_idx: u16_at(d, 0).unwrap_or(0),
        fmt_idx: u16_at(d, 2).unwrap_or(0),
        ..XfRec::default()
    };
    // Alignment byte at offset 6: bits 0–2 halign, bit 3 wrap, bits 4–6 valign.
    let align = u8_at(d, 6).unwrap_or(0);
    xf.halign = align & 0x07;
    xf.wrap = align & 0x08 != 0;
    xf.valign = (align >> 4) & 0x07;
    // Border presence: the line-style nibbles live at offsets 10–11 (left/right/
    // top/bottom each a 4-bit style). Any non-zero ⇒ a border exists.
    let b1 = u16_at(d, 10).unwrap_or(0);
    xf.has_border = b1 & 0x7777 != 0;
    // Fill: pattern is bits 10–15 of the u16 at offset 16; the foreground colour
    // index is bits 0–6 of the u16 at offset 18. Pattern 1 = solid fill.
    let pattern = (u16_at(d, 16).unwrap_or(0) >> 10) & 0x3F;
    if pattern == 1 {
        xf.fill_idx = Some(u16_at(d, 18).unwrap_or(0) & 0x7F);
    }
    xf
}

/// Parse a FORMAT record (format id + format string) into `(id, code)`.
fn parse_format(d: &[u8]) -> Option<(u16, String)> {
    let id = u16_at(d, 0)?;
    let nchars = u16_at(d, 2)? as usize;
    let wide = u8_at(d, 4)? & 0x01 != 0;
    let code = read_short_string(d.get(5..)?, nchars, wide);
    Some((id, code))
}

/// Parse a BOUNDSHEET record into a [`BoundSheet`]. The visibility byte (offset
/// 4) is read past but not surfaced — the model keeps every sheet regardless.
fn parse_boundsheet(d: &[u8]) -> Option<BoundSheet> {
    let bof_offset = u32_at(d, 0)? as usize;
    let nchars = u8_at(d, 6)? as usize;
    let wide = u8_at(d, 7)? & 0x01 != 0;
    let name = read_short_string(d.get(8..)?, nchars, wide);
    Some(BoundSheet { bof_offset, name })
}

/// Parse a PALETTE record into custom colours (each entry `RRGGBBxx`).
fn parse_palette(d: &[u8]) -> Vec<[f64; 3]> {
    let count = u16_at(d, 0).unwrap_or(0) as usize;
    let mut out = Vec::with_capacity(count.min(256));
    let mut o = 2usize;
    for _ in 0..count {
        match (u8_at(d, o), u8_at(d, o + 1), u8_at(d, o + 2)) {
            (Some(r), Some(g), Some(b)) => {
                out.push([r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0]);
            }
            _ => break,
        }
        o += 4;
    }
    out
}

/// Resolve a palette colour index to an RGB triple, or `None` for "automatic" /
/// black. Indices `< 8` are the fixed system colours; `8..` index the custom
/// palette (or the BIFF default palette when no `PALETTE` record was present).
fn resolve_color(idx: u16, palette: &[[f64; 3]]) -> Option<[f64; 3]> {
    // 0x7FFF means "automatic", and 0/1/64 are black/auto ⇒ default (black).
    if idx == 0x7FFF || idx == 0 || idx == 1 || idx == 64 {
        return None;
    }
    let table_idx = idx as usize;
    if table_idx >= 8 {
        let off = table_idx - 8;
        if let Some(c) = palette.get(off) {
            return Some(*c);
        }
        return DEFAULT_PALETTE.get(off).copied();
    }
    // System colours 2..=7 (red/green/blue/yellow/magenta/cyan).
    DEFAULT_PALETTE.get(table_idx.saturating_sub(2)).copied()
}

/// The BIFF default colour palette from index 8 onward (the standard colours),
/// as RGB `0.0..=1.0`. Truncated to the common subset; out-of-range indices fall
/// back to "no colour" (default black) rather than panicking.
const DEFAULT_PALETTE: &[[f64; 3]] = &[
    [0.0, 0.0, 0.0],    // 8  black
    [1.0, 1.0, 1.0],    // 9  white
    [1.0, 0.0, 0.0],    // 10 red
    [0.0, 1.0, 0.0],    // 11 green
    [0.0, 0.0, 1.0],    // 12 blue
    [1.0, 1.0, 0.0],    // 13 yellow
    [1.0, 0.0, 1.0],    // 14 magenta
    [0.0, 1.0, 1.0],    // 15 cyan
    [0.5, 0.0, 0.0],    // 16 dark red
    [0.0, 0.5, 0.0],    // 17 dark green
    [0.0, 0.0, 0.5],    // 18 dark blue
    [0.5, 0.5, 0.0],    // 19 olive
    [0.5, 0.0, 0.5],    // 20 purple
    [0.0, 0.5, 0.5],    // 21 teal
    [0.75, 0.75, 0.75], // 22 silver
    [0.5, 0.5, 0.5],    // 23 gray
];

/// Built-in number-format code for a format id when no FORMAT record overrode
/// it. Covers the common built-ins (ISO 29500 / BIFF8); unknown ids ⇒ `None`.
fn builtin_format(id: u16) -> Option<&'static str> {
    Some(match id {
        0 => return None, // "General" ⇒ no explicit code (matches XLSX default)
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        5..=8 => "$#,##0.00",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        12 => "# ?/?",
        13 => "# ??/??",
        14 => "m/d/yyyy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yyyy h:mm",
        37 | 38 => "#,##0",
        39 | 40 => "#,##0.00",
        45 => "mm:ss",
        46 => "[h]:mm:ss",
        47 => "mmss.0",
        48 => "##0.0E+0",
        49 => "@",
        _ => return None,
    })
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Parse a legacy Excel 97–2003 `.xls` (BIFF8, BIFF5 fallback) workbook into the
/// editable [`Document`] model. Returns `None` when `bytes` is not a usable
/// `.xls` (not a compound file, no workbook stream, or no parseable globals).
pub fn xls_to_model(bytes: &[u8]) -> Option<Document> {
    let cfb = crate::convert::cfb::Cfb::open(bytes)?;
    // BIFF8 names the workbook stream "Workbook"; BIFF5/7 used "Book".
    let stream = cfb
        .read_stream("Workbook")
        .or_else(|| cfb.read_stream("Book"))?;

    let globals = parse_globals(&stream)?;
    if globals.bound_sheets.is_empty() {
        return None;
    }

    let mut sheets = Vec::with_capacity(globals.bound_sheets.len());
    for bs in &globals.bound_sheets {
        sheets.push(parse_worksheet(&stream, bs, &globals));
    }

    Some(build_document(sheets))
}

/// The `dt` (substream type) field of a `BOF` record body — at offset 2 in
/// BIFF8 — identifying the substream as globals / worksheet / chart / etc.
/// `None` when the record is too short to carry one.
fn bof_substream(d: &[u8]) -> Option<u16> {
    u16_at(d, 2)
}

/// Read the first (globals) substream and collect the workbook-level tables.
/// Stops at the globals `EOF`; returns `None` if the stream doesn't open with a
/// `BOF`.
fn parse_globals(stream: &[u8]) -> Option<Globals> {
    let mut rr = RecordReader::new(stream);
    let first = rr.next()?;
    // The workbook stream must open with a globals `BOF`. A few real producers
    // omit the substream type, so accept a missing `dt`, but reject a `BOF` that
    // explicitly declares a non-globals substream (e.g. a stray worksheet).
    if first.typ != REC_BOF || bof_substream(&first.data).is_some_and(|dt| dt != BOF_GLOBALS) {
        return None;
    }
    let mut g = Globals::default();
    while let Some(rec) = rr.next() {
        match rec.typ {
            REC_EOF => break,
            REC_SST => g.sst = parse_sst(&rec.data, &seg_breaks(&rec)),
            REC_FONT => {
                g.fonts.push(parse_font(&rec.data));
                // BIFF has no font index 4 (it is reserved); the standard fix is
                // to duplicate after index 3 so later XF font_idx values line up.
                if g.fonts.len() == 4 {
                    let dup = g.fonts[3].clone();
                    g.fonts.push(dup);
                }
            }
            REC_XF => g.xfs.push(parse_xf(&rec.data)),
            REC_FORMAT => {
                if let Some((id, code)) = parse_format(&rec.data) {
                    g.formats.insert(id, code);
                }
            }
            REC_BOUNDSHEET => {
                if let Some(bs) = parse_boundsheet(&rec.data) {
                    g.bound_sheets.push(bs);
                }
            }
            REC_PALETTE => g.palette = parse_palette(&rec.data),
            _ => {} // charts, drawings, defined names, etc.: skipped
        }
    }
    Some(g)
}

/// Absolute offsets where each CONTINUE-joined segment begins inside a record's
/// concatenated `data`. Empty for un-split records.
fn seg_breaks(rec: &Record) -> Vec<usize> {
    if rec.segments.is_empty() {
        return Vec::new();
    }
    let mut breaks = Vec::with_capacity(rec.segments.len().saturating_sub(1));
    let mut acc = 0usize;
    for (i, seg) in rec.segments.iter().enumerate() {
        if i > 0 {
            breaks.push(acc);
        }
        acc += seg;
    }
    breaks
}

/// A cell parsed from a worksheet substream, before it is placed into the grid.
struct ParsedCell {
    row: usize,
    col: usize,
    /// XF index for this cell (used to resolve style + number format).
    xf: u16,
    value: CellValue,
}

/// Parse one worksheet substream (from its `BOF` to the matching `EOF`) into a
/// fully-styled [`Sheet`].
fn parse_worksheet(stream: &[u8], bs: &BoundSheet, g: &Globals) -> Sheet {
    let mut sheet = Sheet { name: bs.name.clone(), ..Sheet::default() };

    // Seek to this sheet's BOF; a corrupt offset yields an empty (named) sheet.
    if bs.bof_offset >= stream.len() {
        return sheet;
    }
    let mut rr = RecordReader::new(stream);
    rr.pos = bs.bof_offset;
    match rr.next() {
        // A worksheet substream BOF (accept a missing/loose `dt`); a BOF that
        // explicitly declares a different substream isn't a worksheet.
        Some(r)
            if r.typ == REC_BOF
                && bof_substream(&r.data).is_none_or(|dt| dt == BOF_WORKSHEET) => {}
        _ => return sheet,
    }

    let mut cells: Vec<ParsedCell> = Vec::new();
    let mut col_w: std::collections::BTreeMap<usize, f64> = std::collections::BTreeMap::new();
    let mut row_h: std::collections::BTreeMap<usize, f64> = std::collections::BTreeMap::new();
    // A FORMULA whose cached result is a string is followed by a STRING record;
    // remember which cell is awaiting it.
    let mut pending_string: Option<(usize, usize, u16)> = None;

    while let Some(rec) = rr.next() {
        match rec.typ {
            REC_EOF => break,
            REC_LABELSST => {
                if let Some((row, col, xf, sst_idx)) = parse_labelsst(&rec.data) {
                    let text = g.sst.get(sst_idx).cloned().unwrap_or_default();
                    cells.push(ParsedCell { row, col, xf, value: CellValue::Text(text) });
                }
            }
            REC_LABEL => {
                if let Some(pc) = parse_label(&rec.data) {
                    cells.push(pc);
                }
            }
            REC_NUMBER => {
                if let Some((row, col, xf, n)) = parse_number(&rec.data) {
                    cells.push(ParsedCell { row, col, xf, value: CellValue::Number(n) });
                }
            }
            REC_RK => {
                if let Some((row, col, xf, n)) = parse_rk(&rec.data) {
                    cells.push(ParsedCell { row, col, xf, value: CellValue::Number(n) });
                }
            }
            REC_MULRK => parse_mulrk(&rec.data, &mut cells),
            REC_BLANK => {
                if let Some((row, col, xf)) = parse_blank(&rec.data) {
                    cells.push(ParsedCell { row, col, xf, value: CellValue::Empty });
                }
            }
            REC_MULBLANK => parse_mulblank(&rec.data, &mut cells),
            REC_BOOLERR => {
                if let Some((row, col, xf, value)) = parse_boolerr(&rec.data) {
                    cells.push(ParsedCell { row, col, xf, value });
                }
            }
            REC_FORMULA => {
                if let Some((row, col, xf, value, awaits_string)) = parse_formula(&rec.data) {
                    if awaits_string {
                        pending_string = Some((row, col, xf));
                    } else {
                        cells.push(ParsedCell { row, col, xf, value });
                    }
                }
            }
            REC_STRING => {
                if let Some((row, col, xf)) = pending_string.take() {
                    let text = parse_string_record(&rec.data, &seg_breaks(&rec));
                    cells.push(ParsedCell { row, col, xf, value: CellValue::Text(text) });
                }
            }
            REC_MERGEDCELLS => parse_mergedcells(&rec.data, &mut sheet.merges),
            REC_COLINFO => parse_colinfo(&rec.data, &mut col_w),
            REC_ROW => {
                if let Some((row, Some(h))) = parse_row(&rec.data) {
                    row_h.insert(row, h);
                }
            }
            REC_INDEX => {} // row-block index: not needed (we stream records)
            _ => {} // drawings/charts/conditional formats/etc.: skipped
        }
    }

    sheet.rows = build_rows(&cells, &row_h, g);
    sheet.col_widths = finalize_col_widths(&col_w);
    sheet
}

// ── Cell-record parsers (each returns row/col/xf + value) ────────────────────

fn parse_labelsst(d: &[u8]) -> Option<(usize, usize, u16, usize)> {
    let row = u16_at(d, 0)? as usize;
    let col = u16_at(d, 2)? as usize;
    let xf = u16_at(d, 4)?;
    let sst_idx = u32_at(d, 6)? as usize;
    Some((row, col, xf, sst_idx))
}

fn parse_label(d: &[u8]) -> Option<ParsedCell> {
    let row = u16_at(d, 0)? as usize;
    let col = u16_at(d, 2)? as usize;
    let xf = u16_at(d, 4)?;
    // Legacy LABEL holds an inline BIFF8 string (16-bit count at offset 6).
    let nchars = u16_at(d, 6)? as usize;
    let wide = u8_at(d, 8)? & 0x01 != 0;
    let text = read_short_string(d.get(9..)?, nchars, wide);
    Some(ParsedCell { row, col, xf, value: CellValue::Text(text) })
}

fn parse_number(d: &[u8]) -> Option<(usize, usize, u16, f64)> {
    let row = u16_at(d, 0)? as usize;
    let col = u16_at(d, 2)? as usize;
    let xf = u16_at(d, 4)?;
    let n = f64_at(d, 6)?;
    Some((row, col, xf, n))
}

fn parse_rk(d: &[u8]) -> Option<(usize, usize, u16, f64)> {
    let row = u16_at(d, 0)? as usize;
    let col = u16_at(d, 2)? as usize;
    let xf = u16_at(d, 4)?;
    let rk = u32_at(d, 6)?;
    Some((row, col, xf, decode_rk(rk)))
}

/// `MULRK`: row, first col, then `n` `(xf, rk)` 6-byte cells, then a last-col
/// `u16`. The cell count is derived from the body length.
fn parse_mulrk(d: &[u8], out: &mut Vec<ParsedCell>) {
    let (row, first_col) = match (u16_at(d, 0), u16_at(d, 2)) {
        (Some(r), Some(c)) => (r as usize, c as usize),
        _ => return,
    };
    if d.len() < 6 {
        return;
    }
    // Trailing 2 bytes are last_col; the middle is 6 bytes per cell.
    let body = &d[4..d.len() - 2];
    let n = body.len() / 6;
    for i in 0..n {
        let base = i * 6;
        let (xf, rk) = match (u16_at(body, base), u32_at(body, base + 2)) {
            (Some(x), Some(r)) => (x, r),
            _ => break,
        };
        out.push(ParsedCell {
            row,
            col: first_col + i,
            xf,
            value: CellValue::Number(decode_rk(rk)),
        });
    }
}

fn parse_blank(d: &[u8]) -> Option<(usize, usize, u16)> {
    let row = u16_at(d, 0)? as usize;
    let col = u16_at(d, 2)? as usize;
    let xf = u16_at(d, 4)?;
    Some((row, col, xf))
}

/// `MULBLANK`: row, first col, then `n` XF `u16`s, then last-col `u16`.
fn parse_mulblank(d: &[u8], out: &mut Vec<ParsedCell>) {
    let (row, first_col) = match (u16_at(d, 0), u16_at(d, 2)) {
        (Some(r), Some(c)) => (r as usize, c as usize),
        _ => return,
    };
    if d.len() < 6 {
        return;
    }
    let body = &d[4..d.len() - 2];
    let n = body.len() / 2;
    for i in 0..n {
        let xf = match u16_at(body, i * 2) {
            Some(v) => v,
            None => break,
        };
        out.push(ParsedCell { row, col: first_col + i, xf, value: CellValue::Empty });
    }
}

/// `BOOLERR`: row, col, xf, value byte, then a flag (`0` ⇒ boolean, `1` ⇒ error).
/// Errors surface as [`CellValue::Empty`] (no error type in the model).
fn parse_boolerr(d: &[u8]) -> Option<(usize, usize, u16, CellValue)> {
    let row = u16_at(d, 0)? as usize;
    let col = u16_at(d, 2)? as usize;
    let xf = u16_at(d, 4)?;
    let val = u8_at(d, 6)?;
    let is_err = u8_at(d, 7)? != 0;
    let value = if is_err {
        CellValue::Empty
    } else {
        CellValue::Bool(val != 0)
    };
    Some((row, col, xf, value))
}

/// `FORMULA`: row, col, xf, then an 8-byte cached result. When the trailing two
/// bytes are `0xFFFF`, the leading bytes encode the result *type*; a result type
/// of `0` means a following `STRING` record carries the text. Returns
/// `(row, col, xf, value, awaits_string)`.
fn parse_formula(d: &[u8]) -> Option<(usize, usize, u16, CellValue, bool)> {
    let row = u16_at(d, 0)? as usize;
    let col = u16_at(d, 2)? as usize;
    let xf = u16_at(d, 4)?;
    let result = d.get(6..14)?;
    // Special-value marker: result[6..8] == 0xFFFF ⇒ not a plain double.
    if u16_at(result, 6) == Some(0xFFFF) {
        return match result[0] {
            0x00 => Some((row, col, xf, CellValue::Empty, true)), // string follows
            0x01 => Some((row, col, xf, CellValue::Bool(result[2] != 0), false)),
            _ => Some((row, col, xf, CellValue::Empty, false)), // 0x02 error, 0x03 blank
        };
    }
    // Otherwise the 8 bytes are an IEEE-754 number.
    let n = f64_at(result, 0)?;
    Some((row, col, xf, CellValue::Number(n), false))
}

/// `STRING` (cached formula text): a BIFF8 string body that may span CONTINUE.
fn parse_string_record(d: &[u8], breaks: &[usize]) -> String {
    let mut cur = SstCursor::new(d, breaks);
    // BIFF8 STRING uses a u16 char count (BIFF5 used u8).
    cur.read_string().unwrap_or_default()
}

/// `MERGEDCELLS`: count `u16`, then `count` `(r0, r1, c0, c1)` quads (each `u16`).
fn parse_mergedcells(d: &[u8], out: &mut Vec<MergeRange>) {
    let count = match u16_at(d, 0) {
        Some(c) => c as usize,
        None => return,
    };
    let mut o = 2usize;
    for _ in 0..count {
        match (u16_at(d, o), u16_at(d, o + 2), u16_at(d, o + 4), u16_at(d, o + 6)) {
            (Some(r0), Some(r1), Some(c0), Some(c1)) => {
                out.push(MergeRange {
                    r0: r0 as usize,
                    c0: c0 as usize,
                    r1: r1 as usize,
                    c1: c1 as usize,
                });
            }
            _ => break,
        }
        o += 8;
    }
}

/// `COLINFO`: first col, last col, width (1/256 char), … . Width converts to
/// points exactly as the XLSX importer does: `chars * 7.0`, and BIFF8 chars are
/// `width / 256`. So `points = width / 256 * 7`.
fn parse_colinfo(d: &[u8], col_w: &mut std::collections::BTreeMap<usize, f64>) {
    let (first, last, width) = match (u16_at(d, 0), u16_at(d, 2), u16_at(d, 4)) {
        (Some(f), Some(l), Some(w)) => (f as usize, l as usize, w as f64),
        _ => return,
    };
    // Guard a corrupt `last < first` or an absurd span.
    if last < first || last - first > 1 << 16 {
        return;
    }
    let pts = width / 256.0 * 7.0;
    for c in first..=last {
        col_w.insert(c, pts);
    }
}

/// `ROW`: row index, first/last col, then height in twips (low 15 bits; bit 15
/// is the "default height" flag). Returns `(row, Some(points))` when a custom
/// height is explicitly set (option-flags bit 6).
fn parse_row(d: &[u8]) -> Option<(usize, Option<f64>)> {
    let row = u16_at(d, 0)? as usize;
    let twips = (u16_at(d, 6)? & 0x7FFF) as f64;
    let flags = u16_at(d, 12).unwrap_or(0);
    let custom = flags & 0x40 != 0;
    if custom && twips > 0.0 {
        Some((row, Some(twips / 20.0)))
    } else {
        Some((row, None))
    }
}

// ── Grid assembly + styling ──────────────────────────────────────────────────

/// Lay the parsed cells out as a rectangular grid of [`SheetRow`]s, padding gaps
/// with default cells (mirroring `xlsx_sheet_model`). Applies per-row heights
/// and resolves each cell's XF into a [`SheetCell`] style + number format.
fn build_rows(
    cells: &[ParsedCell],
    row_h: &std::collections::BTreeMap<usize, f64>,
    g: &Globals,
) -> Vec<SheetRow> {
    let last_row = match (cells.iter().map(|c| c.row).max(), row_h.keys().last()) {
        (Some(c), Some(r)) => c.max(*r),
        (Some(c), None) => c,
        (None, Some(r)) => *r,
        (None, None) => return Vec::new(),
    };

    // Per-row map of col → cell, so later records overwrite earlier duplicates.
    let mut grid: Vec<std::collections::BTreeMap<usize, &ParsedCell>> =
        vec![std::collections::BTreeMap::new(); last_row + 1];
    for c in cells {
        if c.row < grid.len() {
            grid[c.row].insert(c.col, c);
        }
    }

    let mut rows = Vec::with_capacity(last_row + 1);
    for (r, row_map) in grid.iter().enumerate() {
        let cells_vec = match row_map.keys().last().copied() {
            Some(max_col) => {
                let mut v = Vec::with_capacity(max_col + 1);
                for col in 0..=max_col {
                    match row_map.get(&col) {
                        Some(pc) => v.push(materialize_cell(pc, g)),
                        None => v.push(SheetCell::default()),
                    }
                }
                v
            }
            None => Vec::new(),
        };
        rows.push(SheetRow { cells: cells_vec, height: row_h.get(&r).copied() });
    }
    rows
}

/// Resolve a [`ParsedCell`]'s XF index into a fully-styled [`SheetCell`].
fn materialize_cell(pc: &ParsedCell, g: &Globals) -> SheetCell {
    let mut cell = SheetCell { value: pc.value.clone(), ..SheetCell::default() };

    let Some(xf) = g.xfs.get(pc.xf as usize) else {
        return cell; // unknown XF ⇒ plain cell (still valid)
    };

    // Font → CharStyle.
    if let Some(font) = g.fonts.get(xf.font_idx as usize) {
        cell.style = char_style_from_font(font, &g.palette);
    }

    cell.number_format = resolve_format(xf.fmt_idx, g);
    cell.align = halign_to_model(xf.halign);
    cell.vertical_align = valign_to_model(xf.valign);
    cell.wrap = xf.wrap;

    if let Some(idx) = xf.fill_idx {
        cell.fill = resolve_color(idx, &g.palette);
    }

    // Border (a single uniform style; BIFF8 per-edge styles collapse to one
    // model border, matching the XLSX importer's single `BorderStyle`).
    if xf.has_border {
        cell.border = Some(BorderStyle { width: 1.0, color: [0.0, 0.0, 0.0] });
    }

    cell
}

/// Build a [`CharStyle`] from a FONT record, resolving its palette colour and
/// classifying its family via the shared [`parse_base_font`] helper so the
/// generic fallback matches the rest of the engine.
fn char_style_from_font(font: &FontRec, palette: &[[f64; 3]]) -> CharStyle {
    let family = if font.name.is_empty() {
        "Calibri".to_string()
    } else {
        font.name.clone()
    };
    let generic = parse_base_font(&family).generic;
    CharStyle {
        family,
        generic,
        size_pt: font.height_twips as f64 / 20.0,
        bold: font.bold,
        italic: font.italic,
        underline: font.underline,
        strike: font.strike,
        color: resolve_color(font.color_idx, palette),
        background: None,
        vertical_align: crate::model::VAlign::Baseline,
        ..Default::default()
    }
}

/// Resolve a format id to its code string: a FORMAT-record override first, then
/// a built-in. `None` ⇒ "General" (no explicit code), matching the XLSX path.
fn resolve_format(fmt_idx: u16, g: &Globals) -> Option<String> {
    if let Some(code) = g.formats.get(&fmt_idx) {
        return Some(code.clone());
    }
    builtin_format(fmt_idx).map(str::to_string)
}

/// Map a BIFF horizontal-alignment code to the model's [`Align`]. Codes: 0
/// general, 1 left, 2 centre, 3 right, 4 fill, 5 justify, 6 centre-across.
/// `0` (general) ⇒ `None` (the suite's default), matching XLSX.
fn halign_to_model(code: u8) -> Option<Align> {
    match code {
        1 => Some(Align::Left),
        2 | 6 => Some(Align::Center),
        3 => Some(Align::Right),
        5 => Some(Align::Justify),
        _ => None,
    }
}

/// Map a BIFF vertical-alignment code to the model's [`CellVAlign`]. Codes: 0
/// top, 1 centre, 2 bottom, 3 justify. `2` (bottom, the spreadsheet default) ⇒
/// `None`, matching the XLSX importer's "default = bottom" convention.
fn valign_to_model(code: u8) -> Option<CellVAlign> {
    match code {
        0 => Some(CellVAlign::Top),
        1 => Some(CellVAlign::Middle),
        _ => None, // 2 = bottom (implicit default), 3 = justify ⇒ default
    }
}

/// Finalise per-column widths into a dense vec, trimming trailing zeros exactly
/// as `xlsx_sheet_model` does.
fn finalize_col_widths(col_w: &std::collections::BTreeMap<usize, f64>) -> Vec<f64> {
    let mut widths: Vec<f64> = match col_w.keys().last().copied() {
        Some(highest) => {
            let mut v = vec![0.0; highest + 1];
            for (c, w) in col_w {
                if *c < v.len() {
                    v[*c] = *w;
                }
            }
            v
        }
        None => Vec::new(),
    };
    while matches!(widths.last(), Some(w) if *w == 0.0) {
        widths.pop();
    }
    widths
}

/// Wrap the parsed sheets in a single-page, single-block [`Document`], using the
/// same A4-landscape "tabular" geometry as the XLSX importer (`flow_document` +
/// `page_geometry(PageGeom::tabular_default())`).
fn build_document(sheets: Vec<Sheet>) -> Document {
    // A4 landscape, 0.5in (36 pt) uniform margins — the XLSX/ODS tabular default.
    const A4_W: f64 = 595.276;
    const A4_H: f64 = 841.890;
    const MARGIN: f64 = 36.0;
    let geometry = PageGeometry {
        width: A4_H,
        height: A4_W,
        margins: Margins { top: MARGIN, right: MARGIN, bottom: MARGIN, left: MARGIN },
        column_count: 1,
    };
    let block = Block {
        kind: BlockKind::Sheet(SheetBlock { sheets }),
        ..Block::default()
    };
    Document {
        sections: vec![Section {
            geometry,
            header: None,
            footer: None,
            pages: vec![Page { blocks: vec![block], absolute: false }],
        }],
        ..Document::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::style::Generic;

    // ── A minimal in-memory CFB builder, mirroring cfb.rs's test helper ──
    //
    // Lays out a v3 (512-byte sector) compound file holding a single regular
    // (≥ 4096-byte) stream named by the caller. That is enough to round-trip a
    // hand-built `Workbook` BIFF8 stream through `Cfb::open` + `read_stream`.

    const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    const FREESECT: u32 = 0xFFFF_FFFF;
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FATSECT: u32 = 0xFFFF_FFFD;
    const NOSTREAM: u32 = 0xFFFF_FFFF;
    const OBJ_STREAM: u8 = 2;
    const OBJ_ROOT: u8 = 5;
    const HEADER_DIFAT_OFFSET: usize = 76;
    const HEADER_DIFAT_COUNT: usize = 109;

    fn put_u16(buf: &mut [u8], off: usize, v: u16) {
        buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u32(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    #[allow(clippy::too_many_arguments)]
    fn dir_entry(
        dir: &mut [u8],
        i: usize,
        name: &str,
        obj_type: u8,
        left: u32,
        right: u32,
        child: u32,
        start: u32,
        size: u64,
    ) {
        let base = i * 128;
        let mut nlen = 0usize;
        for (k, ch) in name.encode_utf16().enumerate() {
            put_u16(dir, base + k * 2, ch);
            nlen = (k + 1) * 2;
        }
        nlen += 2;
        put_u16(dir, base + 64, nlen as u16);
        dir[base + 66] = obj_type;
        dir[base + 67] = 1;
        put_u32(dir, base + 68, left);
        put_u32(dir, base + 72, right);
        put_u32(dir, base + 76, child);
        put_u32(dir, base + 116, start);
        put_u32(dir, base + 120, (size & 0xFFFF_FFFF) as u32);
        put_u32(dir, base + 124, (size >> 32) as u32);
    }

    /// Build a CFB whose single stream `name` carries `payload`, zero-padded to a
    /// whole number of 512-byte sectors and ≥ the 4096-byte mini-stream cutoff so
    /// it lives in the regular FAT (the simplest path). The padded length is the
    /// recorded size; BIFF parsing stops at the substream `EOF` records, so the
    /// trailing zero padding is never reached.
    fn build_cfb(name: &str, payload: &[u8]) -> Vec<u8> {
        let min_len = payload.len().max(4096);
        let nsectors = min_len.div_ceil(512);
        let padded_len = nsectors * 512;
        let mut data = payload.to_vec();
        data.resize(padded_len, 0);

        // Layout: sector 0 FAT, 1 directory, then `nsectors` of stream payload.
        let stream_first = 2u32;
        let total = 2 + nsectors;
        let mut sectors = vec![[0u8; 512]; total];

        // FAT (sector 0).
        {
            let fat = &mut sectors[0];
            put_u32(fat, 0, FATSECT); // sector 0 is the FAT
            put_u32(fat, 4, ENDOFCHAIN); // sector 1 directory
            for k in 0..nsectors {
                let sec = stream_first + k as u32;
                let next = if k + 1 < nsectors { sec + 1 } else { ENDOFCHAIN };
                put_u32(fat, sec as usize * 4, next);
            }
            for sec in total..(512 / 4) {
                put_u32(fat, sec * 4, FREESECT);
            }
        }

        // Directory (sector 1): Root (no mini stream) + the stream.
        {
            let dir = &mut sectors[1];
            dir_entry(dir, 0, "Root Entry", OBJ_ROOT, NOSTREAM, NOSTREAM, 1, ENDOFCHAIN, 0);
            dir_entry(
                dir,
                1,
                name,
                OBJ_STREAM,
                NOSTREAM,
                NOSTREAM,
                NOSTREAM,
                stream_first,
                padded_len as u64,
            );
        }

        // Stream payload (sectors 2..).
        for k in 0..nsectors {
            sectors[stream_first as usize + k].copy_from_slice(&data[k * 512..(k + 1) * 512]);
        }

        // Assemble + header.
        let mut bytes = vec![0u8; 512 + total * 512];
        for (i, sec) in sectors.iter().enumerate() {
            bytes[512 + i * 512..512 + (i + 1) * 512].copy_from_slice(sec);
        }
        bytes[0..8].copy_from_slice(&SIGNATURE);
        put_u16(&mut bytes, 24, 0x0003); // major version 3
        put_u16(&mut bytes, 26, 0x003E);
        put_u16(&mut bytes, 28, 0xFFFE); // byte order
        put_u16(&mut bytes, 30, 0x0009); // 512-byte sectors
        put_u16(&mut bytes, 32, 0x0006); // 64-byte mini sectors
        put_u32(&mut bytes, 44, 1); // num FAT sectors
        put_u32(&mut bytes, 48, 1); // dir start sector
        put_u32(&mut bytes, 56, 4096); // mini cutoff
        put_u32(&mut bytes, 60, ENDOFCHAIN); // mini-FAT start (none)
        put_u32(&mut bytes, 64, 0); // num mini-FAT
        put_u32(&mut bytes, 68, ENDOFCHAIN); // DIFAT start
        put_u32(&mut bytes, 72, 0); // num DIFAT
        put_u32(&mut bytes, HEADER_DIFAT_OFFSET, 0); // DIFAT[0] = FAT sector 0
        for k in 1..HEADER_DIFAT_COUNT {
            put_u32(&mut bytes, HEADER_DIFAT_OFFSET + k * 4, FREESECT);
        }
        bytes
    }

    // ── A minimal BIFF8 record-stream builder ──

    struct Biff {
        buf: Vec<u8>,
    }

    impl Biff {
        fn new() -> Self {
            Biff { buf: Vec::new() }
        }

        /// Append a record (type, length, data). Returns the byte offset of the
        /// record header (so a BOUNDSHEET can be patched to point at a BOF).
        fn rec(&mut self, typ: u16, data: &[u8]) -> usize {
            let off = self.buf.len();
            self.buf.extend_from_slice(&typ.to_le_bytes());
            self.buf.extend_from_slice(&(data.len() as u16).to_le_bytes());
            self.buf.extend_from_slice(data);
            off
        }

        fn bof(&mut self, dt: u16) -> usize {
            // BOF: version (u16), dt (u16), then 12 reserved bytes.
            let mut d = vec![0u8; 16];
            put_u16(&mut d, 0, 0x0600); // BIFF8
            put_u16(&mut d, 2, dt);
            self.rec(REC_BOF, &d)
        }

        fn eof(&mut self) {
            self.rec(REC_EOF, &[]);
        }

        /// Patch a previously-written BOUNDSHEET record (header at `bs_off`) so
        /// its BOF-offset field points at the current write position.
        fn patch_boundsheet_bof(&mut self, bs_off: usize) {
            let here = self.buf.len() as u32;
            let field = bs_off + 4; // skip the 4-byte record header
            self.buf[field..field + 4].copy_from_slice(&here.to_le_bytes());
        }
    }

    /// Encode a minimal BIFF8 SST holding one compressed string `s`.
    fn sst_one(s: &str) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&1u32.to_le_bytes()); // total count
        d.extend_from_slice(&1u32.to_le_bytes()); // unique count
        let bytes = s.as_bytes();
        d.extend_from_slice(&(bytes.len() as u16).to_le_bytes()); // char count
        d.push(0x00); // grbit: compressed, no rich/ext
        d.extend_from_slice(bytes);
        d
    }

    /// Encode a BOUNDSHEET pointing at `bof_offset`, named `name`, visible.
    fn boundsheet(bof_offset: u32, name: &str) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&bof_offset.to_le_bytes()); // BOF position
        d.push(0x00); // visible
        d.push(0x00); // sheet type (worksheet)
        d.push(name.len() as u8); // char count
        d.push(0x00); // compressed
        d.extend_from_slice(name.as_bytes());
        d
    }

    fn cell_header(row: u16, col: u16, xf: u16) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&row.to_le_bytes());
        d.extend_from_slice(&col.to_le_bytes());
        d.extend_from_slice(&xf.to_le_bytes());
        d
    }

    fn first_sheet(doc: &Document) -> &Sheet {
        match &doc.sections[0].pages[0].blocks[0].kind {
            BlockKind::Sheet(s) => &s.sheets[0],
            other => panic!("expected sheet block, got {other:?}"),
        }
    }

    /// Build a complete `.xls`: globals (SST["Hi"] + one BOUNDSHEET) then a
    /// worksheet substream with a LABELSST at A1 and a NUMBER at B1.
    fn build_minimal_xls() -> Vec<u8> {
        let mut b = Biff::new();
        // Globals substream.
        b.bof(BOF_GLOBALS);
        b.rec(REC_SST, &sst_one("Hi"));
        let bs_off = b.rec(REC_BOUNDSHEET, &boundsheet(0, "Sheet1"));
        b.eof();

        // Worksheet substream: patch the BOUNDSHEET to point here, then write it.
        b.patch_boundsheet_bof(bs_off);
        b.bof(BOF_WORKSHEET);
        // A1 (row 0, col 0) → SST index 0 ("Hi").
        let mut labelsst = cell_header(0, 0, 0);
        labelsst.extend_from_slice(&0u32.to_le_bytes());
        b.rec(REC_LABELSST, &labelsst);
        // B1 (row 0, col 1) = 42.0.
        let mut number = cell_header(0, 1, 0);
        number.extend_from_slice(&42.0f64.to_le_bytes());
        b.rec(REC_NUMBER, &number);
        b.eof();

        build_cfb("Workbook", &b.buf)
    }

    /// Build globals (one BOUNDSHEET, no SST) + a worksheet whose body is
    /// produced by `sheet_body`, wiring the BOUNDSHEET offset automatically.
    fn build_xls_with(name: &str, sheet_body: impl FnOnce(&mut Biff)) -> Vec<u8> {
        let mut b = Biff::new();
        b.bof(BOF_GLOBALS);
        let bs_off = b.rec(REC_BOUNDSHEET, &boundsheet(0, name));
        b.eof();
        b.patch_boundsheet_bof(bs_off);
        b.bof(BOF_WORKSHEET);
        sheet_body(&mut b);
        b.eof();
        build_cfb("Workbook", &b.buf)
    }

    #[test]
    fn minimal_xls_label_and_number() {
        let xls = build_minimal_xls();
        let doc = xls_to_model(&xls).expect("xls → model");
        let sheet = first_sheet(&doc);
        assert_eq!(sheet.name, "Sheet1");
        assert_eq!(sheet.rows[0].cells[0].value, CellValue::Text("Hi".into()));
        assert_eq!(sheet.rows[0].cells[1].value, CellValue::Number(42.0));
    }

    #[test]
    fn rk_integer_cell_decodes() {
        // RK with the integer flag (bit 1) set: value 100 ⇒ (100 << 2) | 0x02.
        let xls = build_xls_with("S", |b| {
            let mut rk = cell_header(2, 3, 0);
            let encoded: u32 = (100u32 << 2) | 0x02; // integer 100
            rk.extend_from_slice(&encoded.to_le_bytes());
            b.rec(REC_RK, &rk);
        });
        let doc = xls_to_model(&xls).expect("xls → model");
        let sheet = first_sheet(&doc);
        assert_eq!(sheet.rows[2].cells[3].value, CellValue::Number(100.0));
    }

    #[test]
    fn rk_div100_cell_decodes() {
        // RK integer with div-by-100: 1234 → 12.34.
        let xls = build_xls_with("S", |b| {
            let mut rk = cell_header(0, 0, 0);
            let encoded: u32 = (1234u32 << 2) | 0x02 | 0x01; // int + /100
            rk.extend_from_slice(&encoded.to_le_bytes());
            b.rec(REC_RK, &rk);
        });
        let doc = xls_to_model(&xls).expect("xls → model");
        let sheet = first_sheet(&doc);
        assert_eq!(sheet.rows[0].cells[0].value, CellValue::Number(12.34));
    }

    #[test]
    fn mergedcells_produces_range() {
        // One merge: rows 0..=1, cols 0..=2 ⇒ stored as (r0,r1,c0,c1)=(0,1,0,2).
        let xls = build_xls_with("S", |b| {
            let mut mc = Vec::new();
            mc.extend_from_slice(&1u16.to_le_bytes()); // count
            for v in [0u16, 1, 0, 2] {
                mc.extend_from_slice(&v.to_le_bytes());
            }
            b.rec(REC_MERGEDCELLS, &mc);
        });
        let doc = xls_to_model(&xls).expect("xls → model");
        let sheet = first_sheet(&doc);
        assert_eq!(sheet.merges.len(), 1);
        let m = sheet.merges[0];
        assert_eq!((m.r0, m.c0, m.r1, m.c1), (0, 0, 1, 2));
    }

    #[test]
    fn colinfo_width_converts_to_points() {
        // COLINFO width 256 (1/256ths) = 1 char = 7 pt.
        let xls = build_xls_with("S", |b| {
            let mut ci = Vec::new();
            ci.extend_from_slice(&0u16.to_le_bytes()); // first col 0
            ci.extend_from_slice(&0u16.to_le_bytes()); // last col 0
            ci.extend_from_slice(&256u16.to_le_bytes()); // width = 256/256 char
            ci.extend_from_slice(&0u16.to_le_bytes()); // xf
            ci.extend_from_slice(&0u16.to_le_bytes()); // options
            b.rec(REC_COLINFO, &ci);
            // A cell so the sheet isn't empty.
            let mut num = cell_header(0, 0, 0);
            num.extend_from_slice(&1.0f64.to_le_bytes());
            b.rec(REC_NUMBER, &num);
        });
        let doc = xls_to_model(&xls).expect("xls → model");
        let sheet = first_sheet(&doc);
        assert_eq!(sheet.col_widths, vec![7.0]);
    }

    #[test]
    fn formula_with_cached_string() {
        let xls = build_xls_with("S", |b| {
            // FORMULA at A1 whose cached result is a string (result bytes:
            // [0x00, _, _, _, _, _, 0xFF, 0xFF]).
            let mut f = cell_header(0, 0, 0);
            f.extend_from_slice(&[0x00, 0, 0, 0, 0, 0, 0xFF, 0xFF]); // 8-byte result
            f.extend_from_slice(&0u16.to_le_bytes()); // grbit
            f.extend_from_slice(&[0, 0, 0, 0]); // chn
            f.extend_from_slice(&0u16.to_le_bytes()); // formula token length
            b.rec(REC_FORMULA, &f);
            // The STRING record with the cached text.
            let mut s = Vec::new();
            s.extend_from_slice(&2u16.to_le_bytes()); // 2 chars
            s.push(0x00); // compressed
            s.extend_from_slice(b"Ok");
            b.rec(REC_STRING, &s);
        });
        let doc = xls_to_model(&xls).expect("xls → model");
        let sheet = first_sheet(&doc);
        assert_eq!(sheet.rows[0].cells[0].value, CellValue::Text("Ok".into()));
    }

    #[test]
    fn formula_with_cached_number() {
        let xls = build_xls_with("S", |b| {
            let mut f = cell_header(1, 1, 0);
            f.extend_from_slice(&7.5f64.to_le_bytes()); // plain IEEE-754 result
            f.extend_from_slice(&0u16.to_le_bytes()); // grbit
            f.extend_from_slice(&[0, 0, 0, 0]); // chn
            f.extend_from_slice(&0u16.to_le_bytes()); // token length
            b.rec(REC_FORMULA, &f);
        });
        let doc = xls_to_model(&xls).expect("xls → model");
        let sheet = first_sheet(&doc);
        assert_eq!(sheet.rows[1].cells[1].value, CellValue::Number(7.5));
    }

    #[test]
    fn mulrk_run_decodes() {
        // MULRK at row 0, cols 1..=2 with two integer RKs (10, 20).
        let xls = build_xls_with("S", |b| {
            let mut m = Vec::new();
            m.extend_from_slice(&0u16.to_le_bytes()); // row
            m.extend_from_slice(&1u16.to_le_bytes()); // first col
            for v in [10u32, 20] {
                m.extend_from_slice(&0u16.to_le_bytes()); // xf
                m.extend_from_slice(&((v << 2) | 0x02).to_le_bytes()); // int RK
            }
            m.extend_from_slice(&2u16.to_le_bytes()); // last col
            b.rec(REC_MULRK, &m);
        });
        let doc = xls_to_model(&xls).expect("xls → model");
        let sheet = first_sheet(&doc);
        assert_eq!(sheet.rows[0].cells[1].value, CellValue::Number(10.0));
        assert_eq!(sheet.rows[0].cells[2].value, CellValue::Number(20.0));
    }

    #[test]
    fn boolerr_boolean_cell() {
        let xls = build_xls_with("S", |b| {
            let mut be = cell_header(0, 0, 0);
            be.push(0x01); // value = TRUE
            be.push(0x00); // fBoolErr = 0 ⇒ boolean
            b.rec(REC_BOOLERR, &be);
        });
        let doc = xls_to_model(&xls).expect("xls → model");
        let sheet = first_sheet(&doc);
        assert_eq!(sheet.rows[0].cells[0].value, CellValue::Bool(true));
    }

    #[test]
    fn xf_font_bold_maps_to_style() {
        // Globals carry a bold FONT and an XF referencing it; a NUMBER cell uses
        // that XF and must come out bold.
        let mut b = Biff::new();
        b.bof(BOF_GLOBALS);
        // FONT: height 240 twips (12 pt), weight 700 (bold), name "Arial".
        let mut font = Vec::new();
        font.extend_from_slice(&240u16.to_le_bytes()); // height
        font.extend_from_slice(&0u16.to_le_bytes()); // grbit
        font.extend_from_slice(&0x7FFFu16.to_le_bytes()); // colour = auto
        font.extend_from_slice(&0x02BCu16.to_le_bytes()); // weight 700
        font.extend_from_slice(&0u16.to_le_bytes()); // escapement
        font.push(0); // underline
        font.push(0); // family
        font.push(0); // charset
        font.push(0); // reserved
        font.push(b"Arial".len() as u8); // name char count
        font.push(0x00); // compressed
        font.extend_from_slice(b"Arial");
        b.rec(REC_FONT, &font);
        // XF #0 referencing font 0, fmt 0, no alignment.
        let mut xf = vec![0u8; 20];
        put_u16(&mut xf, 0, 0); // font idx
        put_u16(&mut xf, 2, 0); // fmt idx
        b.rec(REC_XF, &xf);
        let bs_off = b.rec(REC_BOUNDSHEET, &boundsheet(0, "S"));
        b.eof();
        b.patch_boundsheet_bof(bs_off);
        b.bof(BOF_WORKSHEET);
        let mut num = cell_header(0, 0, 0); // uses XF 0
        num.extend_from_slice(&3.0f64.to_le_bytes());
        b.rec(REC_NUMBER, &num);
        b.eof();
        let xls = build_cfb("Workbook", &b.buf);

        let doc = xls_to_model(&xls).expect("xls → model");
        let sheet = first_sheet(&doc);
        let cell = &sheet.rows[0].cells[0];
        assert!(cell.style.bold, "cell should be bold from its XF font");
        assert_eq!(cell.style.size_pt, 12.0);
        assert_eq!(cell.style.family, "Arial");
        assert_eq!(cell.style.generic, Generic::Sans);
    }

    #[test]
    fn book_stream_biff5_name_fallback() {
        // A workbook stored under the legacy "Book" name must still open.
        let mut b = Biff::new();
        b.bof(BOF_GLOBALS);
        let bs_off = b.rec(REC_BOUNDSHEET, &boundsheet(0, "Sheet1"));
        b.eof();
        b.patch_boundsheet_bof(bs_off);
        b.bof(BOF_WORKSHEET);
        let mut num = cell_header(0, 0, 0);
        num.extend_from_slice(&5.0f64.to_le_bytes());
        b.rec(REC_NUMBER, &num);
        b.eof();
        let xls = build_cfb("Book", &b.buf);
        let doc = xls_to_model(&xls).expect("Book stream → model");
        assert_eq!(first_sheet(&doc).rows[0].cells[0].value, CellValue::Number(5.0));
    }

    #[test]
    fn garbage_returns_none() {
        assert!(xls_to_model(b"not a compound file at all").is_none());
        assert!(xls_to_model(&[]).is_none());
        // A valid CFB but with no BIFF content in the Workbook stream.
        let junk = build_cfb("Workbook", &[0u8; 10]);
        assert!(xls_to_model(&junk).is_none());
    }

    #[test]
    fn decode_rk_all_four_encodings() {
        // int, no div: 5
        assert_eq!(decode_rk((5u32 << 2) | 0x02), 5.0);
        // int, div100: 250 → 2.5
        assert_eq!(decode_rk((250u32 << 2) | 0x02 | 0x01), 2.5);
        // float, no div: encode 1.5 as the top 30 bits of its f64 bits.
        let bits = (1.5f64.to_bits() >> 32) as u32 & 0xFFFF_FFFC;
        assert_eq!(decode_rk(bits), 1.5);
        // float, div100: 150.0 packed, /100 → 1.5
        let bits2 = ((150.0f64.to_bits() >> 32) as u32 & 0xFFFF_FFFC) | 0x01;
        assert_eq!(decode_rk(bits2), 1.5);
    }
}
