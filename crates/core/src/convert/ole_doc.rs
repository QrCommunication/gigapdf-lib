//! A from-scratch **legacy Word 97–2003 `.doc` ([MS-DOC]) reader**. Zero
//! dependencies — built on the in-house CFB container reader ([`crate::convert::cfb`]).
//!
//! A `.doc` is a Compound File (OLE2) whose payload is a set of binary streams.
//! The text is **not** a contiguous blob: Word stores it in *pieces* described by
//! a **piece table** (the `CLX`/`Plcfpcd`), each piece pointing at either an
//! 8-bit (CP1252) or a UTF-16LE run somewhere inside the `WordDocument` stream.
//! Reassembling those pieces in character-position (CP) order yields the full
//! logical text — the fragmentation-safe extraction every serious `.doc` reader
//! performs (a naive "read the largest stream as UTF-16" scrape mis-orders and
//! corrupts edited documents). Formatting (bold/italic/underline/size/colour and
//! paragraph alignment) lives in separate **bin tables** (`PlcfBteChpx`/
//! `PlcfBtePapx`) that index 512-byte **formatted-disk-page (FKP)** structures
//! mapping byte ranges to `CHPX`/`PAPX` property lists (`grpprl`s of `sprm`s).
//!
//! ## What this module reads
//!
//! - **Streams** (via [`cfb`](crate::convert::cfb)): `WordDocument` (main text +
//!   FKPs), the table stream — `0Table` *or* `1Table`, chosen by the FIB
//!   `fWhichTblStm` flag — and `Data`.
//! - **FIB** at offset 0 of `WordDocument`: `wIdent` magic (`0xA5EC`), `nFib`,
//!   the flag word carrying `fWhichTblStm`, `ccpText`, and the `fc/lcb` blob
//!   (`fcClx`/`lcbClx`, `fcPlcfBteChpx`, `fcPlcfBtePapx`, `fcStshf`).
//! - **Piece table** (`CLX` at `fcClx` in the table stream): leading `clxtPrc`
//!   (`0x01`) blocks are skipped; the `clxtPlcfpcd` (`0x02`) block holds the
//!   `Plcfpcd` (`n+1` CPs then `n` 8-byte `Pcd`s). Each piece is decoded as
//!   CP1252 (when `fc & 0x4000_0000`, text at `(fc & 0x3FFF_FFFF)/2`, one byte per
//!   char) or UTF-16LE (at `fc`).
//! - **Structure**: split on paragraph marks (`\r`); cell marks (`\u{07}`) close a
//!   table cell / row; the special control characters are stripped from the text.
//! - **Formatting**: `CHPX` → [`CharStyle`] per run (bold `sprmCFBold 0x0835`,
//!   italic `0x0836`, underline `sprmCKul 0x2A3E`, size `sprmCHps 0x4A43`
//!   half-points, colour `sprmCCv 0x6870` / `sprmCIco 0x2A42`); `PAPX` →
//!   paragraph alignment (`sprmPJc 0x2403`).
//!
//! ## Deferred (noted, not silently dropped)
//!
//! - **Images** (Data-stream / `escher` `\u{01}` picture placeholders) — the
//!   placeholder char is stripped; the picture bytes are not extracted.
//! - **Field-result computation** — the field control characters
//!   (`\u{13}`/`\u{14}`/`\u{15}`) are stripped, keeping the cached *result* text
//!   that sits between them; no field code is evaluated.
//! - **Style sheet** (`fcStshf`) base styles — only the **direct** `sprm`s on
//!   each run/paragraph are applied; a paragraph that relies solely on its named
//!   style's defaults keeps the model defaults.
//!
//! **Robustness.** This is untrusted input. Every offset/length read is
//! bounds-checked, every table walk is bounded, and any malformed/truncated
//! structure yields `None` or an empty result rather than a panic. The container
//! reader already enforces the same discipline; `#![forbid(unsafe_code)]` is in
//! force crate-wide.
//!
//! [MS-DOC]: https://learn.microsoft.com/openspecs/office_file_formats/ms-doc/

use crate::convert::cfb::Cfb;
use crate::convert::style::Generic;
use crate::model::{
    self, Block, BlockKind, Cell, CharStyle, Document, PageGeometry, Paragraph, ParagraphStyle,
    Row, Section, Table,
};

/// The `wIdent` magic at offset 0 of every `WordDocument` FIB (MS-DOC §2.5.1).
const WIDENT: u16 = 0xA5EC;

// ---- Word control characters in the decoded text stream (MS-DOC §2.4.1). ----

/// Inline picture placeholder (an embedded `\u{01}` standing in for a drawing).
const CH_PIC: char = '\u{01}';
/// Field-begin control character.
const CH_FIELD_BEGIN: char = '\u{13}';
/// Field-separator control character (cached result follows).
const CH_FIELD_SEP: char = '\u{14}';
/// Field-end control character.
const CH_FIELD_END: char = '\u{15}';
/// Cell mark — ends a table cell, and (when the row's last cell) the row.
const CH_CELL: char = '\u{07}';
/// Paragraph mark.
const CH_PARA: char = '\r';
/// Hard line break within a paragraph.
const CH_LINE: char = '\u{0B}';
/// Page / column break.
const CH_PAGE: char = '\u{0C}';
/// Section mark (treated like a paragraph mark for splitting).
const CH_SECTION: char = '\u{1E}'; // not used as a boundary; kept for clarity

/// Parse `bytes` as a legacy Word `.doc` (MS-DOC) and lower it into the unified
/// [`Document`] model. Returns `None` if the bytes are not a Compound File, lack
/// a `WordDocument` stream, or carry an invalid FIB (`wIdent != 0xA5EC`). Never
/// panics on malformed input.
pub fn doc_to_model(bytes: &[u8]) -> Option<Document> {
    let cfb = Cfb::open(bytes)?;
    let word = cfb.read_stream("WordDocument")?;
    let fib = Fib::parse(&word)?;

    // The table stream is named `0Table` or `1Table`; the FIB flag picks which.
    let table_name = if fib.which_table_stream {
        "1Table"
    } else {
        "0Table"
    };
    // Fall back to the other name if the indicated one is absent (some producers
    // disagree with the flag); an absent table stream means no piece table, so we
    // degrade to a single UTF-16 piece over `ccpText` (handled in `build_text`).
    let table = cfb
        .read_stream(table_name)
        .or_else(|| cfb.read_stream(if fib.which_table_stream { "0Table" } else { "1Table" }))
        .unwrap_or_default();

    // Reassemble the logical text from the piece table (CP-ordered), recording for
    // every decoded character its originating byte position in `WordDocument` so
    // the CHPX/PAPX bin tables (which key on those file offsets) can be applied.
    let doc_text = build_text(&word, &table, &fib);
    if doc_text.chars.is_empty() {
        // A valid FIB with no recoverable text still yields a (one empty page)
        // document so callers get a well-formed model rather than `None`.
        return Some(empty_document(fib.n_fib));
    }

    // Style-sheet base (`fcStshf`): the default-paragraph (`Normal`, istd 0)
    // character defaults, used as the base every run's direct sprms layer onto so
    // a run that only carries a delta (e.g. just bold) still inherits the document
    // default size/colour. Absent/garbled STSH ⇒ the model default.
    let base_style = parse_stshf_base_style(&table, &fib);
    // Character formatting: PlcfBteChpx → FKP pages → CHPX runs keyed by FC range.
    let chp_runs = parse_chpx_runs(&word, &table, &fib, &base_style);
    // Paragraph formatting: PlcfBtePapx → FKP pages → PAPX keyed by FC range.
    let pap_runs = parse_papx_runs(&word, &table, &fib);

    let blocks = build_blocks(&doc_text, &chp_runs, &pap_runs, &base_style);
    Some(document_from_blocks(blocks, fib.n_fib))
}

// ===========================================================================
// FIB — File Information Block (MS-DOC §2.5.1)
// ===========================================================================

/// The subset of the FIB this reader needs. All offsets are into the
/// `WordDocument` stream; the `fc*`/`lcb*` pairs locate structures in the table
/// stream (except the FKP page indices, which are file offsets into
/// `WordDocument`).
#[derive(Debug, Default, Clone)]
struct Fib {
    /// `nFib` — the format version (Word 97 ≈ `0x00C1`, later `0x0101`/`0x010C`).
    n_fib: u16,
    /// `base.A` bit 9 (`fWhichTblStm`): `true` ⇒ the `1Table` stream, else `0Table`.
    which_table_stream: bool,
    /// `ccpText` — count of characters in the main document text.
    ccp_text: u32,
    /// `(fcClx, lcbClx)` — the piece table / complex-file structure.
    fc_clx: u32,
    lcb_clx: u32,
    /// `(fcPlcfBteChpx, lcb)` — the CHPX bin table.
    fc_plcf_bte_chpx: u32,
    lcb_plcf_bte_chpx: u32,
    /// `(fcPlcfBtePapx, lcb)` — the PAPX bin table.
    fc_plcf_bte_papx: u32,
    lcb_plcf_bte_papx: u32,
    /// `(fcStshf, lcb)` — the style sheet (read for completeness; base styles are
    /// a deferred bonus, see the module docs).
    fc_stshf: u32,
    lcb_stshf: u32,
}

