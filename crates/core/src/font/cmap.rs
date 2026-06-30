//! `/ToUnicode` CMap parsing (ISO 32000-1 §9.10.3) for font-aware text
//! extraction.
//!
//! A character code in a content stream is not Unicode — it indexes a glyph in
//! whatever encoding the font uses. To extract *readable* text (no tofu) for
//! CID/Type0 fonts and custom-encoded simple fonts, we read the font's
//! `/ToUnicode` stream: a small CMap mapping raw codes to Unicode via
//! `beginbfchar`/`beginbfrange` blocks. The CMap is lexed with our own
//! [`Lexer`], so no new tokenizer and zero dependencies.

use std::collections::{BTreeMap, BTreeSet};

use crate::lexer::{Lexer, Token};

/// A parsed `/ToUnicode` CMap: character code → Unicode string.
#[derive(Debug, Clone, Default)]
pub struct ToUnicode {
    map: BTreeMap<u32, String>,
}

impl ToUnicode {
    /// Parse a decoded `/ToUnicode` CMap stream. Unknown constructs are skipped
    /// rather than rejected — a partial map still beats tofu.
    pub fn parse(data: &[u8]) -> Self {
        let mut lexer = Lexer::new(data);
        let mut tokens = Vec::new();
        while let Ok(token) = lexer.next_token() {
            if matches!(token, Token::Eof) {
                break;
            }
            tokens.push(token);
        }

        let mut map = BTreeMap::new();
        let mut i = 0;
        while i < tokens.len() {
            match &tokens[i] {
                Token::Keyword(k) if k == b"beginbfchar" => {
                    i = parse_bfchar(&tokens, i + 1, &mut map);
                }
                Token::Keyword(k) if k == b"beginbfrange" => {
                    i = parse_bfrange(&tokens, i + 1, &mut map);
                }
                _ => i += 1,
            }
        }
        Self { map }
    }

    /// The Unicode string for a character code, if mapped.
    pub fn decode(&self, code: u32) -> Option<&str> {
        self.map.get(&code).map(String::as_str)
    }

    /// Whether the CMap mapped nothing useful.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Accumulate this CMap's affine offsets (`unicode - code`) over its
    /// single-scalar, printable-ASCII entries into `acc`. Drives the
    /// document-wide offset used by [`Self::infer_ascii_gaps`] to recover sparse
    /// subset CMaps; the histogram's dominant key is that offset.
    pub fn accumulate_ascii_deltas(&self, acc: &mut BTreeMap<i64, u32>) {
        for (&code, text) in &self.map {
            if let Some(scalar) = single_scalar(text) {
                if (0x20..=0x7E).contains(&scalar) {
                    *acc.entry(scalar as i64 - code as i64).or_default() += 1;
                }
            }
        }
    }

    /// Recover the codes a **broken** `/ToUnicode` omits, for subset fonts that
    /// assign glyph codes by a single affine offset `unicode = code + delta` over
    /// the printable-ASCII run but whose producer only emitted entries for the
    /// glyphs it happened to touch (common in Mac-produced PDFs whose embedded
    /// TrueType ships no `cmap`/`post`, leaving `/ToUnicode` the *only* mapping —
    /// and an incomplete one).
    ///
    /// The fix is self-calibrating: it infers `delta` from this font's *own*
    /// existing entries (the dominant `unicode - code` among single-scalar
    /// ASCII-printable targets), and only when that delta is **strongly
    /// dominant** (covers nearly all such entries, with enough samples) does it
    /// fill the gaps. Filled codes are bounded to the run actually covered and to
    /// printable-ASCII targets, so it never extrapolates wildly or touches a font
    /// with a normal (non-offset) encoding. Existing entries are never changed.
    ///
    /// `doc_delta` is an optional **document-wide** affine offset (the dominant
    /// `unicode - code` across every subset `/ToUnicode` in the file). A sparse
    /// font — too few entries to self-calibrate (e.g. a 2-entry CMap) — borrows it
    /// **only when its own handful of entries is consistent with it** (none
    /// contradicts), so a genuinely different font is never mis-filled.
    ///
    /// `reserved` lists character codes that the font's `/Encoding`+`/Differences`
    /// resolves **authoritatively** (a glyph repacked onto an ASCII slot, e.g.
    /// `n`@0x25, `p`@0x26). The gap-filler must never invent a `code → chr(code)`
    /// mapping for such a code: that synthetic `/ToUnicode` entry would be consulted
    /// *before* `/Encoding`+`/Differences` (see [`Self`]'s decoder) and mask the
    /// correct glyph, corrupting extraction (`'parents'` → `'&are%ts'`). These codes
    /// are therefore skipped, leaving the decoder to resolve them via `/Differences`.
    ///
    /// No-op for well-formed CMaps: a complete `/ToUnicode` has no gaps among the
    /// codes it covers, so nothing is added.
    pub fn infer_ascii_gaps(&mut self, doc_delta: Option<i64>, reserved: &BTreeSet<u32>) {
        // Tally the delta of every single-scalar, ASCII-printable entry.
        let mut deltas: BTreeMap<i64, u32> = BTreeMap::new();
        let mut min_code = u32::MAX;
        let mut max_code = 0u32;
        let mut ascii_entries = 0u32;
        for (&code, text) in &self.map {
            let Some(scalar) = single_scalar(text) else {
                continue;
            };
            min_code = min_code.min(code);
            max_code = max_code.max(code);
            if (0x20..=0x7E).contains(&scalar) {
                *deltas.entry(scalar as i64 - code as i64).or_default() += 1;
                ascii_entries += 1;
            }
        }
        if ascii_entries == 0 {
            return;
        }
        let own_dominant = deltas
            .iter()
            .max_by_key(|(_, &n)| n)
            .filter(|(_, &hits)| hits as f64 >= 0.8 * ascii_entries as f64)
            .map(|(&d, _)| d);

        // Choose the delta to extrapolate with:
        // - a strong self-calibrated delta (≥8 own samples) is most authoritative;
        // - otherwise the document-wide delta, but only if this font's entries
        //   don't contradict it (every own ASCII entry already matches it).
        let delta = match own_dominant {
            Some(d) if ascii_entries >= 8 => d,
            _ => {
                let Some(d) = doc_delta else { return };
                let consistent = self
                    .map
                    .iter()
                    .filter_map(|(&code, text)| single_scalar(text).map(|s| (code, s)))
                    .filter(|&(_, s)| (0x20..=0x7E).contains(&s))
                    .all(|(code, s)| s as i64 - code as i64 == d);
                if !consistent {
                    return;
                }
                d
            }
        };

        let _ = (min_code, max_code);
        // Fill every unmapped code whose `code + delta` lands on printable ASCII or
        // the space (0x20..=0x7E — skip the C0/DEL controls). The code range is
        // exactly the one that produces those scalars under `delta`, so a sparse
        // font (e.g. only `O`/`S` mapped) still recovers its whole ASCII alphabet
        // from the document-wide offset. Unmapped codes the content never uses are
        // inert; only the broken-but-used ones are what we rescue.
        let lo = (0x20i64 - delta).max(0) as u32;
        let hi = match u32::try_from(0x7Ei64 - delta) {
            Ok(h) => h,
            Err(_) => return,
        };
        for code in lo..=hi {
            // Skip codes already mapped, and codes the font's `/Encoding`+
            // `/Differences` owns authoritatively (a glyph repacked onto an ASCII
            // slot): inventing `code → chr(code)` for them would mask the real
            // glyph at decode time, since `/ToUnicode` is consulted first.
            if self.map.contains_key(&code) || reserved.contains(&code) {
                continue;
            }
            let scalar = code as i64 + delta;
            if let Some(c) = char::from_u32(scalar as u32) {
                self.map.insert(code, c.to_string());
            }
        }
    }
}

/// The single Unicode scalar a CMap target encodes, or `None` when it is empty
/// or a multi-character string (a ligature — never part of the affine ASCII run).
fn single_scalar(text: &str) -> Option<u32> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c as u32),
        _ => None,
    }
}