impl Fib {
    /// Parse the FIB at offset 0 of the `WordDocument` stream. Returns `None` if
    /// `wIdent` is wrong or the header is too short to hold the fixed base.
    fn parse(w: &[u8]) -> Option<Fib> {
        if read_u16(w, 0)? != WIDENT {
            return None;
        }
        let n_fib = read_u16(w, 2)?;
        // `base.A` is the 16-bit flag word at offset 10 (after wIdent, nFib,
        // unused, lid, pnNext). `fWhichTblStm` is bit 9 (mask `0x0200`).
        let flags = read_u16(w, 10)?;
        let which_table_stream = flags & 0x0200 != 0;

        // The FibRgFcLcb blob layout is versioned. After the 32-byte fixed base
        // comes `csw` (u16) + `fibRgW` (csw shorts) + `cslw` (u16) + `fibRgLw`
        // (cslw longs) + `cbRgFcLcb` (u16) + the `fibRgFcLcbBlob`. Walk those
        // count-prefixed arrays so the blob's file offset is exact across
        // nFib 0x00C1 / 0x0101 / 0x010C rather than hard-coded.
        let mut off = 32usize;
        let csw = read_u16(w, off)? as usize;
        off = off.checked_add(2)?; // past csw
        off = off.checked_add(csw.checked_mul(2)?)?; // past fibRgW (csw u16s)

        // `ccpText` is the first field of `fibRgLw` (FibRgLw97). Read it relative
        // to the start of fibRgLw, after its own `cslw` count.
        let cslw = read_u16(w, off)? as usize;
        let fib_rg_lw_start = off.checked_add(2)?;
        let ccp_text = read_u32(w, fib_rg_lw_start)?;
        off = fib_rg_lw_start.checked_add(cslw.checked_mul(4)?)?; // past fibRgLw

        // `cbRgFcLcb` (count of 8-byte fc/lcb pairs) then the blob itself.
        let _cb_rg_fc_lcb = read_u16(w, off)?;
        let blob = off.checked_add(2)?; // start of fibRgFcLcbBlob

        // FibRgFcLcb97 field offsets (each an 8-byte fc/lcb pair) from the blob
        // start (MS-DOC §2.5.5). Indices below are the pair ordinals × 8.
        // fcStshf is pair 1, fcPlcfBteChpx pair 12, fcPlcfBtePapx pair 13,
        // fcClx pair 33.
        let pair = |idx: usize| -> Option<(u32, u32)> {
            let base = blob.checked_add(idx.checked_mul(8)?)?;
            Some((read_u32(w, base)?, read_u32(w, base.checked_add(4)?)?))
        };
        let (fc_stshf, lcb_stshf) = pair(1)?;
        let (fc_plcf_bte_chpx, lcb_plcf_bte_chpx) = pair(12)?;
        let (fc_plcf_bte_papx, lcb_plcf_bte_papx) = pair(13)?;
        let (fc_clx, lcb_clx) = pair(33)?;

        Some(Fib {
            n_fib,
            which_table_stream,
            ccp_text,
            fc_clx,
            lcb_clx,
            fc_plcf_bte_chpx,
            lcb_plcf_bte_chpx,
            fc_plcf_bte_papx,
            lcb_plcf_bte_papx,
            fc_stshf,
            lcb_stshf,
        })
    }
}

// ===========================================================================
// Piece table (CLX / Plcfpcd) + logical-text reassembly (MS-DOC §2.8.35, §2.9.177)
// ===========================================================================

/// One text piece: the half-open character-position range it covers and where its
/// bytes live in `WordDocument` (decoded as CP1252 when `cp1252`, else UTF-16LE).
#[derive(Debug, Clone)]
struct Piece {
    /// First character position (inclusive) this piece covers.
    cp_start: u32,
    /// One past the last character position (exclusive).
    cp_end: u32,
    /// Byte offset of the piece's first character within `WordDocument`.
    fc: u32,
    /// `true` ⇒ 8-bit CP1252 (one byte/char); `false` ⇒ UTF-16LE (two bytes/char).
    cp1252: bool,
}

/// The reassembled logical document text plus, for each character, its byte offset
/// in `WordDocument` (the key the CHPX/PAPX bin tables index on).
#[derive(Debug, Default)]
struct DocText {
    /// Decoded characters in character-position order.
    chars: Vec<char>,
    /// `fc[i]` = byte offset of `chars[i]` in `WordDocument`.
    fc: Vec<u32>,
}

/// Decode the piece table from the `CLX` and reassemble the document text.
///
/// Falls back to a single UTF-16 piece spanning `ccpText` (anchored at the FIB
/// header end, offset `0x200`) when the CLX is absent/empty/malformed — the
/// simplest valid layout for a tiny Word file with no complex-file part.
fn build_text(word: &[u8], table: &[u8], fib: &Fib) -> DocText {
    let pieces = parse_piece_table(table, fib).unwrap_or_default();
    let pieces = if pieces.is_empty() {
        // Single UTF-16 piece across the whole text, starting just past the FIB.
        vec![Piece {
            cp_start: 0,
            cp_end: fib.ccp_text,
            fc: 0x200,
            cp1252: false,
        }]
    } else {
        pieces
    };

    let mut out = DocText::default();
    for p in &pieces {
        let count = p.cp_end.saturating_sub(p.cp_start) as usize;
        if count == 0 {
            continue;
        }
        if p.cp1252 {
            // One byte per character; map each through CP1252.
            for i in 0..count {
                let byte_off = (p.fc as usize).saturating_add(i);
                let Some(&b) = word.get(byte_off) else { break };
                out.chars.push(cp1252_decode(b));
                out.fc.push(byte_off as u32);
            }
        } else {
            // Two bytes per character (UTF-16LE). Lone surrogates are dropped.
            for i in 0..count {
                let byte_off = (p.fc as usize).checked_add(i.saturating_mul(2));
                let Some(byte_off) = byte_off else { break };
                let Some(cu) = read_u16(word, byte_off) else {
                    break;
                };
                if let Some(ch) = char::from_u32(cu as u32) {
                    out.chars.push(ch);
                    out.fc.push(byte_off as u32);
                }
            }
        }
    }
    out
}

/// Parse the `CLX` at `fcClx` in the table stream: skip leading `clxtPrc` (`0x01`)
/// property blocks, then read the `clxtPlcfpcd` (`0x02`) `Plcfpcd` into pieces.
/// Returns `None` if the CLX is absent or no `Plcfpcd` is present.
fn parse_piece_table(table: &[u8], fib: &Fib) -> Option<Vec<Piece>> {
    if fib.lcb_clx == 0 {
        return None;
    }
    let start = fib.fc_clx as usize;
    let end = start.checked_add(fib.lcb_clx as usize)?;
    let clx = table.get(start..end.min(table.len()))?;

    let mut off = 0usize;
    // The CLX is a sequence of `Prc` (tag `0x01`) then exactly one `Pcdt`
    // (tag `0x02`). Skip every `Prc`: tag byte + `cbGrpprl` (u16) + that many bytes.
    while off < clx.len() {
        match clx[off] {
            0x01 => {
                let cb = read_u16(clx, off.checked_add(1)?)? as usize;
                off = off.checked_add(3)?.checked_add(cb)?;
            }
            0x02 => {
                // Pcdt: tag (1) + lcb (u32) + Plcfpcd (lcb bytes).
                let lcb = read_u32(clx, off.checked_add(1)?)? as usize;
                let pl_start = off.checked_add(5)?;
                let pl_end = pl_start.checked_add(lcb)?;
                let plcfpcd = clx.get(pl_start..pl_end.min(clx.len()))?;
                return Some(parse_plcfpcd(plcfpcd));
            }
            _ => return None, // unknown tag ⇒ give up (fall back to single piece)
        }
    }
    None
}

/// Parse a `Plcfpcd`: `n+1` little-endian `u32` CPs followed by `n` 8-byte `Pcd`s.
/// Each `Pcd`'s `fc.fc` field encodes the byte offset *and* the CP1252/UTF-16 flag.
fn parse_plcfpcd(pl: &[u8]) -> Vec<Piece> {
    // A `PLC` of n `Pcd`s has size `(n+1)*4 + n*8` ⇒ `n = (size - 4) / 12`.
    if pl.len() < 4 + 8 {
        return Vec::new();
    }
    let n = (pl.len() - 4) / 12;
    if n == 0 {
        return Vec::new();
    }
    let cps_bytes = 4 * (n + 1);
    let mut pieces = Vec::with_capacity(n);
    for i in 0..n {
        let cp_start = read_u32(pl, i * 4).unwrap_or(0);
        let cp_end = read_u32(pl, (i + 1) * 4).unwrap_or(cp_start);
        let pcd_off = cps_bytes + i * 8;
        // Pcd: u16 flags, then `fc` (a 4-byte FcCompressed) at +2, then PRM at +6.
        let Some(fc_compressed) = read_u32(pl, pcd_off + 2) else {
            continue;
        };
        // FcCompressed: bit 30 (`fCompressed`) ⇒ CP1252, and the real byte offset
        // is `(fc & 0x3FFF_FFFF) / 2`. Otherwise the offset is `fc` and the text
        // is UTF-16LE.
        let cp1252 = fc_compressed & 0x4000_0000 != 0;
        let fc = if cp1252 {
            (fc_compressed & 0x3FFF_FFFF) / 2
        } else {
            fc_compressed
        };
        pieces.push(Piece {
            cp_start,
            cp_end,
            fc,
            cp1252,
        });
    }
    pieces
}

// ===========================================================================
// Bin tables → FKP pages → CHPX / PAPX (MS-DOC §2.8.6, §2.9.4, §2.9.177)
// ===========================================================================

/// A character-formatting run over a half-open byte range `[fc_start, fc_end)` in
/// `WordDocument`, with the resolved [`CharStyle`].
#[derive(Debug, Clone)]
struct ChpRun {
    fc_start: u32,
    fc_end: u32,
    style: CharStyle,
}

/// A paragraph-formatting run over a half-open byte range, with the resolved
/// paragraph alignment.
#[derive(Debug, Clone)]
struct PapRun {
    fc_start: u32,
    fc_end: u32,
    align: model::Align,
}

/// Read a `PlcfBteChpx`/`PlcfBtePapx` bin table: it is a `PLC` of `u32` FC keys
/// (`n+1` of them) followed by `n` `PnFkpChpx`/`PnFkpPapx` entries (each a 4-byte
/// value whose low 22 bits — `pn` — give the FKP page number; the FKP itself
/// lives at `pn * 512` in `WordDocument`). Returns the list of FKP page numbers.
fn bin_table_fkp_pages(table: &[u8], fc: u32, lcb: u32) -> Vec<u32> {
    if lcb == 0 {
        return Vec::new();
    }
    let start = fc as usize;
    let Some(end) = start.checked_add(lcb as usize) else {
        return Vec::new();
    };
    let Some(plc) = table.get(start..end.min(table.len())) else {
        return Vec::new();
    };
    // size = (n+1)*4 + n*4 = 8n + 4 ⇒ n = (size - 4) / 8.
    if plc.len() < 8 {
        return Vec::new();
    }
    let n = (plc.len() - 4) / 8;
    let keys_bytes = 4 * (n + 1);
    let mut pages = Vec::with_capacity(n);
    for i in 0..n {
        let off = keys_bytes + i * 4;
        if let Some(v) = read_u32(plc, off) {
            // `pn` is the low 22 bits of the PnFkp* (MS-DOC §2.9.180).
            pages.push(v & 0x003F_FFFF);
        }
    }
    pages
}

/// Parse all CHPX runs by walking the FKP pages named by the `PlcfBteChpx`. Each
/// run starts from `base` (the style-sheet default-paragraph character defaults)
/// so a run carrying only a delta sprm still inherits the document default.
fn parse_chpx_runs(word: &[u8], table: &[u8], fib: &Fib, base: &CharStyle) -> Vec<ChpRun> {
    let pages = bin_table_fkp_pages(table, fib.fc_plcf_bte_chpx, fib.lcb_plcf_bte_chpx);
    let mut runs = Vec::new();
    for pn in pages {
        parse_chpx_fkp(word, pn, base, &mut runs);
    }
    runs.sort_by_key(|r| r.fc_start);
    runs
}

/// Parse all PAPX runs by walking the FKP pages named by the `PlcfBtePapx`.
fn parse_papx_runs(word: &[u8], table: &[u8], fib: &Fib) -> Vec<PapRun> {
    let pages = bin_table_fkp_pages(table, fib.fc_plcf_bte_papx, fib.lcb_plcf_bte_papx);
    let mut runs = Vec::new();
    for pn in pages {
        parse_papx_fkp(word, pn, &mut runs);
    }
    runs.sort_by_key(|r| r.fc_start);
    runs
}

/// Parse the style-sheet (`STSH` at `fcStshf` in the table stream) and return the
/// character defaults of the **default paragraph style** (istd 0, "Normal").
///
/// STSH layout (MS-DOC §2.9.271): a `cbStshi` (u16) length prefix, then the
/// `STSHI` header (whose first two fields are `cstd` (u16, the STD count) and
/// `cbSTDBaseInFile` (u16)), then the array of STDs. Each STD is a `cbStd` (u16)
/// length-prefix followed by `cbStd` bytes: an `STDF` base (`cbSTDBaseInFile`
/// bytes, ≥ 8) then the style name (an MS-DOC `Xst` / `xstzName`) then the
/// `grpprl`s. Style 0 is the document default paragraph style, so its character
/// `grpprl` carries the document-wide default size/colour/etc.
///
/// This walks just far enough to reach STD 0's `grpprl` and feeds its character
/// `sprm`s through [`apply_chpx`]. Anything missing/garbled ⇒ the model default
/// [`CharStyle`] (no panic). Paragraph defaults from the style sheet (alignment,
/// indents) are **not** lowered here — only the character base — keeping the
/// "stylesheet base styles are a bonus" scope tight (see the module docs).
fn parse_stshf_base_style(table: &[u8], fib: &Fib) -> CharStyle {
    let mut base = CharStyle::default();
    if fib.lcb_stshf < 2 {
        return base;
    }
    let start = fib.fc_stshf as usize;
    let Some(end) = start.checked_add(fib.lcb_stshf as usize) else {
        return base;
    };
    let Some(stsh) = table.get(start..end.min(table.len())) else {
        return base;
    };

    // `cbStshi` length-prefix, then the STSHI header.
    let Some(cb_stshi) = read_u16(stsh, 0) else {
        return base;
    };
    let stshi_start = 2usize;
    // `cbSTDBaseInFile` is the STSHI's second u16 (after `cstd`).
    let Some(cb_std_base) = read_u16(stsh, stshi_start + 2) else {
        return base;
    };
    // STD array begins right after the STSHI header.
    let std_array = match stshi_start.checked_add(cb_stshi as usize) {
        Some(off) if off <= stsh.len() => off,
        _ => return base,
    };

    // STD 0: `cbStd` (u16) length, then `cbStd` bytes of STDF + name + grpprls.
    let Some(cb_std) = read_u16(stsh, std_array) else {
        return base;
    };
    if cb_std == 0 {
        return base; // an empty STD ⇒ no overrides; keep the model default
    }
    let std_body_start = std_array + 2;
    let std_body_end = match std_body_start.checked_add(cb_std as usize) {
        Some(e) if e <= stsh.len() => e,
        _ => return base,
    };
    let std = &stsh[std_body_start..std_body_end];

    // After the STDF base comes the style name as an `Xst`: a `cch` (u16) count of
    // UTF-16 units, then `cch` units, then a terminating NUL unit. Skip it to reach
    // the grpprls, then skip the paragraph grpprl to reach the character grpprl.
    let base_off = cb_std_base as usize;
    let Some(cch) = read_u16(std, base_off) else {
        return base;
    };
    // name bytes = 2 (cch) + cch*2 (units) + 2 (NUL terminator).
    let name_bytes = 2usize
        .checked_add((cch as usize).saturating_mul(2))
        .and_then(|n| n.checked_add(2));
    let Some(name_bytes) = name_bytes else {
        return base;
    };
    let grpprls_start = match base_off.checked_add(name_bytes) {
        Some(o) if o <= std.len() => o,
        _ => return base,
    };

    // The grpprls follow as a packed area. For a paragraph style the area holds a
    // PAPX (`sprmP*`) and a CHPX (`sprmC*`); rather than guess the PAPX/CHPX split
    // (which varies by format), scan every `sprm` in the remaining bytes and apply
    // the character ones — `apply_chpx` ignores non-character opcodes, and
    // `for_each_sprm` advances by each opcode's true operand length, so a leading
    // paragraph sprm is stepped over cleanly. This yields the default character
    // properties regardless of the split.
    apply_chpx(&std[grpprls_start..], &mut base);
    base
}

/// Borrow the 512-byte FKP page `pn` from `WordDocument`, length-checked.
fn fkp_page(word: &[u8], pn: u32) -> Option<&[u8]> {
    let base = (pn as usize).checked_mul(512)?;
    let end = base.checked_add(512)?;
    word.get(base..end)
}

/// Parse one **CHPX FKP** page (MS-DOC §2.9.4): the last byte `crun` is the run
/// count; the first `crun+1` `u32`s are the FC boundaries; then `crun` 1-byte
/// `BX` word-offsets (each pointing at a `ChpxFkp` = `cb` byte + that many grpprl
/// bytes) — a `BX` of `0` means "no exceptions" (default formatting).
fn parse_chpx_fkp(word: &[u8], pn: u32, base: &CharStyle, out: &mut Vec<ChpRun>) {
    let Some(page) = fkp_page(word, pn) else {
        return;
    };
    let crun = page[511] as usize;
    if crun == 0 {
        return;
    }
    // FC boundaries occupy `(crun+1)*4` bytes; the `crun` BX bytes follow.
    let fc_area = (crun + 1) * 4;
    let bx_area_end = fc_area + crun; // one BX byte per run
    if bx_area_end > 511 {
        return; // malformed page; refuse rather than misread
    }
    for i in 0..crun {
        let Some(fc_start) = read_u32(page, i * 4) else {
            continue;
        };
        let Some(fc_end) = read_u32(page, (i + 1) * 4) else {
            continue;
        };
        if fc_end <= fc_start {
            continue;
        }
        let word_off = page[fc_area + i] as usize;
        let mut style = base.clone();
        if word_off != 0 {
            let chpx_off = word_off * 2; // BX offset is in 2-byte words
            if let Some(grpprl) = chpx_at(page, chpx_off) {
                apply_chpx(grpprl, &mut style);
            }
        }
        out.push(ChpRun {
            fc_start,
            fc_end,
            style,
        });
    }
}

/// Borrow a `ChpxFkp` grpprl at byte offset `off` within an FKP page: a `cb`
/// length byte followed by `cb` bytes of `sprm`s. `None` if it runs past the page.
fn chpx_at(page: &[u8], off: usize) -> Option<&[u8]> {
    let cb = *page.get(off)? as usize;
    let start = off.checked_add(1)?;
    let end = start.checked_add(cb)?;
    page.get(start..end)
}

/// Parse one **PAPX FKP** page (MS-DOC §2.9.177): like the CHPX FKP but each run's
/// `BX` is a 13-byte structure whose first byte `wOffset` (×2) locates a `PapxFkp`
/// = a `cb` byte (where `cb==0` means a following 2-byte `cb` ×2) then `istd`
/// (u16) then the grpprl bytes.
fn parse_papx_fkp(word: &[u8], pn: u32, out: &mut Vec<PapRun>) {
    let Some(page) = fkp_page(word, pn) else {
        return;
    };
    let crun = page[511] as usize;
    if crun == 0 {
        return;
    }
    let fc_area = (crun + 1) * 4;
    let bx_area_end = fc_area + crun * 13; // BX is 13 bytes for PAPX
    if bx_area_end > 511 {
        return;
    }
    for i in 0..crun {
        let Some(fc_start) = read_u32(page, i * 4) else {
            continue;
        };
        let Some(fc_end) = read_u32(page, (i + 1) * 4) else {
            continue;
        };
        if fc_end <= fc_start {
            continue;
        }
        let bx_off = fc_area + i * 13;
        let mut align = model::Align::Left;
        if let Some(&w_offset) = page.get(bx_off) {
            if w_offset != 0 {
                if let Some(grpprl) = papx_grpprl_at(page, w_offset as usize * 2) {
                    apply_papx(grpprl, &mut align);
                }
            }
        }
        out.push(PapRun {
            fc_start,
            fc_end,
            align,
        });
    }
}