/// `<src> <dst>` pairs until `endbfchar`. Returns the index past the block.
fn parse_bfchar(tokens: &[Token], mut i: usize, map: &mut BTreeMap<u32, String>) -> usize {
    while i < tokens.len() {
        if matches!(&tokens[i], Token::Keyword(k) if k == b"endbfchar") {
            return i + 1;
        }
        if let (Some(Token::HexString(src)), Some(Token::HexString(dst))) =
            (tokens.get(i), tokens.get(i + 1))
        {
            if let Some(code) = bytes_to_code(src) {
                let text = utf16be_to_string(dst);
                if !text.is_empty() {
                    map.insert(code, text);
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

/// `<lo> <hi> <dst>` or `<lo> <hi> [<d0> <d1> …]` triples until `endbfrange`.
fn parse_bfrange(tokens: &[Token], mut i: usize, map: &mut BTreeMap<u32, String>) -> usize {
    while i < tokens.len() {
        if matches!(&tokens[i], Token::Keyword(k) if k == b"endbfrange") {
            return i + 1;
        }
        let (lo, hi) = match (tokens.get(i), tokens.get(i + 1)) {
            (Some(Token::HexString(lo)), Some(Token::HexString(hi))) => {
                (bytes_to_code(lo), bytes_to_code(hi))
            }
            _ => {
                i += 1;
                continue;
            }
        };
        let (lo, hi) = match (lo, hi) {
            (Some(lo), Some(hi)) if hi >= lo => (lo, hi),
            _ => {
                i += 3;
                continue;
            }
        };
        match tokens.get(i + 2) {
            // Contiguous: dst is the Unicode of `lo`, incrementing per code.
            Some(Token::HexString(dst)) => {
                if let Some(start) = utf16be_to_scalar(dst) {
                    let span = (hi - lo).min(0xFFFF);
                    for offset in 0..=span {
                        if let Some(c) = char::from_u32(start + offset) {
                            map.insert(lo + offset, c.to_string());
                        }
                    }
                } else {
                    let text = utf16be_to_string(dst);
                    if !text.is_empty() {
                        map.insert(lo, text);
                    }
                }
                i += 3;
            }
            // Explicit per-code targets.
            Some(Token::ArrayOpen) => {
                let mut j = i + 3;
                let mut code = lo;
                while j < tokens.len() {
                    match &tokens[j] {
                        Token::ArrayClose => {
                            j += 1;
                            break;
                        }
                        Token::HexString(dst) => {
                            let text = utf16be_to_string(dst);
                            if !text.is_empty() {
                                map.insert(code, text);
                            }
                            code = code.wrapping_add(1);
                            j += 1;
                        }
                        _ => j += 1,
                    }
                }
                i = j;
            }
            _ => i += 3,
        }
    }
    i
}

/// Big-endian code from 1–4 source bytes (`<0041>` → `0x41`).
fn bytes_to_code(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.len() > 4 {
        return None;
    }
    Some(bytes.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32))
}

/// Decode raw UTF-16BE bytes (no BOM) to a String, handling surrogate pairs.
fn utf16be_to_string(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let unit = ((bytes[i] as u16) << 8) | bytes[i + 1] as u16;
        i += 2;
        if (0xD800..=0xDBFF).contains(&unit) && i + 1 < bytes.len() {
            let low = ((bytes[i] as u16) << 8) | bytes[i + 1] as u16;
            if (0xDC00..=0xDFFF).contains(&low) {
                i += 2;
                let scalar = 0x10000 + (((unit - 0xD800) as u32) << 10) + (low - 0xDC00) as u32;
                if let Some(c) = char::from_u32(scalar) {
                    out.push(c);
                }
                continue;
            }
        }
        if let Some(c) = char::from_u32(unit as u32) {
            out.push(c);
        }
    }
    out
}

/// Single scalar value for a `bfrange` start (one BMP unit or one surrogate
/// pair). `None` when the destination spans several characters (then the caller
/// only maps the `lo` code as a string).
fn utf16be_to_scalar(bytes: &[u8]) -> Option<u32> {
    match bytes.len() {
        2 => {
            let unit = ((bytes[0] as u16) << 8) | bytes[1] as u16;
            (!(0xD800..=0xDFFF).contains(&unit)).then_some(unit as u32)
        }
        4 => {
            let hi = ((bytes[0] as u16) << 8) | bytes[1] as u16;
            let lo = ((bytes[2] as u16) << 8) | bytes[3] as u16;
            if (0xD800..=0xDBFF).contains(&hi) && (0xDC00..=0xDFFF).contains(&lo) {
                Some(0x10000 + (((hi - 0xD800) as u32) << 10) + (lo - 0xDC00) as u32)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A composite-font `/Encoding` CMap mapping character **codes** to **CIDs**
/// (ISO 32000-1 §9.7.5). A Type0 font's text-show bytes are split into codes by
/// the CMap's `codespacerange`s, then each code resolves to a CID via its
/// `cidrange`/`cidchar` entries. The CID is *not* the glyph id: a non-Identity
/// `/CIDToGIDMap` then maps CID → glyph id (`/W` widths are likewise keyed by
/// CID). `Identity-H`/`Identity-V` are the trivial code == CID case and need no
/// CMap at all (the decoder's `code_to_cid` stays `None`).
///
/// Only the **2-byte** code path is modelled: every predefined CMap a Type0
/// `/Encoding` actually uses in the wild for whole-document text is 2-byte
/// (`UniGB-UCS2-H`, `UniJIS-UCS2-H`, `UniKS-UCS2-H`, `GBK-EUC-H`, the UTF16
/// families, …). Mixed 1/2-byte CMaps (`*-RKSJ-*` Shift-JIS) keep only their
/// 2-byte ranges — the single-byte ASCII half is deferred (it would need the
/// content layer's atom splitter to vary code length, out of read-fidelity
/// scope) — so their CJK glyphs still resolve while ASCII falls back to the
/// raw-code path.
#[derive(Debug, Clone, Default)]
pub struct Cmap {
    /// `[lo, hi]` ranges of 2-byte codes the CMap defines (its `codespacerange`).
    /// Empty ⇒ assume the whole 2-byte space (a stream that omitted the block).
    codespace: Vec<(u16, u16)>,
    /// Single `code → CID` assignments (`cidchar`), highest precedence.
    singles: BTreeMap<u16, u16>,
    /// `[lo, hi] → first_cid` contiguous spans (`cidrange`); CID is
    /// `first_cid + (code - lo)`.
    ranges: Vec<(u16, u16, u16)>,
    /// Writing mode (ISO 32000-1 §9.7.4.3): `0` = horizontal (advance along the
    /// x-axis, the default), `1` = vertical (advance top-to-bottom using the
    /// `/W2`/`/DW2` metrics). Set from the predefined CMap name (`*-V`) or the
    /// embedded CMap stream's `/WMode`. Carried here because the `/Encoding` CMap
    /// is the one composite-font datum already threaded into a font's
    /// [`TextDecoder`] (`code_to_cid`), so the layout layer reads the writing mode
    /// from it with no extra plumbing — `Identity-V` therefore yields `Some` (an
    /// identity CMap tagged vertical) rather than the `None` of `Identity-H`.
    wmode: u8,
}

impl Cmap {
    /// Parse a decoded embedded `/Encoding` CMap stream (the `begincodespacerange`
    /// / `begincidrange` / `begincidchar` blocks). Lexed with the shared [`Lexer`]
    /// — no new tokenizer, zero dependencies. Unknown constructs are skipped (a
    /// partial map still beats treating the raw code as the CID).
    pub fn parse(data: &[u8]) -> Self {
        let mut lexer = Lexer::new(data);
        let mut tokens = Vec::new();
        while let Ok(token) = lexer.next_token() {
            if matches!(token, Token::Eof) {
                break;
            }
            tokens.push(token);
        }

        let mut cmap = Cmap::default();
        let mut i = 0;
        while i < tokens.len() {
            match &tokens[i] {
                Token::Keyword(k) if k == b"begincodespacerange" => {
                    i = cmap.parse_codespace(&tokens, i + 1);
                }
                Token::Keyword(k) if k == b"begincidrange" => {
                    i = cmap.parse_cidrange(&tokens, i + 1);
                }
                Token::Keyword(k) if k == b"begincidchar" => {
                    i = cmap.parse_cidchar(&tokens, i + 1);
                }
                // `/WMode 1 def` (ISO 32000-1 §9.7.5.1): a leading `1` declares
                // vertical writing. The dict header writes it as `/WMode <int> def`;
                // any value other than 1 (or its absence) leaves the default 0.
                Token::Name(n) if n == b"WMode" => {
                    if let Some(&Token::Integer(m)) = tokens.get(i + 1) {
                        cmap.wmode = u8::try_from(m).unwrap_or(0);
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        cmap
    }

    /// Build a CMap for a **predefined** `/Encoding` name (ISO 32000-1 §9.7.5.2).
    /// `Identity-H` ⇒ `None` (code == CID, horizontal, the zero-overhead no-op);
    /// `Identity-V` ⇒ `Some` of an identity CMap **tagged vertical** (code == CID,
    /// but the writing mode must reach the layout layer). The recognised CJK
    /// families map their whole 2-byte codespace identically (code == CID over the
    /// codespace): the predefined Adobe CMaps assign a distinct CID per code, and
    /// without the bundled Adobe CMap resources the faithful, reversible choice is
    /// identity over the byte codespace — it keeps the per-code `/W` widths and the
    /// `/CIDToGIDMap` indexing correct, which is what read fidelity needs. Their
    /// `-V` spelling sets [`wmode`](Self::wmode) = 1 so vertical text lays out
    /// top-to-bottom. `None` for an unrecognised name ⇒ caller falls back to the
    /// raw-code path.
    pub fn predefined(name: &[u8]) -> Option<Self> {
        // A predefined CMap name ending in `-V` selects vertical writing
        // (ISO 32000-1 §9.7.5.2); every Adobe-Japan/GB/CNS/Korea family ships an
        // `-H`/`-V` pair. `Identity-V` is the trivial vertical case.
        let vertical = name.ends_with(b"-V");
        // Identity-H is the horizontal no-op: keep the decoder's raw-code path
        // (`None`). Identity-V must still travel as a (vertical) identity CMap.
        if name == b"Identity-H" || name == b"Identity" {
            return None;
        }
        if name == b"Identity-V" {
            return Some(Cmap::identity_with_wmode(1));
        }
        // Every supported predefined CMap here is 2-byte; map its full codespace
        // identically. `None` for a name we don't recognise.
        let two_byte = matches!(
            name,
            b"UniGB-UCS2-H"
                | b"UniGB-UCS2-V"
                | b"UniGB-UTF16-H"
                | b"UniGB-UTF16-V"
                | b"GBK-EUC-H"
                | b"GBK-EUC-V"
                | b"GBK2K-H"
                | b"GBK2K-V"
                | b"GBpc-EUC-H"
                | b"GBpc-EUC-V"
                | b"UniCNS-UCS2-H"
                | b"UniCNS-UCS2-V"
                | b"UniCNS-UTF16-H"
                | b"UniCNS-UTF16-V"
                | b"B5pc-H"
                | b"B5pc-V"
                | b"ETen-B5-H"
                | b"ETen-B5-V"
                | b"UniJIS-UCS2-H"
                | b"UniJIS-UCS2-V"
                | b"UniJIS-UCS2-HW-H"
                | b"UniJIS-UCS2-HW-V"
                | b"UniJIS-UTF16-H"
                | b"UniJIS-UTF16-V"
                | b"90ms-RKSJ-H"
                | b"90ms-RKSJ-V"
                | b"90pv-RKSJ-H"
                | b"Ext-RKSJ-H"
                | b"Ext-RKSJ-V"
                | b"UniKS-UCS2-H"
                | b"UniKS-UCS2-V"
                | b"UniKS-UTF16-H"
                | b"UniKS-UTF16-V"
                | b"KSC-EUC-H"
                | b"KSC-EUC-V"
                | b"KSCms-UHC-H"
                | b"KSCms-UHC-V"
        );
        two_byte.then(|| Cmap::identity_with_wmode(u8::from(vertical)))
    }

    /// An identity CMap (CID == code over the whole 2-byte codespace) with an
    /// explicit writing mode. Shared by the `Identity-V` and the `-V`/`-H`
    /// predefined CJK paths.
    fn identity_with_wmode(wmode: u8) -> Self {
        Cmap {
            codespace: vec![(0x0000, 0xFFFF)],
            singles: BTreeMap::new(),
            // Identity over the whole 2-byte codespace: CID == code.
            ranges: vec![(0x0000, 0xFFFF, 0x0000)],
            wmode,
        }
    }

    /// The CMap's writing mode: `0` horizontal, `1` vertical (ISO 32000-1
    /// §9.7.4.3). Horizontal for every CMap that did not declare `/WMode 1` or
    /// carry a `-V` predefined name.
    pub fn wmode(&self) -> u8 {
        self.wmode
    }

    /// Whether `code` falls inside one of the CMap's 2-byte `codespacerange`s.
    /// An empty codespace (a stream that omitted the block) admits every code.
    pub fn in_codespace(&self, code: u16) -> bool {
        self.codespace.is_empty()
            || self
                .codespace
                .iter()
                .any(|&(lo, hi)| code >= lo && code <= hi)
    }

    /// The CID for a 2-byte `code`: a `cidchar` single wins, then the first
    /// covering `cidrange`. `None` ⇒ the CMap maps this code nowhere (the caller
    /// then leaves the code unresolved rather than inventing a glyph).
    pub fn cid(&self, code: u16) -> Option<u16> {
        if let Some(&cid) = self.singles.get(&code) {
            return Some(cid);
        }
        for &(lo, hi, first) in &self.ranges {
            if code >= lo && code <= hi {
                return Some(first.wrapping_add(code - lo));
            }
        }
        None
    }

    /// `<lo> <hi>` codespace pairs until `endcodespacerange`. Only 2-byte ranges
    /// are kept (the modelled path); other widths are skipped.
    fn parse_codespace(&mut self, tokens: &[Token], mut i: usize) -> usize {
        while i < tokens.len() {
            if matches!(&tokens[i], Token::Keyword(k) if k == b"endcodespacerange") {
                return i + 1;
            }
            if let (Some(Token::HexString(lo)), Some(Token::HexString(hi))) =
                (tokens.get(i), tokens.get(i + 1))
            {
                if let (Some(lo), Some(hi)) = (hex_u16(lo), hex_u16(hi)) {
                    if hi >= lo {
                        self.codespace.push((lo, hi));
                    }
                }
                i += 2;
            } else {
                i += 1;
            }
        }
        i
    }

    /// `<lo> <hi> <cid|int>` triples until `endcidrange`. The CID target may be a
    /// hex string or an integer.
    fn parse_cidrange(&mut self, tokens: &[Token], mut i: usize) -> usize {
        while i < tokens.len() {
            if matches!(&tokens[i], Token::Keyword(k) if k == b"endcidrange") {
                return i + 1;
            }
            let lo = tokens.get(i).and_then(token_u16);
            let hi = tokens.get(i + 1).and_then(token_u16);
            let cid = tokens.get(i + 2).and_then(token_u16);
            if let (Some(lo), Some(hi), Some(cid)) = (lo, hi, cid) {
                if hi >= lo {
                    self.ranges.push((lo, hi, cid));
                }
                i += 3;
            } else {
                i += 1;
            }
        }
        i
    }

    /// `<code> <cid|int>` pairs until `endcidchar`.
    fn parse_cidchar(&mut self, tokens: &[Token], mut i: usize) -> usize {
        while i < tokens.len() {
            if matches!(&tokens[i], Token::Keyword(k) if k == b"endcidchar") {
                return i + 1;
            }
            let code = tokens.get(i).and_then(token_u16);
            let cid = tokens.get(i + 1).and_then(token_u16);
            if let (Some(code), Some(cid)) = (code, cid) {
                self.singles.insert(code, cid);
                i += 2;
            } else {
                i += 1;
            }
        }
        i
    }
}

/// A 1–2 byte big-endian hex string as a `u16` (`<0041>` → `0x41`). `None` for an
/// empty or over-wide string (a >2-byte code is outside the modelled path).
fn hex_u16(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() || bytes.len() > 2 {
        return None;
    }
    Some(bytes.iter().fold(0u16, |acc, &b| (acc << 8) | b as u16))
}

/// A CMap operand as a `u16`: either a hex string (`<0041>`) or a plain integer
/// (CIDs are written as integers in `cidrange`/`cidchar`). `None` when neither.
fn token_u16(token: &Token) -> Option<u16> {
    match token {
        Token::HexString(bytes) => hex_u16(bytes),
        Token::Integer(n) => u16::try_from(*n).ok(),
        _ => None,
    }
}

/// Decodes the bytes of a text-show operand to Unicode for one font.
#[derive(Debug, Clone, Default)]
pub struct TextDecoder {
    /// A composite (Type0) font addressed with 2-byte character codes.
    pub two_byte: bool,
    /// The font's `/ToUnicode` map, when the font carries one.
    pub to_unicode: Option<ToUnicode>,
    /// Per-code advance widths from the font's `/Widths` (simple) or `/W`+`/DW`
    /// (Type0) tables, when present — lets a text run be measured by real glyph
    /// advances instead of a 0.5-em estimate.
    pub widths: Option<CodeWidths>,
    /// For an Identity-H Type0 font: a glyph-id → Unicode map derived from the
    /// embedded font program's own `cmap` (and `post` glyph names). With Identity
    /// encoding the 2-byte code equals the glyph id, so this recovers real text
    /// (no tofu) where there's no `/ToUnicode` to consult **or where a partial
    /// `/ToUnicode` omits some codes** (subset fonts routinely do).
    pub cid_to_unicode: Option<std::collections::BTreeMap<u16, String>>,
    /// For a composite (Type0) font whose `/Encoding` is **not** `Identity-H`: a
    /// CMap mapping the 2-byte character **code** to a **CID** (predefined CJK
    /// CMap or embedded CMap stream). `None` ⇒ Identity (code == CID), the common
    /// case — and the existing zero-overhead path. Applied *before* `cid_to_gid`
    /// and the `/W` width lookup, both of which are keyed by CID.
    pub code_to_cid: Option<Cmap>,
    /// For a composite font with a non-Identity `/CIDToGIDMap` stream: CID →
    /// glyph-id (the stream is 2 bytes per CID, indexed by CID). `None` ⇒ Identity
    /// (CID == glyph id). Resolves the glyph id that `cid_to_unicode` is keyed by,
    /// so text extracts correctly even when the font reorders glyphs.
    pub cid_to_gid: Option<std::vec::Vec<u16>>,
    /// For a **simple** (single-byte, non-CID) font: a character-code → Unicode
    /// map resolved from the font's base `/Encoding`
    /// (`WinAnsiEncoding`/`MacRomanEncoding`/`StandardEncoding`/the font's
    /// built-in) overlaid with its `/Encoding` `/Differences` (each code → glyph
    /// name → Unicode via the Adobe Glyph List). This is the spec resolution order
    /// (ISO 32000-1 §9.10.2) for simple fonts that ship no `/ToUnicode`; it
    /// supersedes the hard-coded WinAnsi fallback (wrong for MacRoman-encoded and
    /// custom-`/Differences` fonts). `None` ⇒ fall back to WinAnsi.
    pub simple_encoding: Option<std::collections::BTreeMap<u8, String>>,
    /// For a composite (Type0) font in **vertical** writing mode: the CIDFont's
    /// `/W2` per-CID vertical metrics and `/DW2` default (ISO 32000-1 §9.7.4.3).
    /// `None` ⇒ no `/W2`/`/DW2` were given, so the spec defaults apply (vertical
    /// advance −1000 ‰, position vector `[w0/2, 880]` ‰). Consulted only when the
    /// `/Encoding` CMap's [`Cmap::wmode`] is 1; horizontal fonts ignore it.
    pub vmetrics: Option<VerticalMetrics>,
}

/// Per-character-code advance widths in PDF glyph space (1000 units = 1 em),
/// from a font's `/Widths` or `/W` table. `default` is the missing-width / `/DW`
/// fallback applied to codes absent from the map.
#[derive(Debug, Clone, Default)]
pub struct CodeWidths {
    map: std::collections::BTreeMap<u32, f64>,
    default: f64,
}

impl CodeWidths {
    /// Build from a code→advance map (1000-em units) and a default width.
    pub fn new(map: std::collections::BTreeMap<u32, f64>, default: f64) -> Self {
        Self { map, default }
    }

    /// The advance of `code` in 1000-em units, falling back to the default.
    pub fn advance(&self, code: u32) -> f64 {
        self.map.get(&code).copied().unwrap_or(self.default)
    }
}

/// One CID's vertical metrics (`/W2` entry, ISO 32000-1 §9.7.4.3), all in PDF
/// glyph space (1000 units = 1 em): `w1y` is the glyph's **vertical
/// displacement** (the pen step along −y when writing top-to-bottom, normally
/// negative); `v = (vx, vy)` is the **position vector** from the glyph's
/// horizontal origin (origin 0, where the pen sits) to its vertical origin
/// (origin 1, the top-centre the glyph hangs from).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VMetric {
    /// Vertical displacement `w1y` (glyph-space, normally negative).
    pub w1y: f64,
    /// Position-vector x `vx` (glyph-space; spec default `w0 / 2`).
    pub vx: f64,
    /// Position-vector y `vy` (glyph-space; `/DW2[0]`, default 880).
    pub vy: f64,
}

/// A composite font's vertical writing metrics: the `/DW2` default plus per-CID
/// `/W2` overrides (ISO 32000-1 §9.7.4.3). `/DW2` is `[vy w1y]`; its `vx` is not
/// stored (the spec fixes the default position vector's x at half the glyph's
/// **horizontal** width `w0`, which lives in the font's `/W`/`CodeWidths`).
#[derive(Debug, Clone, Default)]
pub struct VerticalMetrics {
    /// `/DW2[1]` — default vertical displacement `w1y` (spec default −1000).
    dw2_w1y: f64,
    /// `/DW2[0]` — default position-vector y `vy` (spec default 880).
    dw2_vy: f64,
    /// `/W2` per-CID overrides (`w1y`, `vx`, `vy`).
    map: std::collections::BTreeMap<u32, VMetric>,
}

impl VerticalMetrics {
    /// The PDF defaults when a composite font supplies no `/DW2`: vertical
    /// displacement −1000 ‰ (one em downward) and position-vector y 880 ‰.
    pub const DEFAULT_DW2_W1Y: f64 = -1000.0;
    /// Default `/DW2[0]` (position-vector y), 880 ‰.
    pub const DEFAULT_DW2_VY: f64 = 880.0;

    /// Build from a `/DW2` (`[vy, w1y]`) and a parsed `/W2` map. A `None` `dw2`
    /// uses the spec defaults `[880, −1000]`.
    pub fn new(dw2: Option<(f64, f64)>, map: std::collections::BTreeMap<u32, VMetric>) -> Self {
        let (dw2_vy, dw2_w1y) = dw2.unwrap_or((Self::DEFAULT_DW2_VY, Self::DEFAULT_DW2_W1Y));
        Self {
            dw2_w1y,
            dw2_vy,
            map,
        }
    }

    /// The vertical metrics for `cid`, in glyph space (1000 units = 1 em). A `/W2`
    /// override wins; otherwise the `/DW2` default with the position vector's x set
    /// to `w0 / 2` (`w0` = the CID's **horizontal** advance, passed by the caller
    /// from the font's `/W`/`/DW`, so the glyph centres on the vertical baseline).
    pub fn metric(&self, cid: u32, w0: f64) -> VMetric {
        match self.map.get(&cid) {
            Some(&m) => m,
            None => VMetric {
                w1y: self.dw2_w1y,
                vx: w0 / 2.0,
                vy: self.dw2_vy,
            },
        }
    }
}

impl TextDecoder {
    /// The default decoder: a single-byte WinAnsi font.
    pub fn winansi() -> Self {
        Self::default()
    }

    /// The advance of one text-show string in user-space points, summed from the
    /// real per-glyph widths. `None` when this font carries no width table (the
    /// caller then falls back to an average-advance estimate).
    pub fn string_advance(&self, bytes: &[u8], font_size: f64) -> Option<f64> {
        let widths = self.widths.as_ref()?;
        let mut units = 0.0;
        if self.two_byte {
            let mut i = 0;
            while i + 1 < bytes.len() {
                let code = ((bytes[i] as u16) << 8) | bytes[i + 1] as u16;
                // `/W`/`/DW` are keyed by **CID**, not the raw code: map the code
                // through the `/Encoding` CMap first (Identity ⇒ CID == code).
                units += widths.advance(self.code_to_cid_id(code) as u32);
                i += 2;
            }
        } else {
            for &b in bytes {
                units += widths.advance(b as u32);
            }
        }
        Some(units * font_size / 1000.0)
    }

    /// The font's writing mode: `1` (vertical) when its composite `/Encoding`
    /// CMap declared it (`*-V` / `Identity-V` / `/WMode 1`), else `0`
    /// (horizontal). Only composite fonts carry a CMap, so a simple font is
    /// always horizontal here (ISO 32000-1 §9.7.4.3).
    pub fn wmode(&self) -> u8 {
        self.code_to_cid.as_ref().map_or(0, Cmap::wmode)
    }

    /// The horizontal advance `w0` of a CID in glyph space (1000 = 1 em), from the
    /// font's `/W`/`/DW` table — the source of the default vertical position
    /// vector's x (`w0 / 2`). Falls back to one em (1000) when the font carries no
    /// width table, matching the `/DW` spec default.
    fn cid_w0(&self, cid: u16) -> f64 {
        self.widths
            .as_ref()
            .map_or(1000.0, |w| w.advance(cid as u32))
    }

    /// One glyph's **vertical** layout (composite font, vertical writing mode):
    /// the vertical displacement `w1y` and position vector `(vx, vy)` in
    /// **user-space points**, for the 2-byte `code`. `w1y` is the pen step along
    /// the text-space y-axis (normally negative — top to bottom); the position
    /// vector offsets the glyph from the pen so it hangs centred on the vertical
    /// baseline (ISO 32000-1 §9.7.4.3). Uses a `/W2` override when present, else
    /// the `/DW2` default with `vx = w0 / 2`.
    pub fn vertical_metric(&self, code: u16, font_size: f64) -> (f64, f64, f64) {
        let cid = self.code_to_cid_id(code);
        let w0 = self.cid_w0(cid);
        let vm = self
            .vmetrics
            .as_ref()
            .map(|v| v.metric(cid as u32, w0))
            .unwrap_or(VMetric {
                w1y: VerticalMetrics::DEFAULT_DW2_W1Y,
                vx: w0 / 2.0,
                vy: VerticalMetrics::DEFAULT_DW2_VY,
            });
        let s = font_size / 1000.0;
        (vm.w1y * s, vm.vx * s, vm.vy * s)
    }

    /// The total **vertical** advance of a text-show string in user-space points:
    /// the sum of each 2-byte code's vertical displacement `w1y` (normally
    /// negative). Returns `0.0` for a non-composite font (vertical mode requires a
    /// composite font). The sign is preserved so the caller advances the text
    /// matrix downward (along −y).
    pub fn string_vertical_advance(&self, bytes: &[u8], font_size: f64) -> f64 {
        if !self.two_byte {
            return 0.0;
        }
        let mut total = 0.0;
        let mut i = 0;
        while i + 1 < bytes.len() {
            let code = ((bytes[i] as u16) << 8) | bytes[i + 1] as u16;
            total += self.vertical_metric(code, font_size).0;
            i += 2;
        }
        total
    }

    /// Decode one text-show string to Unicode.
    ///
    /// Resolution order per code (ISO 32000-1 §9.10):
    /// - composite (2-byte): `/ToUnicode` → embedded-cmap `cid_to_unicode`
    ///   fallback (covers codes a **partial** `/ToUnicode` omits — subset fonts
    ///   routinely do) → **nothing** (a code no source maps is unrecoverable;
    ///   the glyph draws but its Unicode exists nowhere — drop it silently, as
    ///   reference extractors do, never a `U+FFFD` tofu placeholder).
    /// - simple (1-byte): `/ToUnicode` → base-`/Encoding` + `/Differences`
    ///   (`simple_encoding`) → WinAnsi last-resort.
    pub fn decode(&self, bytes: &[u8]) -> String {
        if self.two_byte {
            let mut out = String::new();
            let mut i = 0;
            while i + 1 < bytes.len() {
                let code = ((bytes[i] as u16) << 8) | bytes[i + 1] as u16;
                i += 2;
                if let Some(text) = self.decode_two_byte(code) {
                    out.push_str(text);
                }
                // A 2-byte code that **no** source maps (a partial `/ToUnicode`
                // with no embedded `cmap`/`post` to fill the gap — the subset
                // Hebrew/CJK fonts whose `FontFile2` ships only `glyf`/`loca`)
                // is genuinely unrecoverable: the glyph draws, but its Unicode
                // exists nowhere in the file. Emit **nothing** for it — never a
                // `U+FFFD` placeholder — mirroring the simple-font empty-glyph
                // rule and every reference extractor (pdftotext/Adobe drop such
                // CIDs silently). A tofu character helps no consumer.
            }
            out
        } else {
            let mut out = String::new();
            for &b in bytes {
                self.decode_one_byte(b, &mut out);
            }
            out
        }
    }

    /// Unicode for one 2-byte composite code: `/ToUnicode` first (keyed by the raw
    /// code, as the CMap is), then the embedded-program fallback keyed by **glyph
    /// id**. The glyph id is recovered code → CID (`/Encoding` CMap; Identity ⇒
    /// code == CID) → glyph id (`/CIDToGIDMap`; Identity ⇒ CID == glyph id), so a
    /// predefined CJK CMap and/or a reordering `/CIDToGIDMap` resolve to the same
    /// glyph the rasterizer draws. `None` ⇒ no source maps the code.
    fn decode_two_byte(&self, code: u16) -> Option<&str> {
        if let Some(text) = self.to_unicode.as_ref().and_then(|c| c.decode(code as u32)) {
            return Some(text);
        }
        let gid = self.code_to_gid_id(code);
        self.cid_to_unicode
            .as_ref()
            .and_then(|m| m.get(&gid))
            .map(String::as_str)
    }

    /// Map a 2-byte character code to its CID through the `/Encoding` CMap. With
    /// no CMap (Identity-H) the CID is the code itself. A code outside the CMap's
    /// codespace or unmapped by it falls back to the raw code (a conservative
    /// best-effort that preserves the Identity behaviour for the predefined
    /// CMaps' identity codespace).
    fn code_to_cid_id(&self, code: u16) -> u16 {
        match &self.code_to_cid {
            Some(cmap) => cmap.cid(code).unwrap_or(code),
            None => code,
        }
    }

    /// Map a 2-byte character code to a glyph id: code → CID (`/Encoding` CMap) →
    /// glyph id (non-Identity `/CIDToGIDMap`). Either step is identity when its
    /// table is absent. A CID past the end of the `/CIDToGIDMap` ⇒ glyph 0
    /// (`.notdef`), exactly as the rasterizer treats it.
    fn code_to_gid_id(&self, code: u16) -> u16 {
        let cid = self.code_to_cid_id(code);
        match &self.cid_to_gid {
            Some(map) => map.get(cid as usize).copied().unwrap_or(0),
            None => cid,
        }
    }

    /// Append the Unicode for one single-byte simple-font code: `/ToUnicode`,
    /// then the base-`/Encoding`+`/Differences` map, then WinAnsi as a last
    /// resort. Each source may yield a multi-character string (e.g. a ligature).
    ///
    /// Two rules keep subset fonts from emitting tofu:
    /// - An entry **present** in `simple_encoding` is *authoritative* (the
    ///   font's `/Encoding`/`/Differences` explicitly assigned this code). Its
    ///   value may be the empty string — a glyph that carries no Unicode (an
    ///   opaque `/gNN` subset name) — in which case the code contributes **no
    ///   text** and we do **not** fall back to WinAnsi (which would invent a
    ///   wrong letter or leak a control byte).
    /// - The WinAnsi last resort (for codes no map covers) **skips control
    ///   characters**: a C0/C1 byte with no encoding mapping is never visible
    ///   text, so emitting its raw scalar only produces dingbats.
    fn decode_one_byte(&self, b: u8, out: &mut String) {
        if let Some(text) = self.to_unicode.as_ref().and_then(|c| c.decode(b as u32)) {
            out.push_str(text);
            return;
        }
        if let Some(text) = self.simple_encoding.as_ref().and_then(|m| m.get(&b)) {
            // Present ⇒ authoritative (may be "" = explicitly no text). Stop here.
            out.push_str(text);
            return;
        }
        let c = super::winansi_to_char(b);
        if !c.is_control() {
            out.push(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn parses_bfchar_pairs() {
        let cmap = ToUnicode::parse(b"beginbfchar <01> <0041> <02> <00E9> endbfchar");
        assert_eq!(cmap.decode(0x01), Some("A"));
        assert_eq!(cmap.decode(0x02), Some("\u{00E9}")); // é
        assert_eq!(cmap.decode(0x03), None);
    }

    #[test]
    fn parses_contiguous_bfrange() {
        // codes 0x10..=0x12 map to 'A','B','C'.
        let cmap = ToUnicode::parse(b"beginbfrange <10> <12> <0041> endbfrange");
        assert_eq!(cmap.decode(0x10), Some("A"));
        assert_eq!(cmap.decode(0x11), Some("B"));
        assert_eq!(cmap.decode(0x12), Some("C"));
    }

    #[test]
    fn parses_array_bfrange() {
        let cmap = ToUnicode::parse(b"beginbfrange <20> <22> [<0058> <0059> <005A>] endbfrange");
        assert_eq!(cmap.decode(0x20), Some("X"));
        assert_eq!(cmap.decode(0x21), Some("Y"));
        assert_eq!(cmap.decode(0x22), Some("Z"));
    }

    #[test]
    fn two_byte_decoder_uses_cmap() {
        let cmap = ToUnicode::parse(b"beginbfchar <0041> <00C9> endbfchar");
        let decoder = TextDecoder {
            two_byte: true,
            to_unicode: Some(cmap),
            widths: None,
            cid_to_unicode: None,
            code_to_cid: None,
            cid_to_gid: None,
            simple_encoding: None,
            vmetrics: None,
        };
        // One 2-byte code 0x0041 → 'É'.
        assert_eq!(decoder.decode(&[0x00, 0x41]), "\u{00C9}");
    }

    #[test]
    fn string_advance_sums_real_widths() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(b'A' as u32, 600.0);
        map.insert(b'B' as u32, 700.0);
        let decoder = TextDecoder {
            two_byte: false,
            to_unicode: None,
            widths: Some(CodeWidths::new(map, 500.0)),
            cid_to_unicode: None,
            code_to_cid: None,
            cid_to_gid: None,
            simple_encoding: None,
            vmetrics: None,
        };
        // "AB?" → 600 + 700 + 500 (default) = 1800 units × 12/1000 = 21.6 pt.
        assert_eq!(decoder.string_advance(b"AB?", 12.0), Some(21.6));
        // No width table → None, so the caller falls back to an estimate.
        assert_eq!(TextDecoder::winansi().string_advance(b"AB", 12.0), None);
    }

    #[test]
    fn surrogate_pair_round_trips() {
        // U+1F600 in UTF-16BE is D83D DE00.
        let cmap = ToUnicode::parse(b"beginbfchar <01> <D83DDE00> endbfchar");
        assert_eq!(cmap.decode(0x01), Some("\u{1F600}"));
    }

    // ── broken-subset `/ToUnicode` recovery (the s1106 bug class) ──────────────

    #[test]
    fn infer_ascii_gaps_self_calibrated() {
        // A subset that maps glyph code = unicode − 0x1D, but the producer only
        // emitted a few letters — `J`(0x2d), the apostrophe(0x0a) and `k`(0x2e)
        // are absent. With ≥8 own samples the dominant delta (0x1D) self-fills.
        let mut cmap = ToUnicode::parse(
            b"beginbfrange \
              <0024> <0024> <0041> \
              <0044> <004c> <0061> \
              <004f> <0059> <006c> \
              endbfrange",
        ); // A, a..i, l..v
        assert_eq!(cmap.decode(0x2d), None); // J unmapped before
        cmap.infer_ascii_gaps(None, &BTreeSet::new());
        assert_eq!(cmap.decode(0x2d), Some("J")); // 0x2d + 0x1D = 0x4A
        assert_eq!(cmap.decode(0x0a), Some("'")); // quote slot recovered
        assert_eq!(cmap.decode(0x03), Some(" ")); // space recovered
                                                  // Existing entries untouched.
        assert_eq!(cmap.decode(0x24), Some("A"));
        assert_eq!(cmap.decode(0x44), Some("a"));
    }

    #[test]
    fn infer_ascii_gaps_borrows_document_delta_when_sparse() {
        // A near-empty CMap (only O,S) cannot self-calibrate, but borrows the
        // document-wide delta because its two entries are consistent with it.
        let mut cmap =
            ToUnicode::parse(b"beginbfrange <0032> <0032> <004f> <0036> <0036> <0053> endbfrange");
        cmap.infer_ascii_gaps(Some(0x1D), &BTreeSet::new());
        assert_eq!(cmap.decode(0x44), Some("a")); // recovered via doc delta
        assert_eq!(cmap.decode(0x2d), Some("J"));
        assert_eq!(cmap.decode(0x32), Some("O")); // own entry kept
    }

    #[test]
    fn infer_ascii_gaps_rejects_contradictory_document_delta() {
        // A sparse CMap whose own entry contradicts the document delta must NOT
        // borrow it (different font / encoding) — no gaps filled.
        let mut cmap = ToUnicode::parse(b"beginbfchar <0041> <0041> <0042> <0042> endbfchar"); // identity: A,B
        cmap.infer_ascii_gaps(Some(0x1D), &BTreeSet::new()); // doc delta would map 0x41→0x5E (^)
        assert_eq!(cmap.decode(0x41), Some("A")); // unchanged
        assert_eq!(cmap.decode(0x2d), None); // not filled — delta contradicted
    }

    #[test]
    fn infer_ascii_gaps_noop_on_complete_cmap() {
        // A well-formed CMap with no contiguous-run gaps gains nothing spurious.
        let mut cmap = ToUnicode::parse(b"beginbfchar <0041> <0041> endbfchar");
        cmap.infer_ascii_gaps(Some(0x00), &BTreeSet::new());
        // Only the inert range fills around the single entry; the entry stands.
        assert_eq!(cmap.decode(0x41), Some("A"));
    }

    #[test]
    fn infer_ascii_gaps_skips_reserved_codes() {
        // The s3705 bug class: an identity-aligned (delta 0) `/ToUnicode` whose
        // producer OMITS codes that the font's `/Encoding`+`/Differences` actually
        // repacks onto ASCII-punctuation slots (e.g. `p`@0x26). The gap-filler must
        // not invent `0x26 → chr(0x26)` ('&'), which would mask the real glyph at
        // decode time (`/ToUnicode` is consulted before `/Differences`).
        //
        // Source: a..u (0x61..0x75) at delta 0 — ≥8 samples, self-calibrates to 0.
        const CMAP: &[u8] = b"beginbfrange <61> <75> <0061> endbfrange";

        // WITHOUT a reservation the filler injects the bug ('&' at 0x26).
        let mut unguarded = ToUnicode::parse(CMAP);
        unguarded.infer_ascii_gaps(None, &BTreeSet::new());
        assert_eq!(
            unguarded.decode(0x26),
            Some("&"),
            "baseline: gap-filler invents chr(0x26) when nothing is reserved"
        );

        // WITH 0x26 reserved (it is `/Differences`-owned) the filler leaves it
        // unmapped, so the decoder will fall through to `/Encoding`+`/Differences`.
        let mut reserved = BTreeSet::new();
        reserved.insert(0x26u32);
        let mut guarded = ToUnicode::parse(CMAP);
        guarded.infer_ascii_gaps(None, &reserved);
        assert_eq!(
            guarded.decode(0x26),
            None,
            "reserved code must stay unmapped"
        );
        // Real entries (and non-reserved gaps) are unaffected.
        assert_eq!(guarded.decode(0x62), Some("b")); // real entry, untouched
        assert_eq!(guarded.decode(0x25), Some("%")); // non-reserved gap still fills
    }

    // ── simple-font fallbacks (MacRoman base + `/Differences`) ─────────────────

    #[test]
    fn simple_decoder_uses_macroman_then_differences() {
        // MacRoman base (0x88 = à, 0xD5 = ’) plus a `/Differences` override that
        // maps 0x41 to `agrave`. No `/ToUnicode`.
        let mut enc = std::collections::BTreeMap::new();
        enc.insert(0x88, "à".to_string());
        enc.insert(0xD5, "\u{2019}".to_string());
        enc.insert(0x41, "à".to_string()); // /Differences override
        let dec = TextDecoder {
            two_byte: false,
            to_unicode: None,
            widths: None,
            cid_to_unicode: None,
            code_to_cid: None,
            cid_to_gid: None,
            simple_encoding: Some(enc),
            vmetrics: None,
        };
        assert_eq!(dec.decode(&[0x88, 0xD5, 0x41, b'e']), "à\u{2019}àe");
    }

    #[test]
    fn simple_decoder_empty_sentinel_emits_nothing_not_winansi() {
        // A `/Differences` code whose glyph carries no Unicode (opaque subset
        // `gNN`) is recorded as the empty-string sentinel; the code must emit NO
        // text and must NOT fall back to a WinAnsi letter for that byte.
        let mut enc = std::collections::BTreeMap::new();
        enc.insert(0x18u8, String::new()); // explicitly "no text"
        enc.insert(0x41u8, "A".to_string());
        let dec = TextDecoder {
            two_byte: false,
            to_unicode: None,
            widths: None,
            cid_to_unicode: None,
            code_to_cid: None,
            cid_to_gid: None,
            simple_encoding: Some(enc),
            vmetrics: None,
        };
        // 0x18 → nothing (sentinel, not WinAnsi 'N'/control), 0x41 → 'A',
        // 0x42 (unmapped, printable) → WinAnsi 'B'.
        assert_eq!(dec.decode(&[0x18, 0x41, 0x42]), "AB");
    }

    #[test]
    fn simple_decoder_skips_unmapped_control_bytes() {
        // A control-range byte not covered by ToUnicode or the encoding map must
        // not leak its raw scalar (the dingbat tofu) — it is dropped.
        let dec = TextDecoder {
            two_byte: false,
            to_unicode: None,
            widths: None,
            cid_to_unicode: None,
            code_to_cid: None,
            cid_to_gid: None,
            simple_encoding: None,
            vmetrics: None,
        };
        // 0x02..0x08 (controls) drop; 'A'..'C' pass through.
        assert_eq!(dec.decode(&[0x02, b'A', 0x07, b'B', 0x1F, b'C']), "ABC");
    }

    #[test]
    fn composite_decoder_falls_back_to_cid_map_for_partial_tounicode() {
        // ToUnicode covers code 0x41 only; the embedded-cmap fallback covers 0x42.
        let to = ToUnicode::parse(b"beginbfchar <0041> <0041> endbfchar");
        let mut cid = std::collections::BTreeMap::new();
        cid.insert(0x42u16, "B".to_string());
        let dec = TextDecoder {
            two_byte: true,
            to_unicode: Some(to),
            widths: None,
            cid_to_unicode: Some(cid),
            code_to_cid: None,
            cid_to_gid: None,
            simple_encoding: None,
            vmetrics: None,
        };
        // 0x0041 → A (ToUnicode), 0x0042 → B (cid fallback), 0x0043 → no source
        // maps it ⇒ emit NOTHING (never a U+FFFD tofu). The glyph would draw, but
        // its Unicode lives nowhere in the file — exactly what reference
        // extractors (pdftotext/Adobe) drop. So the run is "AB", not "AB\u{FFFD}".
        assert_eq!(dec.decode(&[0x00, 0x41, 0x00, 0x42, 0x00, 0x43]), "AB");
    }

    #[test]
    fn composite_decoder_drops_fully_unmapped_codes() {
        // A composite font whose codes are covered by NO source at all (partial
        // `/ToUnicode`, no embedded cmap) must yield empty text — never tofu.
        let to = ToUnicode::parse(b"beginbfchar <0003> <0020> endbfchar");
        let dec = TextDecoder {
            two_byte: true,
            to_unicode: Some(to),
            widths: None,
            cid_to_unicode: None,
            code_to_cid: None,
            cid_to_gid: None,
            simple_encoding: None,
            vmetrics: None,
        };
        // 0x0003 → space (mapped); the Hebrew CIDs 0x02A2/0x02A4 map nowhere ⇒
        // dropped. Result is just the one space, with zero U+FFFD.
        let out = dec.decode(&[0x00, 0x03, 0x02, 0xA2, 0x02, 0xA4]);
        assert_eq!(out, " ");
        assert!(!out.contains('\u{FFFD}'));
    }

    // ── predefined / embedded `/Encoding` CMaps (code → CID) — issue #46 ────────

    #[test]
    fn cmap_parses_embedded_codespace_cidrange_and_cidchar() {
        // A minimal embedded CMap: 2-byte codespace, one cidrange, one cidchar.
        let cmap = Cmap::parse(
            b"1 begincodespacerange <0000> <ffff> endcodespacerange \
              1 begincidrange <0020> <0022> 10 endcidrange \
              1 begincidchar <0041> 99 endcidchar",
        );
        // Range <0020>..<0022> → CIDs 10,11,12 (first_cid + offset).
        assert_eq!(cmap.cid(0x0020), Some(10));
        assert_eq!(cmap.cid(0x0021), Some(11));
        assert_eq!(cmap.cid(0x0022), Some(12));
        // Single cidchar wins for its exact code.
        assert_eq!(cmap.cid(0x0041), Some(99));
        // A code mapped by nothing ⇒ None (caller leaves it unresolved).
        assert_eq!(cmap.cid(0x0030), None);
        // The codespace covers the whole 2-byte plane here.
        assert!(cmap.in_codespace(0x1234));
    }

    #[test]
    fn cmap_cidchar_overrides_overlapping_cidrange() {
        // When a code is covered by both a range and a char, the char wins.
        let cmap = Cmap::parse(
            b"begincidrange <0000> <00ff> 0 endcidrange \
              begincidchar <0041> 500 endcidchar",
        );
        assert_eq!(cmap.cid(0x0040), Some(0x40)); // range: identity here
        assert_eq!(cmap.cid(0x0041), Some(500)); // char override
    }

    #[test]
    fn cmap_cidrange_accepts_integer_cid_target() {
        // `cidrange` CID targets are commonly written as integers, not hex.
        let cmap = Cmap::parse(b"begincidrange <8140> <8142> 633 endcidrange");
        assert_eq!(cmap.cid(0x8140), Some(633));
        assert_eq!(cmap.cid(0x8142), Some(635));
    }

    #[test]
    fn cmap_predefined_identity_is_none_and_cjk_is_identity_codespace() {
        // Identity-H is the horizontal no-op (code == CID): signalled by None so
        // the decoder keeps its zero-overhead raw-code path.
        assert!(Cmap::predefined(b"Identity-H").is_none());
        // Identity-V is identity too, but the writing mode must reach the layout
        // layer: it yields `Some` of a vertical identity CMap (code == CID, wmode 1).
        let idv = Cmap::predefined(b"Identity-V").expect("Identity-V is vertical");
        assert_eq!(idv.wmode(), 1);
        assert_eq!(idv.cid(0x4E00), Some(0x4E00)); // still identity (code == CID)

        // A recognised CJK family maps its full 2-byte codespace identically.
        let gb = Cmap::predefined(b"UniGB-UCS2-H").expect("known predefined CMap");
        assert_eq!(gb.cid(0x4E00), Some(0x4E00));
        assert_eq!(gb.cid(0x0041), Some(0x0041));
        assert_eq!(gb.wmode(), 0); // `-H` name ⇒ horizontal
        assert!(Cmap::predefined(b"UniJIS-UCS2-H").is_some());
        assert!(Cmap::predefined(b"UniKS-UCS2-H").is_some());
        assert!(Cmap::predefined(b"GBK-EUC-H").is_some());
        // The `-V` spelling of a CJK family is the same identity mapping, vertical.
        let gbv = Cmap::predefined(b"UniGB-UCS2-V").expect("known predefined CMap");
        assert_eq!(gbv.wmode(), 1);
        assert_eq!(gbv.cid(0x4E00), Some(0x4E00));
        // An unknown name ⇒ None (caller falls back to the raw-code path).
        assert!(Cmap::predefined(b"NoSuch-CMap-H").is_none());
    }

    #[test]
    fn composite_decoder_resolves_code_through_cmap_and_cidtogidmap() {
        // End-to-end issue #46 path: a predefined-style `/Encoding` CMap maps the
        // 2-byte code → CID, a non-identity `/CIDToGIDMap` maps CID → glyph id, and
        // the glyph-id-keyed embedded-cmap fallback yields the Unicode.
        //
        //   code 0x0005 --CMap--> CID 3 --CIDToGIDMap--> GID 7 --cid_to_unicode--> "好"
        let code_to_cid = Cmap::parse(b"begincidchar <0005> 3 endcidchar");
        // CIDToGIDMap stream is 2 bytes per CID, indexed by CID: CID 3 → GID 7.
        let cid_to_gid = vec![0u16, 0, 0, 7];
        let mut gid_unicode = std::collections::BTreeMap::new();
        gid_unicode.insert(7u16, "好".to_string());
        // CID-keyed widths (`/W`): CID 3 → 1000 units.
        let mut widths = std::collections::BTreeMap::new();
        widths.insert(3u32, 1000.0);
        let dec = TextDecoder {
            two_byte: true,
            to_unicode: None,
            widths: Some(CodeWidths::new(widths, 500.0)),
            cid_to_unicode: Some(gid_unicode),
            code_to_cid: Some(code_to_cid),
            cid_to_gid: Some(cid_to_gid),
            simple_encoding: None,
            vmetrics: None,
        };
        // Text extraction walks code → CID → GID → Unicode.
        assert_eq!(dec.decode(&[0x00, 0x05]), "好");
        // Width is looked up by CID (3 → 1000), not the raw code (which would miss
        // and yield the 500 default): 1000 units × 12/1000 = 12.0 pt.
        assert_eq!(dec.string_advance(&[0x00, 0x05], 12.0), Some(12.0));
    }

    #[test]
    fn composite_decoder_identity_cmap_keeps_code_equals_cid_equals_gid() {
        // No `/Encoding` CMap and no `/CIDToGIDMap` (the Identity-H common case):
        // the 2-byte code is used directly as the glyph id, unchanged behaviour.
        let mut gid_unicode = std::collections::BTreeMap::new();
        gid_unicode.insert(0x0042u16, "B".to_string());
        let dec = TextDecoder {
            two_byte: true,
            to_unicode: None,
            widths: None,
            cid_to_unicode: Some(gid_unicode),
            code_to_cid: None,
            cid_to_gid: None,
            simple_encoding: None,
            vmetrics: None,
        };
        assert_eq!(dec.decode(&[0x00, 0x42]), "B");
    }

    // ── vertical writing mode metrics (WMode 1 / `/W2` / `/DW2`) — issue #49 ─────

    #[test]
    fn embedded_cmap_stream_reads_wmode() {
        // An embedded `/Encoding` CMap that declares `/WMode 1 def` is vertical.
        let cmap = Cmap::parse(
            b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
              /WMode 1 def \
              1 begincodespacerange <0000> <ffff> endcodespacerange \
              1 begincidrange <0000> <ffff> 0 endcidrange \
              endcmap end end",
        );
        assert_eq!(cmap.wmode(), 1);
        // A CMap with no `/WMode` (or `/WMode 0`) stays horizontal.
        let h = Cmap::parse(b"1 begincidchar <0001> 1 endcidchar");
        assert_eq!(h.wmode(), 0);
    }

    #[test]
    fn decoder_wmode_reads_through_encoding_cmap() {
        // A composite font whose `/Encoding` is Identity-V reports wmode 1.
        let dec = TextDecoder {
            two_byte: true,
            to_unicode: None,
            widths: None,
            cid_to_unicode: None,
            code_to_cid: Cmap::predefined(b"Identity-V"),
            cid_to_gid: None,
            simple_encoding: None,
            vmetrics: None,
        };
        assert_eq!(dec.wmode(), 1);
        // Identity-H (None code_to_cid) and simple fonts are horizontal.
        assert_eq!(TextDecoder::winansi().wmode(), 0);
    }

    #[test]
    fn vertical_metric_uses_dw2_default_with_w0_half_centring() {
        // No `/W2`/`/DW2` ⇒ spec defaults: w1y = −1000 ‰, vy = 880 ‰, vx = w0/2.
        // Width table gives every CID w0 = 1000 ‰; size 10 pt.
        let mut wmap = std::collections::BTreeMap::new();
        wmap.insert(1u32, 1000.0);
        let dec = TextDecoder {
            two_byte: true,
            to_unicode: None,
            widths: Some(CodeWidths::new(wmap, 1000.0)),
            cid_to_unicode: None,
            code_to_cid: Cmap::predefined(b"Identity-V"),
            cid_to_gid: None,
            simple_encoding: None,
            vmetrics: None, // ⇒ DW2 defaults
        };
        let (w1y, vx, vy) = dec.vertical_metric(0x0001, 10.0);
        assert!((w1y - (-10.0)).abs() < 1e-9, "w1y = −10 pt: {w1y}");
        assert!((vx - 5.0).abs() < 1e-9, "vx = w0/2 = 5 pt: {vx}");
        assert!((vy - 8.8).abs() < 1e-9, "vy = 8.8 pt: {vy}");
        // The whole-string vertical advance sums the (negative) displacements.
        let adv = dec.string_vertical_advance(&[0x00, 0x01, 0x00, 0x01], 10.0);
        assert!(
            (adv - (-20.0)).abs() < 1e-9,
            "two glyphs advance −20 pt: {adv}"
        );
    }

    #[test]
    fn vertical_metric_w2_override_beats_dw2() {
        // `/W2`: CID 1 → w1y = −1300 ‰, vx = 250 ‰, vy = 750 ‰. CID 2 falls back
        // to the (custom) `/DW2` `[vy=900, w1y=−1100]`.
        let mut w2 = std::collections::BTreeMap::new();
        w2.insert(
            1u32,
            VMetric {
                w1y: -1300.0,
                vx: 250.0,
                vy: 750.0,
            },
        );
        let vmetrics = VerticalMetrics::new(Some((900.0, -1100.0)), w2);
        let mut wmap = std::collections::BTreeMap::new();
        wmap.insert(1u32, 1000.0);
        wmap.insert(2u32, 800.0);
        let dec = TextDecoder {
            two_byte: true,
            to_unicode: None,
            widths: Some(CodeWidths::new(wmap, 1000.0)),
            cid_to_unicode: None,
            code_to_cid: Cmap::predefined(b"Identity-V"),
            cid_to_gid: None,
            simple_encoding: None,
            vmetrics: Some(vmetrics),
        };
        // CID 1: the `/W2` override.
        let (w1y1, vx1, vy1) = dec.vertical_metric(0x0001, 10.0);
        assert!(
            (w1y1 - (-13.0)).abs() < 1e-9,
            "override w1y = −13 pt: {w1y1}"
        );
        assert!((vx1 - 2.5).abs() < 1e-9, "override vx = 2.5 pt: {vx1}");
        assert!((vy1 - 7.5).abs() < 1e-9, "override vy = 7.5 pt: {vy1}");
        // CID 2: no `/W2` entry ⇒ custom `/DW2` w1y = −1100 ‰ = −11 pt, vy = 9.0 pt,
        // vx = w0(800)/2 = 400 ‰ = 4 pt.
        let (w1y2, vx2, vy2) = dec.vertical_metric(0x0002, 10.0);
        assert!((w1y2 - (-11.0)).abs() < 1e-9, "DW2 w1y = −11 pt: {w1y2}");
        assert!((vx2 - 4.0).abs() < 1e-9, "DW2 vx = w0/2 = 4 pt: {vx2}");
        assert!((vy2 - 9.0).abs() < 1e-9, "DW2 vy = 9 pt: {vy2}");
    }

    #[test]
    fn horizontal_decoder_has_no_vertical_advance() {
        // A simple (non-composite) font never advances vertically.
        let adv = TextDecoder::winansi().string_vertical_advance(b"AB", 12.0);
        assert_eq!(adv, 0.0);
    }
}