/// Borrow a `PapxInFkp` grpprl at byte offset `off`: `cb` byte; if `cb != 0` the
/// grpprl size is `2*cb - 1` and `istd`+grpprl follow the `cb` byte; if `cb == 0`
/// a 2-byte `cb` follows and the size is `2*cb`. The leading `istd` (u16) is
/// skipped, returning only the `sprm` bytes.
fn papx_grpprl_at(page: &[u8], off: usize) -> Option<&[u8]> {
    let cb = *page.get(off)? as usize;
    let (size, body_off) = if cb != 0 {
        // grpprl byte length = 2*cb - 1; body starts right after the cb byte and
        // begins with the 2-byte `istd`.
        (cb.checked_mul(2)?.checked_sub(1)?, off.checked_add(1)?)
    } else {
        let cb2 = *page.get(off.checked_add(1)?)? as usize;
        (cb2.checked_mul(2)?, off.checked_add(2)?)
    };
    // The body is `istd` (2 bytes) + grpprl. Skip the istd.
    let grpprl_len = size.checked_sub(2)?;
    let start = body_off.checked_add(2)?;
    let end = start.checked_add(grpprl_len)?;
    page.get(start..end)
}

// ===========================================================================
// sprm decoding (MS-DOC §2.6.1, §2.9.255) — the common character/paragraph sprms
// ===========================================================================

/// Decode one sprm's operand size from its 16-bit opcode's `spra` field (bits
/// 13–15), per MS-DOC §2.6.1. Returns the operand byte length, or `None` for the
/// variable-length case (`spra == 6`, handled by reading a leading length byte).
fn sprm_operand_len(opcode: u16) -> Option<usize> {
    match (opcode >> 13) & 0x7 {
        0 | 1 => Some(1),     // 0 = toggle/operation (1 byte), 1 = 1-byte
        2 | 4 | 5 => Some(2), // 2-byte
        3 => Some(4),         // 4-byte (e.g. sprmCCv colour, a 4-byte COLORREF)
        6 => None,            // variable: a length byte precedes the operand
        7 => Some(3),         // 3-byte
        _ => Some(0),
    }
}

/// Iterate a `grpprl` (a packed sequence of `sprm`s), calling `f(opcode, operand)`
/// for each. Bounded and bounds-checked; a malformed length ends iteration.
fn for_each_sprm(grpprl: &[u8], mut f: impl FnMut(u16, &[u8])) {
    let mut off = 0usize;
    // Cap iterations to the byte length (each sprm consumes ≥ 2 bytes).
    let mut guard = grpprl.len().saturating_add(1);
    while off + 2 <= grpprl.len() && guard > 0 {
        guard -= 1;
        let opcode = u16::from_le_bytes([grpprl[off], grpprl[off + 1]]);
        let body = off + 2;
        let (operand_len, len_prefix) = match sprm_operand_len(opcode) {
            Some(n) => (n, 0usize),
            None => {
                // Variable length: a single length byte precedes the operand.
                let Some(&n) = grpprl.get(body) else { break };
                (n as usize, 1usize)
            }
        };
        let operand_start = body + len_prefix;
        let operand_end = operand_start.saturating_add(operand_len);
        if operand_end > grpprl.len() {
            break;
        }
        f(opcode, &grpprl[operand_start..operand_end]);
        off = operand_end;
    }
}

/// Apply a CHPX `grpprl` to a [`CharStyle`]: the common character `sprm`s
/// (bold/italic/underline/size/colour). Unknown `sprm`s are ignored.
fn apply_chpx(grpprl: &[u8], style: &mut CharStyle) {
    for_each_sprm(grpprl, |opcode, operand| match opcode {
        // sprmCFBold (0x0835): toggle bold. Operand 0/1 = off/on; 128/129 = "as
        // style"/"negate style" — treat the low bit as the boolean.
        0x0835 => style.bold = toggle_bool(operand, style.bold),
        // sprmCFItalic (0x0836): toggle italic.
        0x0836 => style.italic = toggle_bool(operand, style.italic),
        // sprmCKul (0x2A3E): underline kind. 0 = none; anything else = underlined.
        0x2A3E => style.underline = operand.first().is_some_and(|&v| v != 0),
        // sprmCFStrike (0x0837): toggle strike-through.
        0x0837 => style.strike = toggle_bool(operand, style.strike),
        // sprmCHps (0x4A43): font size in half-points (u16) ⇒ points.
        0x4A43 => {
            if let Some(hps) = read_u16(operand, 0) {
                if hps > 0 {
                    style.size_pt = hps as f64 / 2.0;
                }
            }
        }
        // sprmCCv (0x6870): a 4-byte COLORREF (`0x00bbggrr` little-endian ⇒ bytes
        // R, G, B, 0). `rgb_from_operand` reads the first three bytes as R,G,B.
        0x6870 => {
            if let Some(rgb) = rgb_from_operand(operand) {
                style.color = Some(rgb);
            }
        }
        // sprmCIco (0x2A42): one of the 16 classic Word palette colours (1-byte).
        0x2A42 => {
            if let Some(&ico) = operand.first() {
                if let Some(rgb) = ico_color(ico) {
                    style.color = Some(rgb);
                }
            }
        }
        _ => {}
    });
}

/// Apply a PAPX `grpprl`, extracting paragraph alignment (`sprmPJc`).
fn apply_papx(grpprl: &[u8], align: &mut model::Align) {
    for_each_sprm(grpprl, |opcode, operand| {
        // sprmPJc80 (0x2403) and sprmPJc (0x2461) both carry the justification
        // code as a 1-byte operand: 0=left, 1=center, 2=right, 3/4=justify.
        if opcode == 0x2403 || opcode == 0x2461 {
            if let Some(&jc) = operand.first() {
                *align = match jc {
                    1 => model::Align::Center,
                    2 => model::Align::Right,
                    3 | 4 | 5 | 7 | 8 | 9 => model::Align::Justify,
                    _ => model::Align::Left,
                };
            }
        }
    });
}

/// Resolve a toggle `sprm`'s operand against the current value: `0` ⇒ false,
/// `1` ⇒ true, `0x80` ⇒ keep (use style default = current), `0x81` ⇒ negate.
fn toggle_bool(operand: &[u8], current: bool) -> bool {
    match operand.first() {
        Some(0) => false,
        Some(1) => true,
        Some(0x80) => current,
        Some(0x81) => !current,
        _ => current,
    }
}

/// Build an RGB triple (`0.0..=1.0`) from a 3- or 4-byte colour operand (R,G,B in
/// the first three bytes). `None` if too short.
fn rgb_from_operand(operand: &[u8]) -> Option<[f64; 3]> {
    if operand.len() < 3 {
        return None;
    }
    Some([
        operand[0] as f64 / 255.0,
        operand[1] as f64 / 255.0,
        operand[2] as f64 / 255.0,
    ])
}

/// Map a classic Word `Ico` palette index (1–16; 0 = "auto") to RGB. Returns
/// `None` for `0`/out-of-range so the model keeps its default (black).
fn ico_color(ico: u8) -> Option<[f64; 3]> {
    // MS-DOC §2.9.123: 0=auto, then black, blue, cyan, green, magenta, red,
    // yellow, white, dark variants, then grey shades.
    let rgb255: [u8; 3] = match ico {
        1 => [0x00, 0x00, 0x00],  // black
        2 => [0x00, 0x00, 0xFF],  // blue
        3 => [0x00, 0xFF, 0xFF],  // cyan
        4 => [0x00, 0xFF, 0x00],  // green
        5 => [0xFF, 0x00, 0xFF],  // magenta
        6 => [0xFF, 0x00, 0x00],  // red
        7 => [0xFF, 0xFF, 0x00],  // yellow
        8 => [0xFF, 0xFF, 0xFF],  // white
        9 => [0x00, 0x00, 0x80],  // dark blue
        10 => [0x00, 0x80, 0x80], // dark cyan
        11 => [0x00, 0x80, 0x00], // dark green
        12 => [0x80, 0x00, 0x80], // dark magenta
        13 => [0x80, 0x00, 0x00], // dark red
        14 => [0x80, 0x80, 0x00], // dark yellow (olive)
        15 => [0x80, 0x80, 0x80], // dark grey
        16 => [0xC0, 0xC0, 0xC0], // light grey
        _ => return None,
    };
    Some([
        rgb255[0] as f64 / 255.0,
        rgb255[1] as f64 / 255.0,
        rgb255[2] as f64 / 255.0,
    ])
}

// ===========================================================================
// Structure building — paragraphs & tables from the decoded text + formatting
// ===========================================================================

/// A logical paragraph as split from the text stream: its visible characters with
/// their originating FCs, and whether it ended at a cell mark (`\u{07}`) — which,
/// in the absence of an explicit row marker, signals a table-cell boundary.
#[derive(Debug, Default)]
struct RawPara {
    chars: Vec<char>,
    fc: Vec<u32>,
    /// `true` when this paragraph's terminator was a cell mark, not a `\r`.
    ends_cell: bool,
}

/// Split the decoded text into logical paragraphs at `\r`, `\u{07}` (cell), page
/// and section marks, stripping the field/picture control characters. A run of
/// cell-terminated paragraphs forms one table row (closed by the row's final cell
/// mark — in MS-DOC a row ends with a cell mark whose paragraph carries the
/// "table row end" property; lacking the full TAP parse we treat a `\u{07}`
/// immediately followed by `\r`, or the last `\u{07}` before a non-cell paragraph,
/// as the row end). Line breaks (`\u{0B}`) are kept inside the paragraph text.
fn split_paragraphs(doc: &DocText) -> Vec<RawPara> {
    let mut paras = Vec::new();
    let mut cur = RawPara::default();
    for (i, &ch) in doc.chars.iter().enumerate() {
        let fc = doc.fc.get(i).copied().unwrap_or(0);
        match ch {
            CH_PARA | CH_PAGE => {
                cur.ends_cell = false;
                paras.push(std::mem::take(&mut cur));
            }
            CH_CELL => {
                cur.ends_cell = true;
                paras.push(std::mem::take(&mut cur));
            }
            // Stripped control characters (field markers + inline-picture
            // placeholder): they carry no visible text.
            CH_FIELD_BEGIN | CH_FIELD_SEP | CH_FIELD_END | CH_PIC | CH_SECTION => {}
            // Keep a hard line break as a real newline inside the paragraph text.
            CH_LINE => {
                cur.chars.push('\n');
                cur.fc.push(fc);
            }
            _ => {
                cur.chars.push(ch);
                cur.fc.push(fc);
            }
        }
    }
    // A trailing paragraph with content but no terminator still counts.
    if !cur.chars.is_empty() {
        paras.push(cur);
    }
    paras
}

/// Build the model blocks: consecutive cell-terminated paragraphs are grouped into
/// table rows (and runs of rows into a [`Table`]); every other paragraph becomes a
/// [`Paragraph`] block. Character runs within a paragraph are split where the CHPX
/// formatting changes; paragraph alignment comes from the PAPX covering its text.
fn build_blocks(doc: &DocText, chp: &[ChpRun], pap: &[PapRun], base: &CharStyle) -> Vec<Block> {
    let paras = split_paragraphs(doc);
    let mut blocks: Vec<Block> = Vec::new();
    // Pending table rows being accumulated from consecutive cell-terminated paras.
    let mut pending_rows: Vec<Row> = Vec::new();
    // Cells of the row currently being built.
    let mut cur_cells: Vec<Cell> = Vec::new();

    for rp in &paras {
        if rp.ends_cell {
            // This paragraph is a table cell. Build the cell's paragraph block and
            // push it as a one-paragraph cell.
            let para_block = build_paragraph_block(rp, chp, pap, base);
            cur_cells.push(Cell {
                blocks: vec![para_block],
                ..Cell::default()
            });
            // Heuristic row break: in well-formed MS-DOC the last cell of a row is
            // immediately followed by a paragraph mark belonging to the row-end
            // paragraph; lacking the TAP we close the row when the next paragraph
            // is not itself a cell. That decision is made by peeking below, so here
            // we only keep accumulating cells.
            continue;
        }

        // A normal paragraph ends any open table row, then any open table.
        if !cur_cells.is_empty() {
            pending_rows.push(Row {
                cells: std::mem::take(&mut cur_cells),
                ..Row::default()
            });
        }
        if !pending_rows.is_empty() {
            blocks.push(table_block(std::mem::take(&mut pending_rows)));
        }

        // Skip an empty paragraph that is purely a structural artifact (e.g. the
        // row-end paragraph between a table and following content is often empty),
        // but keep genuinely empty paragraphs the user typed only when they carry
        // text; an entirely empty paragraph contributes a blank Paragraph so blank
        // lines survive.
        blocks.push(build_paragraph_block(rp, chp, pap, base));
    }

    // Flush a table left open at the end of the document.
    if !cur_cells.is_empty() {
        pending_rows.push(Row {
            cells: cur_cells,
            ..Row::default()
        });
    }
    if !pending_rows.is_empty() {
        blocks.push(table_block(pending_rows));
    }
    blocks
}

/// Wrap accumulated rows into a `Table` block, computing the column count as the
/// widest row so the model's `col_widths` length is sensible (widths default to
/// `0.0`, meaning "auto" to the exporters).
fn table_block(rows: Vec<Row>) -> Block {
    let cols = rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
    Block {
        kind: BlockKind::Table(Table {
            rows,
            col_widths: vec![0.0; cols],
            border: model::BorderStyle::default(),
        }),
        ..Block::default()
    }
}

/// Build a [`Paragraph`] block from one raw paragraph: split its characters into
/// styled runs at CHPX boundaries and set the paragraph alignment from the PAPX
/// covering its first character.
fn build_paragraph_block(rp: &RawPara, chp: &[ChpRun], pap: &[PapRun], base: &CharStyle) -> Block {
    let runs = build_runs(rp, chp, base);
    let align = rp
        .fc
        .first()
        .map(|&fc| align_at(pap, fc))
        .unwrap_or(model::Align::Left);
    Block {
        kind: BlockKind::Paragraph(Paragraph {
            style: ParagraphStyle {
                align,
                ..ParagraphStyle::default()
            },
            style_ref: None,
            runs,
        }),
        ..Block::default()
    }
}

/// Split a paragraph's characters into [`Inline::Run`]s, starting a new run
/// whenever the CHPX style of the underlying FC changes. Adjacent characters with
/// equal style coalesce into one run.
fn build_runs(rp: &RawPara, chp: &[ChpRun], base: &CharStyle) -> Vec<model::Inline> {
    let mut runs: Vec<model::Inline> = Vec::new();
    let mut cur_text = String::new();
    let mut cur_style: Option<CharStyle> = None;

    for (i, &ch) in rp.chars.iter().enumerate() {
        let fc = rp.fc.get(i).copied().unwrap_or(0);
        let style = style_at(chp, fc, base);
        match &cur_style {
            Some(s) if *s == style => cur_text.push(ch),
            _ => {
                if let Some(s) = cur_style.take() {
                    if !cur_text.is_empty() {
                        runs.push(make_run(std::mem::take(&mut cur_text), s));
                    }
                }
                cur_text.clear();
                cur_text.push(ch);
                cur_style = Some(style);
            }
        }
    }
    if let Some(s) = cur_style {
        if !cur_text.is_empty() {
            runs.push(make_run(cur_text, s));
        }
    }
    runs
}

/// Build an `Inline::Run` from finished text + style, filling the portable
/// `generic` fallback (always `Sans` here — the legacy format names no family
/// reliably without the STSH/SttbfFfn, a deferred bonus).
fn make_run(text: String, mut style: CharStyle) -> model::Inline {
    if style.generic == Generic::default() && style.family.is_empty() {
        style.generic = Generic::Sans;
    }
    model::Inline::Run(model::InlineRun {
        text,
        style,
        source_index: None,
    })
}

/// The [`CharStyle`] in effect at byte offset `fc`: the CHPX run whose `[start,end)`
/// range contains `fc`, or the style-sheet `base` style when none does.
fn style_at(chp: &[ChpRun], fc: u32, base: &CharStyle) -> CharStyle {
    // Runs are sorted by `fc_start`; a linear scan is fine for the run counts
    // legacy docs produce, and keeps the lookup allocation-free.
    for r in chp {
        if fc >= r.fc_start && fc < r.fc_end {
            return r.style.clone();
        }
    }
    base.clone()
}

/// The paragraph alignment in effect at byte offset `fc` (PAPX run containing it).
fn align_at(pap: &[PapRun], fc: u32) -> model::Align {
    for r in pap {
        if fc >= r.fc_start && fc < r.fc_end {
            return r.align;
        }
    }
    model::Align::Left
}

// ===========================================================================
// Document assembly
// ===========================================================================

/// Wrap the built blocks into a single-section, single-page [`Document`], using
/// the `..Default::default()` pattern so the model stays robust to fields added
/// concurrently. `n_fib` (the FIB format version) is recorded as the generating
/// application so the origin Word generation survives into the model metadata.
fn document_from_blocks(blocks: Vec<Block>, n_fib: u16) -> Document {
    Document {
        meta: model::DocMeta {
            application: word_version_name(n_fib).to_string(),
            ..model::DocMeta::default()
        },
        sections: vec![Section {
            geometry: default_geometry(),
            pages: vec![model::Page {
                blocks,
                absolute: false,
            }],
            ..Section::default()
        }],
        ..Document::default()
    }
}

/// Human label for an `nFib` FIB version — the legacy Word generation that wrote
/// the file (MS-DOC §2.5.1 / the public `nFib` table). Unknown values fall back to
/// a generic "Microsoft Word (legacy .doc)" so the field is never misleading.
fn word_version_name(n_fib: u16) -> &'static str {
    match n_fib {
        0x00C1 => "Microsoft Word 97",
        0x00D9 => "Microsoft Word 2000",
        0x0101 => "Microsoft Word 2002",
        0x010C => "Microsoft Word 2003",
        0x0112 => "Microsoft Word 2007 (legacy .doc)",
        _ => "Microsoft Word (legacy .doc)",
    }
}

/// A well-formed empty document (one empty page) for a valid-but-text-less file.
/// `n_fib` is taken from the parsed FIB so even the empty case records its origin.
fn empty_document(n_fib: u16) -> Document {
    document_from_blocks(Vec::new(), n_fib)
}

/// US-Letter page geometry in points — the legacy `.doc` default when the section
/// table is not parsed (deferred). Built via `..Default::default()` so any new
/// geometry field defaults cleanly.
fn default_geometry() -> PageGeometry {
    PageGeometry {
        width: 612.0,
        height: 792.0,
        ..PageGeometry::default()
    }
}

// ===========================================================================
// Byte helpers + CP1252 (zero-dependency)
// ===========================================================================

/// Read a little-endian `u16` at `off`, or `None` if it would run past the end.
fn read_u16(bytes: &[u8], off: usize) -> Option<u16> {
    let end = off.checked_add(2)?;
    if end > bytes.len() {
        return None;
    }
    Some(u16::from_le_bytes([bytes[off], bytes[off + 1]]))
}

/// Read a little-endian `u32` at `off`, or `None` if it would run past the end.
fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    if end > bytes.len() {
        return None;
    }
    Some(u32::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
    ]))
}

/// Decode a single Windows-1252 (CP1252) byte to its Unicode scalar. The `0x80..=0x9F`
/// band differs from Latin-1; the official mapping is reproduced. Undefined
/// positions (`0x81`, `0x8D`, `0x8F`, `0x90`, `0x9D`) map to their C1 control
/// code points (the de-facto behaviour of every decoder).
fn cp1252_decode(b: u8) -> char {
    match b {
        0x80 => '\u{20AC}', // €
        0x82 => '\u{201A}', // ‚
        0x83 => '\u{0192}', // ƒ
        0x84 => '\u{201E}', // „
        0x85 => '\u{2026}', // …
        0x86 => '\u{2020}', // †
        0x87 => '\u{2021}', // ‡
        0x88 => '\u{02C6}', // ˆ
        0x89 => '\u{2030}', // ‰
        0x8A => '\u{0160}', // Š
        0x8B => '\u{2039}', // ‹
        0x8C => '\u{0152}', // Œ
        0x8E => '\u{017D}', // Ž
        0x91 => '\u{2018}', // ‘
        0x92 => '\u{2019}', // ’
        0x93 => '\u{201C}', // “
        0x94 => '\u{201D}', // ”
        0x95 => '\u{2022}', // •
        0x96 => '\u{2013}', // –
        0x97 => '\u{2014}', // —
        0x98 => '\u{02DC}', // ˜
        0x99 => '\u{2122}', // ™
        0x9A => '\u{0161}', // š
        0x9B => '\u{203A}', // ›
        0x9C => '\u{0153}', // œ
        0x9E => '\u{017E}', // ž
        0x9F => '\u{0178}', // Ÿ
        // 0x00..=0x7F and 0xA0..=0xFF coincide with Unicode Latin-1; the
        // C1-control positions fall through here too (mapped 1:1).
        other => other as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built minimal Word `.doc` (MS-DOC) inside a valid CFB container.
    /// Mirrors the CFB module's `Builder` approach: lay out 512-byte sectors, wire
    /// a FAT + directory, then place a `WordDocument` and a `1Table` stream.
    ///
    /// Layout (one stream = one or more whole sectors, kept simple — each stream is
    /// ≥ the 4096-byte mini-cutoff so it uses the regular FAT, avoiding the
    /// mini-stream machinery):
    ///   sector 0      : FAT
    ///   sector 1      : directory (Root + WordDocument + 1Table)
    ///   sectors 2..   : WordDocument payload (8 sectors = 4096 bytes)
    ///   sectors ..    : 1Table payload (8 sectors = 4096 bytes)
    struct DocBuilder {
        word: Vec<u8>,
        table: Vec<u8>,
    }

    impl DocBuilder {
        fn new() -> DocBuilder {
            DocBuilder {
                // Generous fixed sizes so all our structures fit and each stream
                // exceeds the 4096 mini-cutoff (⇒ regular FAT, simpler test wiring).
                word: vec![0u8; 4096],
                table: vec![0u8; 4096],
            }
        }

        fn put_u16(buf: &mut [u8], off: usize, v: u16) {
            buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
        }
        fn put_u32(buf: &mut [u8], off: usize, v: u32) {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }

        /// Write the FIB at offset 0 of `word`, wiring `fWhichTblStm = 1` (use
        /// `1Table`) and the four fc/lcb pairs we exercise. The count-prefixed
        /// `csw`/`cslw`/`cbRgFcLcb` arrays are emitted so [`Fib::parse`]'s generic
        /// walk lands the blob exactly — `csw=0`, `cslw=1` (just `ccpText`),
        /// `cbRgFcLcb=0x5D` (the 93-pair FibRgFcLcb97).
        #[allow(clippy::too_many_arguments)]
        fn fib(
            &mut self,
            ccp_text: u32,
            fc_clx: u32,
            lcb_clx: u32,
            fc_chpx: u32,
            lcb_chpx: u32,
            fc_papx: u32,
            lcb_papx: u32,
        ) {
            let w = &mut self.word;
            DocBuilder::put_u16(w, 0, WIDENT); // wIdent
            DocBuilder::put_u16(w, 2, 0x00C1); // nFib (Word 97)
            DocBuilder::put_u16(w, 10, 0x0200); // base.A: fWhichTblStm = 1

            // Count-prefixed arrays after the 32-byte base:
            //   off 32: csw (u16) = 0  → no fibRgW shorts
            //   off 34: cslw (u16) = 1 → one fibRgLw long (ccpText)
            //   off 36: ccpText (u32)
            //   off 40: cbRgFcLcb (u16) = 0x5D
            //   off 42: fibRgFcLcbBlob start (93 × 8 bytes)
            DocBuilder::put_u16(w, 32, 0); // csw
            DocBuilder::put_u16(w, 34, 1); // cslw
            DocBuilder::put_u32(w, 36, ccp_text); // ccpText (first fibRgLw long)
            DocBuilder::put_u16(w, 40, 0x5D); // cbRgFcLcb (93 pairs)
            let blob = 42usize;
            let mut pair = |idx: usize, fc: u32, lcb: u32| {
                let base = blob + idx * 8;
                DocBuilder::put_u32(w, base, fc);
                DocBuilder::put_u32(w, base + 4, lcb);
            };
            pair(1, 0, 0); // fcStshf (unused here)
            pair(12, fc_chpx, lcb_chpx); // fcPlcfBteChpx
            pair(13, fc_papx, lcb_papx); // fcPlcfBtePapx
            pair(33, fc_clx, lcb_clx); // fcClx
        }

        /// Place raw bytes into the `WordDocument` stream at `off`, growing the
        /// stream (to a whole-sector multiple) if the write runs past its end.
        fn word_bytes(&mut self, off: usize, data: &[u8]) {
            let need = off + data.len();
            if need > self.word.len() {
                self.word.resize(need.next_multiple_of(512), 0);
            }
            self.word[off..off + data.len()].copy_from_slice(data);
        }
        /// Place raw bytes into the `1Table` stream at `off`, growing as needed.
        fn table_bytes(&mut self, off: usize, data: &[u8]) {
            let need = off + data.len();
            if need > self.table.len() {
                self.table.resize(need.next_multiple_of(512), 0);
            }
            self.table[off..off + data.len()].copy_from_slice(data);
        }

        /// Build a single-piece UTF-16 `CLX` (`Pcdt` only) into `1Table` at `off`,
        /// covering CPs `0..cp_end` whose text starts at `WordDocument` byte `fc`.
        /// Returns the CLX byte length (for the FIB's `lcbClx`).
        fn clx_single_utf16(&mut self, off: usize, cp_end: u32, fc: u32) -> u32 {
            // Pcdt: tag 0x02, lcb (u32), Plcfpcd. Plcfpcd = [cp0,cp1] (2 u32) + one
            // 8-byte Pcd: u16 flags, u32 fc (uncompressed ⇒ UTF-16), u16 prm.
            let plcfpcd_len = 4 * 2 + 8; // 16
            let mut clx = vec![0u8; 1 + 4 + plcfpcd_len];
            clx[0] = 0x02;
            DocBuilder::put_u32(&mut clx, 1, plcfpcd_len as u32);
            // CPs: 0 then cp_end.
            DocBuilder::put_u32(&mut clx, 5, 0);
            DocBuilder::put_u32(&mut clx, 9, cp_end);
            // Pcd at offset 5 + 8 = 13. flags(0), fc (uncompressed), prm(0).
            DocBuilder::put_u32(&mut clx, 13 + 2, fc); // fc field at Pcd+2
            self.table_bytes(off, &clx);
            clx.len() as u32
        }

        /// Build a two-piece `CLX`: piece 0 UTF-16 (`cp` `0..split`, bytes at
        /// `fc0`), piece 1 CP1252 (`cp` `split..end`, bytes at `fc1`). Returns the
        /// CLX byte length.
        fn clx_two_pieces(
            &mut self,
            off: usize,
            split: u32,
            end: u32,
            fc0: u32,
            fc1: u32,
        ) -> u32 {
            // Plcfpcd: 3 CPs (0, split, end) + 2 Pcds (8 bytes each).
            let plcfpcd_len = 4 * 3 + 8 * 2; // 28
            let mut clx = vec![0u8; 1 + 4 + plcfpcd_len];
            clx[0] = 0x02;
            DocBuilder::put_u32(&mut clx, 1, plcfpcd_len as u32);
            DocBuilder::put_u32(&mut clx, 5, 0);
            DocBuilder::put_u32(&mut clx, 9, split);
            DocBuilder::put_u32(&mut clx, 13, end);
            // Pcds start after the 3 CPs: offset 5 + 12 = 17.
            // Piece 0: UTF-16 ⇒ fc field = fc0 (no high bit).
            DocBuilder::put_u32(&mut clx, 17 + 2, fc0);
            // Piece 1: CP1252 ⇒ FcCompressed bit 30 set, stored fc = fc1*2.
            let fc1_compressed = 0x4000_0000u32 | (fc1 * 2);
            DocBuilder::put_u32(&mut clx, 17 + 8 + 2, fc1_compressed);
            self.table_bytes(off, &clx);
            clx.len() as u32
        }

        /// Build a one-page CHPX bin table (`PlcfBteChpx`) in `1Table` at `off`
        /// naming FKP page `pn`, plus the FKP page itself in `WordDocument`.
        /// The FKP maps one run `[fc_start, fc_end)` to the given `grpprl`.
        /// Returns the bin-table byte length.
        fn chpx_one_run(
            &mut self,
            off: usize,
            pn: u32,
            fc_start: u32,
            fc_end: u32,
            grpprl: &[u8],
        ) -> u32 {
            // --- the FKP page (512 bytes) at pn*512 in WordDocument ---
            let mut fkp = [0u8; 512];
            // crun = 1; FC boundaries (2 u32s) at 0..8.
            DocBuilder::put_u32(&mut fkp, 0, fc_start);
            DocBuilder::put_u32(&mut fkp, 4, fc_end);
            // One BX byte at offset 8 = word offset of the ChpxFkp. Place the
            // ChpxFkp near the page end at byte 500 (word offset 250).
            let chpx_byte = 500usize;
            fkp[8] = (chpx_byte / 2) as u8;
            // ChpxFkp: cb byte + grpprl.
            fkp[chpx_byte] = grpprl.len() as u8;
            fkp[chpx_byte + 1..chpx_byte + 1 + grpprl.len()].copy_from_slice(grpprl);
            // crun in the last byte.
            fkp[511] = 1;
            self.word_bytes(pn as usize * 512, &fkp);

            // --- the bin table (PlcfBteChpx): 2 FC keys + 1 PnFkpChpx ---
            let mut plc = vec![0u8; 4 * 2 + 4];
            DocBuilder::put_u32(&mut plc, 0, fc_start);
            DocBuilder::put_u32(&mut plc, 4, fc_end);
            DocBuilder::put_u32(&mut plc, 8, pn); // PnFkpChpx (pn in low 22 bits)
            self.table_bytes(off, &plc);
            plc.len() as u32
        }

        /// Finalise: wire a CFB around the two streams and return the file bytes.
        /// `WordDocument` gets directory slot 1, `1Table` slot 2 (siblings).
        fn build(self) -> Vec<u8> {
            const SIG: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
            let word_sectors = self.word.len().div_ceil(512); // 8
            let table_sectors = self.table.len().div_ceil(512); // 8
            // sector 0 FAT, 1 directory, then WordDocument, then 1Table.
            let word_first = 2u32;
            let table_first = word_first + word_sectors as u32;
            let total = 2 + word_sectors + table_sectors;

            let mut sectors = vec![[0u8; 512]; total];

            // ---- FAT (sector 0) ----
            {
                let fat = &mut sectors[0];
                DocBuilder::put_u32(fat, 0, 0xFFFF_FFFD); // FATSECT (sector 0)
                DocBuilder::put_u32(fat, 4, 0xFFFF_FFFE); // directory ⇒ ENDOFCHAIN
                                                          // WordDocument chain.
                for k in 0..word_sectors {
                    let sec = word_first as usize + k;
                    let next = if k + 1 < word_sectors {
                        sec as u32 + 1
                    } else {
                        0xFFFF_FFFE
                    };
                    DocBuilder::put_u32(fat, sec * 4, next);
                }
                // 1Table chain.
                for k in 0..table_sectors {
                    let sec = table_first as usize + k;
                    let next = if k + 1 < table_sectors {
                        sec as u32 + 1
                    } else {
                        0xFFFF_FFFE
                    };
                    DocBuilder::put_u32(fat, sec * 4, next);
                }
                for sec in total..(512 / 4) {
                    DocBuilder::put_u32(fat, sec * 4, 0xFFFF_FFFF); // FREESECT
                }
            }

            // ---- directory (sector 1) ----
            {
                let dir = &mut sectors[1];
                dir_entry(dir, 0, "Root Entry", 5, NOSTREAM, NOSTREAM, 1, 0xFFFF_FFFE, 0);
                // WordDocument: slot 1, right→slot 2 (sibling).
                dir_entry(
                    dir,
                    1,
                    "WordDocument",
                    2,
                    NOSTREAM,
                    2,
                    NOSTREAM,
                    word_first,
                    self.word.len() as u64,
                );
                // 1Table: slot 2.
                dir_entry(
                    dir,
                    2,
                    "1Table",
                    2,
                    NOSTREAM,
                    NOSTREAM,
                    NOSTREAM,
                    table_first,
                    self.table.len() as u64,
                );
            }

            // ---- stream payloads ----
            for k in 0..word_sectors {
                let sec = &mut sectors[word_first as usize + k];
                let s = k * 512;
                let e = (s + 512).min(self.word.len());
                sec[..e - s].copy_from_slice(&self.word[s..e]);
            }
            for k in 0..table_sectors {
                let sec = &mut sectors[table_first as usize + k];
                let s = k * 512;
                let e = (s + 512).min(self.table.len());
                sec[..e - s].copy_from_slice(&self.table[s..e]);
            }

            // ---- assemble + header ----
            let mut out = vec![0u8; 512 + total * 512];
            for (i, sec) in sectors.iter().enumerate() {
                out[512 + i * 512..512 + (i + 1) * 512].copy_from_slice(sec);
            }
            out[0..8].copy_from_slice(&SIG);
            DocBuilder::put_u16(&mut out, 24, 0x0003); // major version 3
            DocBuilder::put_u16(&mut out, 26, 0x003E); // minor
            DocBuilder::put_u16(&mut out, 28, 0xFFFE); // BOM
            DocBuilder::put_u16(&mut out, 30, 0x0009); // 512-byte sectors
            DocBuilder::put_u16(&mut out, 32, 0x0006); // 64-byte mini-sectors
            DocBuilder::put_u32(&mut out, 44, 1); // num FAT sectors
            DocBuilder::put_u32(&mut out, 48, 1); // dir start sector
            DocBuilder::put_u32(&mut out, 56, 4096); // mini cutoff
            DocBuilder::put_u32(&mut out, 60, 0xFFFF_FFFE); // mini-FAT start (none)
            DocBuilder::put_u32(&mut out, 64, 0); // num mini-FAT
            DocBuilder::put_u32(&mut out, 68, 0xFFFF_FFFE); // DIFAT start (none)
            DocBuilder::put_u32(&mut out, 72, 0); // num DIFAT
            DocBuilder::put_u32(&mut out, 76, 0); // inline DIFAT[0] = FAT sector 0
            for k in 1..109 {
                DocBuilder::put_u32(&mut out, 76 + k * 4, 0xFFFF_FFFF);
            }
            out
        }
    }

    const NOSTREAM: u32 = 0xFFFF_FFFF;

    /// Write a 128-byte CFB directory entry (UTF-16LE name + links + start/size).
    #[allow(clippy::too_many_arguments)]
    fn dir_entry(
        dir: &mut [u8; 512],
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
            dir[base + k * 2..base + k * 2 + 2].copy_from_slice(&ch.to_le_bytes());
            nlen = (k + 1) * 2;
        }
        nlen += 2; // include the NUL terminator
        dir[base + 64..base + 66].copy_from_slice(&(nlen as u16).to_le_bytes());
        dir[base + 66] = obj_type;
        dir[base + 67] = 1; // colour black
        dir[base + 68..base + 72].copy_from_slice(&left.to_le_bytes());
        dir[base + 72..base + 76].copy_from_slice(&right.to_le_bytes());
        dir[base + 76..base + 80].copy_from_slice(&child.to_le_bytes());
        dir[base + 116..base + 120].copy_from_slice(&start.to_le_bytes());
        dir[base + 120..base + 124].copy_from_slice(&((size & 0xFFFF_FFFF) as u32).to_le_bytes());
        dir[base + 124..base + 128].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
    }

    /// Encode a string as little-endian UTF-16 bytes (helper for placing text).
    fn utf16le(s: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(s.len() * 2);
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out
    }

    /// Collect the plain text of a paragraph block's runs (in order).
    fn para_text(block: &Block) -> String {
        match &block.kind {
            BlockKind::Paragraph(p) => p
                .runs
                .iter()
                .filter_map(|r| match r {
                    model::Inline::Run(run) => Some(run.text.clone()),
                    _ => None,
                })
                .collect(),
            _ => String::new(),
        }
    }

    /// Flatten every paragraph block on the document's single page to its text.
    fn page_paragraphs(doc: &Document) -> Vec<String> {
        doc.sections[0].pages[0]
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Paragraph(_)))
            .map(para_text)
            .collect()
    }

    #[test]
    fn hello_world_with_bold_run() {
        // Text: "Hello world\r" — 12 chars (incl. the paragraph mark). "world" is
        // bold via a CHPX over its byte range.
        let text = "Hello world\r";
        let cp_end = text.encode_utf16().count() as u32; // 12
        let text_fc = 0x400u32; // place the text well past the FIB
        let utf16 = utf16le(text);

        let mut b = DocBuilder::new();
        b.word_bytes(text_fc as usize, &utf16);

        // CLX (single UTF-16 piece) at table offset 0.
        let lcb_clx = b.clx_single_utf16(0, cp_end, text_fc);

        // Bold CHPX over "world" = chars 6..11 ⇒ bytes [fc+12, fc+22).
        // sprmCFBold (0x0835), 1-byte operand 0x01 (on).
        let world_start = text_fc + 6 * 2;
        let world_end = text_fc + 11 * 2;
        let chpx_grpprl = [0x35u8, 0x08, 0x01];
        let lcb_chpx = b.chpx_one_run(0x100, 7, world_start, world_end, &chpx_grpprl);

        b.fib(cp_end, 0, lcb_clx, 0x100, lcb_chpx, 0, 0);
        let bytes = b.build();

        let doc = doc_to_model(&bytes).expect("valid .doc must parse");
        let paras = page_paragraphs(&doc);
        assert_eq!(paras, vec!["Hello world".to_string()], "one paragraph, text intact");

        // The paragraph must contain a bold "world" run.
        let block = &doc.sections[0].pages[0].blocks[0];
        let BlockKind::Paragraph(p) = &block.kind else {
            panic!("expected a paragraph block");
        };
        let bold_text: String = p
            .runs
            .iter()
            .filter_map(|r| match r {
                model::Inline::Run(run) if run.style.bold => Some(run.text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(bold_text, "world", "the 'world' run must be bold");
        // And "Hello " must be a non-bold run.
        let plain_bold = p.runs.iter().any(|r| matches!(
            r, model::Inline::Run(run) if !run.style.bold && run.text.contains("Hello")
        ));
        assert!(plain_bold, "'Hello ' must be a non-bold run");
    }

    #[test]
    fn two_piece_table_reassembles_in_order() {
        // Piece 0 (UTF-16): "Foo" at fc0. Piece 1 (CP1252): "Bar\r" at fc1.
        // The é (CP1252 0xE9) proves the 8-bit decode path.
        let p0 = "Foo";
        let p1 = "Bär\r"; // 'ä' = CP1252 0xE4
        let split = p0.encode_utf16().count() as u32; // 3
        let end = split + p1.chars().count() as u32; // 3 + 4 = 7

        let fc0 = 0x400u32;
        let fc1 = 0x600u32;
        let mut b = DocBuilder::new();
        b.word_bytes(fc0 as usize, &utf16le(p0));
        // CP1252 bytes for "Bär\r": B, ä(0xE4), r, \r.
        b.word_bytes(fc1 as usize, &[b'B', 0xE4, b'r', b'\r']);

        let lcb_clx = b.clx_two_pieces(0, split, end, fc0, fc1);
        b.fib(end, 0, lcb_clx, 0, 0, 0, 0);
        let bytes = b.build();

        let doc = doc_to_model(&bytes).expect("valid .doc must parse");
        let paras = page_paragraphs(&doc);
        assert_eq!(paras, vec!["FooBär".to_string()], "pieces reassemble in CP order");
    }

    #[test]
    fn cell_marks_build_a_table() {
        // "A\u{07}B\u{07}\r" — two cells (A, B) then a row-end paragraph mark.
        // Expect one Table block with a row of two cells "A" and "B".
        let text = "A\u{07}B\u{07}\r";
        let cp_end = text.encode_utf16().count() as u32; // 5
        let fc = 0x400u32;
        let mut b = DocBuilder::new();
        b.word_bytes(fc as usize, &utf16le(text));
        let lcb_clx = b.clx_single_utf16(0, cp_end, fc);
        b.fib(cp_end, 0, lcb_clx, 0, 0, 0, 0);
        let bytes = b.build();

        let doc = doc_to_model(&bytes).expect("valid .doc must parse");
        let tables: Vec<&Table> = doc.sections[0].pages[0]
            .blocks
            .iter()
            .filter_map(|blk| match &blk.kind {
                BlockKind::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 1, "the cell marks must build exactly one table");
        let table = tables[0];
        assert_eq!(table.rows.len(), 1, "one row");
        assert_eq!(table.rows[0].cells.len(), 2, "two cells in the row");
        let cell_text = |c: &Cell| -> String {
            c.blocks.first().map(para_text).unwrap_or_default()
        };
        assert_eq!(cell_text(&table.rows[0].cells[0]), "A", "first cell = A");
        assert_eq!(cell_text(&table.rows[0].cells[1]), "B", "second cell = B");
    }

    #[test]
    fn paragraph_alignment_from_papx() {
        // "Centered\r" with a PAPX setting sprmPJc = 1 (center) over its bytes.
        let text = "Centered\r";
        let cp_end = text.encode_utf16().count() as u32; // 9
        let fc = 0x400u32;
        let para_end = fc + cp_end * 2;
        let mut b = DocBuilder::new();
        b.word_bytes(fc as usize, &utf16le(text));
        let lcb_clx = b.clx_single_utf16(0, cp_end, fc);
        // PAPX bin table at table offset 0x80, FKP page 9, sprmPJc80 (0x2403) = 1.
        let papx_grpprl = [0x03u8, 0x24, 0x01];
        let lcb_papx = papx_one_run(&mut b, 0x80, 9, fc, para_end, &papx_grpprl);
        b.fib(cp_end, 0, lcb_clx, 0, 0, 0x80, lcb_papx);
        let bytes = b.build();

        let doc = doc_to_model(&bytes).expect("valid .doc must parse");
        let block = &doc.sections[0].pages[0].blocks[0];
        let BlockKind::Paragraph(p) = &block.kind else {
            panic!("expected a paragraph block");
        };
        assert_eq!(p.style.align, model::Align::Center, "alignment must be centered");
    }

    /// Build a one-page PAPX bin table (`PlcfBtePapx`) in `1Table` at `off` naming
    /// FKP page `pn`, plus the FKP page in `WordDocument`. Maps one run
    /// `[fc_start, fc_end)` to `grpprl`. Returns the bin-table byte length.
    fn papx_one_run(
        b: &mut DocBuilder,
        off: usize,
        pn: u32,
        fc_start: u32,
        fc_end: u32,
        grpprl: &[u8],
    ) -> u32 {
        // --- the PAPX FKP page (512 bytes) at pn*512 in WordDocument ---
        let mut fkp = [0u8; 512];
        DocBuilder::put_u32(&mut fkp, 0, fc_start);
        DocBuilder::put_u32(&mut fkp, 4, fc_end);
        // One 13-byte BX at offset 8; its first byte is the word offset of the
        // PapxFkp. Put the PapxFkp at byte 480 (word offset 240).
        let papx_byte = 480usize;
        fkp[8] = (papx_byte / 2) as u8;
        // PapxInFkp: cb byte (nonzero) ⇒ grpprl len = 2*cb - 1; body = istd(2)+grpprl.
        // We need body size = 2 (istd) + grpprl.len(); choose cb so 2*cb-1 = that.
        let body_size = 2 + grpprl.len(); // istd + grpprl
        let cb = body_size.div_ceil(2); // 2*cb - 1 >= body_size
        fkp[papx_byte] = cb as u8;
        // istd (2 bytes) = 0, then the grpprl.
        let istd_off = papx_byte + 1;
        DocBuilder::put_u16(&mut fkp, istd_off, 0);
        fkp[istd_off + 2..istd_off + 2 + grpprl.len()].copy_from_slice(grpprl);
        fkp[511] = 1; // crun
        b.word_bytes(pn as usize * 512, &fkp);

        // --- the bin table (PlcfBtePapx): 2 FC keys + 1 PnFkpPapx ---
        let mut plc = vec![0u8; 4 * 2 + 4];
        DocBuilder::put_u32(&mut plc, 0, fc_start);
        DocBuilder::put_u32(&mut plc, 4, fc_end);
        DocBuilder::put_u32(&mut plc, 8, pn);
        b.table_bytes(off, &plc);
        plc.len() as u32
    }

    #[test]
    fn garbage_is_none_no_panic() {
        assert!(doc_to_model(b"not a compound file").is_none(), "garbage ⇒ None");
        assert!(doc_to_model(&[]).is_none(), "empty ⇒ None");
        // A valid CFB but with no WordDocument stream ⇒ None.
        // (Reuse the CFB module's expectation: our builder always adds one, so we
        // craft a CFB whose WordDocument FIB magic is wrong instead.)
        let mut b = DocBuilder::new();
        b.fib(0, 0, 0, 0, 0, 0, 0);
        // Corrupt the wIdent so Fib::parse rejects it.
        b.word[0] = 0x00;
        b.word[1] = 0x00;
        let bytes = b.build();
        assert!(doc_to_model(&bytes).is_none(), "bad wIdent ⇒ None");
    }

    #[test]
    fn truncated_is_safe() {
        let text = "Hello world\r";
        let cp_end = text.encode_utf16().count() as u32;
        let fc = 0x400u32;
        let mut b = DocBuilder::new();
        b.word_bytes(fc as usize, &utf16le(text));
        let lcb_clx = b.clx_single_utf16(0, cp_end, fc);
        b.fib(cp_end, 0, lcb_clx, 0, 0, 0, 0);
        let bytes = b.build();
        // Truncate at several points; never panic, returns None or a safe doc.
        for cut in [16usize, 256, 600, 1024, bytes.len() / 2] {
            if cut <= bytes.len() {
                let _ = doc_to_model(&bytes[..cut]);
            }
        }
    }

    #[test]
    fn cp1252_decodes_special_band() {
        // The 0x80..0x9F band must map to the CP1252 glyphs, not Latin-1.
        assert_eq!(cp1252_decode(0x80), '€');
        assert_eq!(cp1252_decode(0x92), '\u{2019}'); // right single quote
        assert_eq!(cp1252_decode(0x97), '—'); // em dash
        assert_eq!(cp1252_decode(0xE9), 'é'); // Latin-1 region passes through
        assert_eq!(cp1252_decode(b'A'), 'A');
    }

    #[test]
    fn sprm_operand_lengths() {
        // spra encodings (bits 13–15): bold (0x0835) is a toggle ⇒ spra 0 ⇒ 1 byte;
        // CHps (0x4A43) ⇒ spra 2 ⇒ 2 bytes; CCv (0x6870) ⇒ spra 3 ⇒ a 4-byte
        // COLORREF; sprmPJc (0x2403) ⇒ spra 1 ⇒ 1 byte; spra 6 ⇒ variable (None);
        // spra 7 ⇒ 3 bytes.
        assert_eq!(sprm_operand_len(0x0835), Some(1), "toggle ⇒ 1 byte");
        assert_eq!(sprm_operand_len(0x2403), Some(1), "sprmPJc ⇒ 1 byte");
        assert_eq!(sprm_operand_len(0x4A43), Some(2), "CHps ⇒ 2 bytes");
        assert_eq!(sprm_operand_len(0x6870), Some(4), "CCv ⇒ 4-byte COLORREF");
        // Construct a spra=6 opcode (bits 13..15 = 6 ⇒ 0xC000 base) ⇒ variable.
        assert_eq!(sprm_operand_len(0xC000), None, "spra=6 ⇒ variable");
        // spra=7 (bits 13..15 = 7 ⇒ 0xE000 base) ⇒ 3 bytes.
        assert_eq!(sprm_operand_len(0xE000), Some(3), "spra=7 ⇒ 3 bytes");
    }
}
