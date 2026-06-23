//! Raw **Type 1** font embedding (Adobe TN #5040 / #5015) — zero dependencies.
//!
//! Historic Type 1 fonts ship as `eexec`-encrypted PostScript: a PDF `FontFile`
//! (Length1/Length2/Length3), a `.pfb` (binary, 0x80-segmented) or a `.pfa`
//! (ASCII `%!`, hex `eexec`). Their charstrings are a *different* (cubic) stack
//! machine than Type 2, but the curves carry over losslessly.
//!
//! Strategy (imposed): decrypt the font, interpret each Type 1 charstring while
//! inlining its subrs/OtherSubrs, re-emit it as a **Type 2** charstring, and pack
//! the lot into a **bare CFF** that [`super::cff_to_otf::wrap`] already knows how
//! to turn into an embeddable OpenType-CFF. The single subtlety is that `wrap`
//! recovers the char→glyph map from the CFF *charset*: a glyph named `A`,
//! `space`, `eacute`… must carry its **predefined Standard String SID** so that
//! `sid_to_unicode` resolves it (`A` → U+0041). Non-standard names go to the
//! font's String INDEX (SID ≥ 391) and reach `wrap` via `uniXXXX`/single-char
//! AGL conventions.

/// A parsed raw Type 1 font: decrypted charstrings plus the header metadata
/// needed to build an equivalent CFF.
#[derive(Debug, Clone)]
pub struct Type1Font {
    /// PostScript `/FontName` (defaults to `Type1Font` when absent).
    pub font_name: String,
    /// Design units per em derived from `/FontMatrix` (`1/a`, default 1000).
    pub units_per_em: f64,
    /// `/FontBBox` `[x0 y0 x1 y1]` in font units (zeros when absent).
    pub font_bbox: [f64; 4],
    /// `/ItalicAngle` in degrees (informational; 0 when absent).
    pub italic_angle: f64,
    /// `(glyph-name, decrypted Type 1 charstring)` in file order. `.notdef`, if
    /// present, is moved to the front by [`to_cff`].
    pub glyphs: Vec<(String, Vec<u8>)>,
    /// Decrypted `/Subrs`, indexed by subr number (used for `callsubr`).
    pub subrs: Vec<Vec<u8>>,
    /// The font's `/Encoding`: `code → glyph name`. `StandardEncoding` is
    /// expanded; a custom `dup … put` array is read verbatim. Retained as font
    /// metadata (the CFF charset, not this table, drives the char→glyph map).
    pub encoding: Vec<Option<String>>,
}

// ── container normalisation ─────────────────────────────────────────────────

/// Reassemble a `.pfb` (IBM PC segmented) container into the raw Type 1 stream
/// (ASCII clear text + binary `eexec` + 512-zero trailer). Returns `None` on a
/// malformed segment table.
fn unwrap_pfb(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        // Each segment: 0x80, type, then (for 1/2) a u32 LE length + payload.
        if bytes.get(i)? != &0x80 {
            return None;
        }
        let seg_type = *bytes.get(i + 1)?;
        match seg_type {
            1 | 2 => {
                let len = u32::from_le_bytes([
                    *bytes.get(i + 2)?,
                    *bytes.get(i + 3)?,
                    *bytes.get(i + 4)?,
                    *bytes.get(i + 5)?,
                ]) as usize;
                let start = i + 6;
                let end = start.checked_add(len)?;
                out.extend_from_slice(bytes.get(start..end)?);
                i = end;
            }
            3 => break, // EOF marker.
            _ => return None,
        }
    }
    Some(out)
}

/// Locate the byte just past the `eexec` keyword (and the single whitespace that
/// follows it). The clear-text header always ends with this token.
fn eexec_split(bytes: &[u8]) -> Option<usize> {
    let pos = find_subsequence(bytes, b"eexec")?;
    let mut j = pos + 5;
    // Skip the mandatory whitespace separator(s) after the keyword.
    while matches!(bytes.get(j), Some(b' ' | b'\r' | b'\n' | b'\t')) {
        j += 1;
    }
    Some(j)
}

/// Decrypt an `eexec`/charstring block (Adobe TN #5040 §7.3). `r` seeds the
/// LFSR (55665 for `eexec`, 4330 for charstrings/subrs); `skip` drops the first
/// `lenIV` random bytes. Pure integer arithmetic, panic-free.
fn decrypt(cipher: &[u8], mut r: u16, skip: usize) -> Vec<u8> {
    const C1: u16 = 52845;
    const C2: u16 = 22719;
    let mut plain = Vec::with_capacity(cipher.len().saturating_sub(skip));
    for (idx, &c) in cipher.iter().enumerate() {
        let p = c ^ (r >> 8) as u8;
        r = (c as u16).wrapping_add(r).wrapping_mul(C1).wrapping_add(C2);
        if idx >= skip {
            plain.push(p);
        }
    }
    plain
}

/// The `eexec` section may be stored as raw binary or as ASCII hex (the `.pfa`
/// form). Detect hex (only `[0-9A-Fa-f]` + whitespace, and not trivially short)
/// and decode it; otherwise return the bytes unchanged.
fn normalize_eexec_binary(section: &[u8]) -> Vec<u8> {
    let mut hex_digits = 0usize;
    let mut looks_hex = true;
    for &b in section.iter().take(4) {
        // Sample the first few non-space bytes: a binary eexec almost never
        // starts with four hex characters in a row.
        if b.is_ascii_whitespace() {
            continue;
        }
        if !b.is_ascii_hexdigit() {
            looks_hex = false;
            break;
        }
    }
    if looks_hex {
        for &b in section {
            if b.is_ascii_hexdigit() {
                hex_digits += 1;
            } else if !b.is_ascii_whitespace() {
                // A non-hex, non-space byte means this is really binary.
                looks_hex = false;
                break;
            }
        }
    }
    if !looks_hex || hex_digits < 8 {
        return section.to_vec();
    }
    let mut out = Vec::with_capacity(hex_digits / 2);
    let mut hi: Option<u8> = None;
    for &b in section {
        let nib = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => continue,
        };
        match hi.take() {
            None => hi = Some(nib),
            Some(h) => out.push((h << 4) | nib),
        }
    }
    out
}

// ── byte-level scanning helpers ─────────────────────────────────────────────

/// First index of `needle` in `haystack` (naïve; inputs are small font files).
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// First index of `needle` at or after `from`.
fn find_from(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let slice = haystack.get(from..)?;
    find_subsequence(slice, needle).map(|p| p + from)
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\r' | b'\n' | b'\t' | 0x0c)
}

/// Parse a non-negative ASCII integer ending at `end` (exclusive), scanning
/// left from the preceding token. Returns `(value, start_of_number)`.
fn parse_int_before(data: &[u8], end: usize) -> Option<(usize, usize)> {
    let mut j = end;
    while j > 0 && is_ws(data[j - 1]) {
        j -= 1;
    }
    let num_end = j;
    while j > 0 && data[j - 1].is_ascii_digit() {
        j -= 1;
    }
    if j == num_end {
        return None;
    }
    let n = std::str::from_utf8(data.get(j..num_end)?)
        .ok()?
        .parse()
        .ok()?;
    Some((n, j))
}

/// Read a `<len> RD <space> <len bytes>` binary entry. `rd_pos` points at the
/// `R` of the `RD`/`-|` token. Returns the binary payload and the index just
/// past it. The length integer precedes the token; exactly `len` raw bytes are
/// taken after the single separator space (never re-scanned by whitespace, so
/// binary that *looks* like tokens is handled correctly).
fn read_binary_entry(data: &[u8], rd_pos: usize, token_len: usize) -> Option<(Vec<u8>, usize)> {
    let (len, _num_start) = parse_int_before(data, rd_pos)?;
    let data_start = rd_pos + token_len + 1; // token + one separator space
    let data_end = data_start.checked_add(len)?;
    let bytes = data.get(data_start..data_end)?.to_vec();
    Some((bytes, data_end))
}

// ── clear-text header parsing ───────────────────────────────────────────────

/// Extract a `/Key (...) def`-style token value as text, scanning from `/Key`.
fn header_token_after(header: &[u8], key: &[u8]) -> Option<String> {
    let pos = find_subsequence(header, key)?;
    let mut j = pos + key.len();
    while matches!(header.get(j), Some(b) if is_ws(*b)) {
        j += 1;
    }
    if header.get(j) == Some(&b'/') {
        j += 1; // FontName style: "/FontName /Foo def"
    }
    let start = j;
    while let Some(&b) = header.get(j) {
        if is_ws(b) || b == b'(' || b == b'{' || b == b'[' {
            break;
        }
        j += 1;
    }
    let raw = header.get(start..j)?;
    Some(String::from_utf8_lossy(raw).into_owned())
}

/// Parse a bracketed/braced numeric list following `key` (e.g. `/FontMatrix`,
/// `/FontBBox`). Accepts both `[...]` and `{...}` delimiters.
fn header_number_list(header: &[u8], key: &[u8]) -> Vec<f64> {
    let Some(pos) = find_subsequence(header, key) else {
        return Vec::new();
    };
    let mut j = pos + key.len();
    while matches!(header.get(j), Some(b) if is_ws(*b)) {
        j += 1;
    }
    if !matches!(header.get(j), Some(b'[' | b'{')) {
        return Vec::new();
    }
    j += 1;
    let mut out = Vec::new();
    let mut tok = String::new();
    while let Some(&b) = header.get(j) {
        if b == b']' || b == b'}' {
            break;
        }
        if is_ws(b) {
            if !tok.is_empty() {
                if let Ok(v) = tok.parse::<f64>() {
                    out.push(v);
                }
                tok.clear();
            }
        } else {
            tok.push(b as char);
        }
        j += 1;
    }
    if !tok.is_empty() {
        if let Ok(v) = tok.parse::<f64>() {
            out.push(v);
        }
    }
    out
}

/// Parse the `/Encoding` array: `StandardEncoding def`, or a sequence of
/// `dup <code> /<name> put`. Returns the 256-slot code→name table.
fn parse_encoding(header: &[u8]) -> Vec<Option<String>> {
    let mut enc: Vec<Option<String>> = vec![None; 256];
    let Some(pos) = find_subsequence(header, b"/Encoding") else {
        return enc;
    };
    let after = pos + b"/Encoding".len();
    // StandardEncoding shorthand.
    if let Some(rest) = header.get(after..after + 40) {
        if find_subsequence(rest, b"StandardEncoding").is_some() {
            for (code, slot) in enc.iter_mut().enumerate() {
                *slot = standard_encoding_name(code as u8).map(str::to_string);
            }
            return enc;
        }
    }
    // Explicit `dup <code> /<name> put` entries until `readonly`/`def`.
    let mut j = after;
    while let Some(dup) = find_from(header, j, b"dup ") {
        // Stop if a `def`/`readonly` for the Encoding closes before the next dup.
        if let Some(end) = find_from(header, after, b" def") {
            if dup > end {
                break;
            }
        }
        let mut k = dup + 4;
        while matches!(header.get(k), Some(b) if is_ws(*b)) {
            k += 1;
        }
        let cs = k;
        while matches!(header.get(k), Some(b) if b.is_ascii_digit()) {
            k += 1;
        }
        let code: usize = std::str::from_utf8(header.get(cs..k).unwrap_or(b""))
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(usize::MAX);
        while matches!(header.get(k), Some(b) if is_ws(*b)) {
            k += 1;
        }
        if header.get(k) == Some(&b'/') {
            k += 1;
            let ns = k;
            while let Some(&b) = header.get(k) {
                if is_ws(b) {
                    break;
                }
                k += 1;
            }
            if code < 256 {
                enc[code] =
                    Some(String::from_utf8_lossy(header.get(ns..k).unwrap_or(b"")).into_owned());
            }
        }
        j = k;
    }
    enc
}

// ── Private dict parsing (decrypted eexec) ──────────────────────────────────

/// Extract `/lenIV` (default 4) from the decrypted Private dict.
fn parse_len_iv(priv_data: &[u8]) -> usize {
    let Some(pos) = find_subsequence(priv_data, b"/lenIV") else {
        return 4;
    };
    let mut j = pos + b"/lenIV".len();
    while matches!(priv_data.get(j), Some(b) if is_ws(*b)) {
        j += 1;
    }
    let s = j;
    while matches!(priv_data.get(j), Some(b) if b.is_ascii_digit()) {
        j += 1;
    }
    std::str::from_utf8(priv_data.get(s..j).unwrap_or(b""))
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(4)
}

/// Match an `RD`/`-|` charstring-data token at `pos`, returning its length in
/// bytes. Both must be bounded by whitespace to avoid matching inside a name.
fn rd_token_len(data: &[u8], pos: usize) -> Option<usize> {
    let prev_ws = pos == 0 || is_ws(data[pos - 1]);
    if !prev_ws {
        return None;
    }
    if data.get(pos..pos + 2) == Some(b"RD") && matches!(data.get(pos + 2), Some(b' ')) {
        return Some(2);
    }
    if data.get(pos..pos + 2) == Some(b"-|") && matches!(data.get(pos + 2), Some(b' ')) {
        return Some(2);
    }
    None
}

/// Parse `/Subrs <n> array … dup <i> <len> RD <bytes> NP` into a dense vector
/// (index = subr number). Each entry is decrypted (r=4330) with `lenIV` skipped.
fn parse_subrs(priv_data: &[u8], len_iv: usize) -> Vec<Vec<u8>> {
    let Some(start) = find_subsequence(priv_data, b"/Subrs") else {
        return Vec::new();
    };
    let mut subrs: Vec<Vec<u8>> = Vec::new();
    // Search for `dup <i> <len> RD<sp>` entries after `/Subrs`.
    let mut j = start;
    loop {
        let Some(dup) = find_from(priv_data, j, b"dup ") else {
            break;
        };
        // A `/CharStrings` start means the Subrs section is over.
        if let Some(cs) = find_subsequence(priv_data, b"/CharStrings") {
            if dup > cs {
                break;
            }
        }
        // Parse `dup <index> <len> RD`.
        let mut k = dup + 4;
        while matches!(priv_data.get(k), Some(b) if is_ws(*b)) {
            k += 1;
        }
        let is = k;
        while matches!(priv_data.get(k), Some(b) if b.is_ascii_digit()) {
            k += 1;
        }
        let index: usize = match std::str::from_utf8(priv_data.get(is..k).unwrap_or(b""))
            .ok()
            .and_then(|s| s.parse().ok())
        {
            Some(v) => v,
            None => {
                j = k.max(dup + 4);
                continue;
            }
        };
        // Find the RD/-| token that introduces the binary for this entry.
        let Some(rd) = find_rd_after(priv_data, k) else {
            break;
        };
        let token_len = match rd_token_len(priv_data, rd) {
            Some(l) => l,
            None => {
                j = rd + 1;
                continue;
            }
        };
        let Some((raw, end)) = read_binary_entry(priv_data, rd, token_len) else {
            break;
        };
        let charstring = decrypt(&raw, 4330, len_iv);
        if index >= subrs.len() {
            subrs.resize(index + 1, Vec::new());
        }
        subrs[index] = charstring;
        j = end;
    }
    subrs
}

/// Find the next `RD ` or `-| ` token at/after `from`.
fn find_rd_after(data: &[u8], from: usize) -> Option<usize> {
    let rd = find_from(data, from, b"RD ");
    let alt = find_from(data, from, b"-| ");
    match (rd, alt) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Find the next charstring-data token (`ND`/`|-`/`def` follow). We reuse the
/// RD/-| finder since both Subrs and CharStrings introduce binary identically.
fn find_nd_after(data: &[u8], from: usize) -> Option<usize> {
    find_rd_after(data, from)
}

/// Parse `/CharStrings <n> dict dup begin /<name> <len> RD <bytes> ND …`.
/// Returns `(name, decrypted charstring)` pairs in file order.
fn parse_charstrings(priv_data: &[u8], len_iv: usize) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let Some(start) = find_subsequence(priv_data, b"/CharStrings") else {
        return out;
    };
    // Skip to the `begin` that opens the dictionary body.
    let body = find_from(priv_data, start, b"begin")
        .map(|b| b + 5)
        .unwrap_or(start);
    let mut j = body;
    while let Some(slash) = find_from(priv_data, j, b"/") {
        // Read the glyph name token.
        let ns = slash + 1;
        let mut k = ns;
        while let Some(&b) = priv_data.get(k) {
            if is_ws(b) {
                break;
            }
            k += 1;
        }
        let name = String::from_utf8_lossy(priv_data.get(ns..k).unwrap_or(b"")).into_owned();
        // The RD/-| token for this glyph's binary.
        let Some(rd) = find_nd_after(priv_data, k) else {
            break;
        };
        let token_len = match rd_token_len(priv_data, rd) {
            Some(l) => l,
            None => {
                // Not a charstring entry (e.g. `/Private` keyword) — keep going.
                j = k;
                if j <= slash {
                    j = slash + 1;
                }
                // Avoid an infinite loop when the next slash is the same.
                if find_from(priv_data, j, b"/")
                    .map(|p| p <= slash)
                    .unwrap_or(false)
                {
                    j = slash + 1;
                }
                continue;
            }
        };
        let Some((raw, endp)) = read_binary_entry(priv_data, rd, token_len) else {
            break;
        };
        let charstring = decrypt(&raw, 4330, len_iv);
        if !name.is_empty() {
            out.push((name, charstring));
        }
        j = endp;
        // Stop at the dictionary `end`.
        if let Some(end_kw) = find_from(priv_data, body, b" end") {
            if j > end_kw {
                break;
            }
        }
    }
    out
}

// ── public entry: parse ─────────────────────────────────────────────────────

/// Parse a raw Type 1 font (PFB / PFA / PDF `FontFile`). Returns `None` if the
/// container is malformed or no `eexec` section is present.
pub fn parse_type1(bytes: &[u8]) -> Option<Type1Font> {
    // 1. Normalise the container to a raw Type 1 stream.
    let raw = if bytes.first() == Some(&0x80) {
        unwrap_pfb(bytes)?
    } else {
        bytes.to_vec()
    };

    // 2. Split clear header / encrypted section at `eexec`.
    let split = eexec_split(&raw)?;
    let header = raw.get(..split)?;
    let enc_section = raw.get(split..)?;

    // 3. The encrypted section may be hex (PFA) or binary; normalise then
    //    decrypt (eexec seed 55665, skip the 4 random lead bytes).
    let bin = normalize_eexec_binary(enc_section);
    let private = decrypt(&bin, 55665, 4);

    // 4. Header metadata.
    let font_name = header_token_after(header, b"/FontName").unwrap_or_else(|| "Type1Font".into());
    let matrix = header_number_list(header, b"/FontMatrix");
    let units_per_em = match matrix.first() {
        Some(&a) if a.abs() > 1e-12 => (1.0 / a).round(),
        _ => 1000.0,
    };
    let bbox = header_number_list(header, b"/FontBBox");
    let font_bbox = [
        bbox.first().copied().unwrap_or(0.0),
        bbox.get(1).copied().unwrap_or(0.0),
        bbox.get(2).copied().unwrap_or(0.0),
        bbox.get(3).copied().unwrap_or(0.0),
    ];
    let italic_angle = header_token_after(header, b"/ItalicAngle")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let encoding = parse_encoding(header);

    // 5. Private dict: lenIV, Subrs, CharStrings.
    let len_iv = parse_len_iv(&private);
    let subrs = parse_subrs(&private, len_iv);
    let glyphs = parse_charstrings(&private, len_iv);
    if glyphs.is_empty() {
        return None;
    }

    Some(Type1Font {
        font_name,
        units_per_em,
        font_bbox,
        italic_angle,
        glyphs,
        subrs,
        encoding,
    })
}

// ── Type 1 → Type 2 charstring conversion ───────────────────────────────────

/// A `seac` accent-composition request: `(adx, ady, base_char, accent_char)`.
/// `adx/ady` are the accent's displacement (already adjusted for `asb`/`sbx`);
/// `base_char`/`accent_char` are StandardEncoding codes.
type SeacRequest = (f64, f64, u8, u8);

/// Builder for one glyph's Type 2 charstring. Tracks the pending width (emitted
/// on the first stem/move/endchar) and the horizontal side-bearing offset (the
/// Type 1 start point is `(sbx, 0)`, which Type 2 lacks).
struct T2Builder {
    out: Vec<u8>,
    width: Option<f64>, // wx - nominalWidthX, prefixed on the first operator.
    nominal_width: f64,
    width_emitted: bool,
    sbx: f64,
    started: bool, // first moveto seen?
}

impl T2Builder {
    fn new(nominal_width: f64) -> T2Builder {
        T2Builder {
            out: Vec::new(),
            width: None,
            nominal_width,
            width_emitted: false,
            sbx: 0.0,
            started: false,
        }
    }

    /// Emit a Type 2 integer operand (op 28 for anything outside the compact
    /// single-byte range, which `cff::parse_dict`/`Interp` both decode).
    fn push_num(&mut self, v: f64) {
        let n = v.round() as i32;
        if (-107..=107).contains(&n) {
            self.out.push((n + 139) as u8);
        } else if (108..=1131).contains(&n) {
            let v = n - 108;
            self.out.push((v / 256 + 247) as u8);
            self.out.push((v % 256) as u8);
        } else if (-1131..=-108).contains(&n) {
            let v = -n - 108;
            self.out.push((v / 256 + 251) as u8);
            self.out.push((v % 256) as u8);
        } else {
            // 16-bit fallback; coordinates are bounded well within i16 for fonts.
            let c = n.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            self.out.push(28);
            self.out.extend_from_slice(&c.to_be_bytes());
        }
    }

    /// Prefix the pending width on the first emitted stack-clearing operator.
    fn emit_width_if_pending(&mut self) {
        if !self.width_emitted {
            if let Some(w) = self.width {
                self.push_num(w);
            }
            self.width_emitted = true;
        }
    }

    fn op(&mut self, code: u8) {
        self.out.push(code);
    }

    /// Emit `rmoveto`. The first move also carries the width and the `sbx`
    /// offset (Type 1's implicit start at `(sbx, 0)`).
    fn rmoveto(&mut self, dx: f64, dy: f64) {
        let dx = if !self.started { dx + self.sbx } else { dx };
        self.started = true;
        self.emit_width_if_pending();
        self.push_num(dx);
        self.push_num(dy);
        self.op(21);
    }

    fn rlineto(&mut self, dx: f64, dy: f64) {
        self.push_num(dx);
        self.push_num(dy);
        self.op(5);
    }

    fn rrcurveto(&mut self, c: [f64; 6]) {
        for v in c {
            self.push_num(v);
        }
        self.op(8);
    }

    fn endchar(&mut self) {
        self.emit_width_if_pending();
        self.op(14);
    }
}

/// Interpreter state for converting a Type 1 charstring. The PostScript-ish
/// operand stack is `f64`; current point is tracked so Type 1's absolute-ish
/// operators translate into Type 2 deltas.
struct T1Interp<'a> {
    stack: Vec<f64>,
    ps_stack: Vec<f64>, // OtherSubrs argument stack.
    x: f64,
    y: f64,
    subrs: &'a [Vec<u8>],
    builder: T2Builder,
    flex_pts: Vec<(f64, f64)>,
    in_flex: bool,
    /// Current point captured the instant flex collection begins, so the first
    /// emitted Bézier starts from the right place (the flex rmovetos advance the
    /// current point, losing it otherwise).
    flex_start: (f64, f64),
    /// `seac` request `(adx, ady, bchar, achar)` captured for the caller.
    seac: Option<SeacRequest>,
    /// One-shot translation applied to the next drawing `moveto` (used to place
    /// the accent component of a `seac` composite). Consumed on first use.
    accent_shift: Option<(f64, f64)>,
    done: bool,
}

impl<'a> T1Interp<'a> {
    fn new(subrs: &'a [Vec<u8>], nominal_width: f64) -> T1Interp<'a> {
        T1Interp {
            stack: Vec::new(),
            ps_stack: Vec::new(),
            x: 0.0,
            y: 0.0,
            subrs,
            builder: T2Builder::new(nominal_width),
            flex_pts: Vec::new(),
            in_flex: false,
            flex_start: (0.0, 0.0),
            seac: None,
            accent_shift: None,
            done: false,
        }
    }

    fn moveto(&mut self, mut dx: f64, mut dy: f64) {
        if self.in_flex {
            // Collect the 7 flex reference/control points without drawing.
            self.x += dx;
            self.y += dy;
            self.flex_pts.push((self.x, self.y));
            return;
        }
        // Place the accent component of a seac composite: shift the first move.
        if let Some((sx, sy)) = self.accent_shift.take() {
            dx += sx;
            dy += sy;
        }
        self.x += dx;
        self.y += dy;
        self.builder.rmoveto(dx, dy);
    }

    fn lineto(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        self.builder.rlineto(dx, dy);
    }

    fn curveto(&mut self, d: [f64; 6]) {
        self.x += d[0] + d[2] + d[4];
        self.y += d[1] + d[3] + d[5];
        self.builder.rrcurveto(d);
    }

    /// Execute a Type 1 charstring (inlining subrs). `depth` guards recursion.
    fn exec(&mut self, code: &[u8], depth: usize) {
        if depth > 30 || self.done {
            return;
        }
        let mut i = 0;
        while i < code.len() {
            if self.done {
                return;
            }
            let b = code[i];
            i += 1;
            match b {
                13 => {
                    // hsbw: sbx wx → side bearing + nominal width.
                    let sbx = self.stack.first().copied().unwrap_or(0.0);
                    let wx = self.stack.get(1).copied().unwrap_or(0.0);
                    self.builder.sbx = sbx;
                    self.x = sbx;
                    self.builder.width = Some(wx - self.builder.nominal_width);
                    self.stack.clear();
                }
                9 => {
                    // closepath: implicit in Type 2.
                    self.stack.clear();
                }
                21 => {
                    let dy = self.stack.pop().unwrap_or(0.0);
                    let dx = self.stack.pop().unwrap_or(0.0);
                    self.moveto(dx, dy);
                    self.stack.clear();
                }
                22 => {
                    let dx = self.stack.pop().unwrap_or(0.0);
                    self.moveto(dx, 0.0);
                    self.stack.clear();
                }
                4 => {
                    let dy = self.stack.pop().unwrap_or(0.0);
                    self.moveto(0.0, dy);
                    self.stack.clear();
                }
                5 => {
                    let dy = self.stack.get(1).copied().unwrap_or(0.0);
                    let dx = self.stack.first().copied().unwrap_or(0.0);
                    self.lineto(dx, dy);
                    self.stack.clear();
                }
                6 => {
                    let dx = self.stack.first().copied().unwrap_or(0.0);
                    self.lineto(dx, 0.0);
                    self.stack.clear();
                }
                7 => {
                    let dy = self.stack.first().copied().unwrap_or(0.0);
                    self.lineto(0.0, dy);
                    self.stack.clear();
                }
                8 => {
                    // rrcurveto
                    let s = self.take6();
                    self.curveto(s);
                    self.stack.clear();
                }
                30 => {
                    // vhcurveto: dy1 dx2 dy2 dx3 → (0 dy1 dx2 dy2 dx3 0)
                    let a = self.stack.first().copied().unwrap_or(0.0);
                    let b2 = self.stack.get(1).copied().unwrap_or(0.0);
                    let c = self.stack.get(2).copied().unwrap_or(0.0);
                    let d = self.stack.get(3).copied().unwrap_or(0.0);
                    self.curveto([0.0, a, b2, c, d, 0.0]);
                    self.stack.clear();
                }
                31 => {
                    // hvcurveto: dx1 dx2 dy2 dy3 → (dx1 0 dx2 dy2 0 dy3)
                    let a = self.stack.first().copied().unwrap_or(0.0);
                    let b2 = self.stack.get(1).copied().unwrap_or(0.0);
                    let c = self.stack.get(2).copied().unwrap_or(0.0);
                    let d = self.stack.get(3).copied().unwrap_or(0.0);
                    self.curveto([a, 0.0, b2, c, 0.0, d]);
                    self.stack.clear();
                }
                1 | 3 => {
                    // hstem / vstem: dropped (no hints emitted → no hintmask).
                    self.stack.clear();
                }
                10 => {
                    // callsubr: index (no bias in Type 1).
                    if let Some(idx) = self.stack.pop() {
                        let n = idx.round() as i64;
                        if n >= 0 {
                            if let Some(sub) = self.subrs.get(n as usize).cloned() {
                                self.exec(&sub, depth + 1);
                            }
                        }
                    }
                }
                11 => return, // return from subr.
                14 => {
                    self.builder.endchar();
                    self.done = true;
                    return;
                }
                12 => {
                    // Two-byte (escape) operators.
                    let b1 = code.get(i).copied().unwrap_or(0);
                    i += 1;
                    self.exec_escape(b1);
                }
                28 => {
                    let hi = code.get(i).copied().unwrap_or(0);
                    let lo = code.get(i + 1).copied().unwrap_or(0);
                    i += 2;
                    self.stack.push(i16::from_be_bytes([hi, lo]) as f64);
                }
                32..=246 => self.stack.push(b as f64 - 139.0),
                247..=250 => {
                    let b1 = code.get(i).copied().unwrap_or(0) as f64;
                    i += 1;
                    self.stack.push((b as f64 - 247.0) * 256.0 + b1 + 108.0);
                }
                251..=254 => {
                    let b1 = code.get(i).copied().unwrap_or(0) as f64;
                    i += 1;
                    self.stack.push(-(b as f64 - 251.0) * 256.0 - b1 - 108.0);
                }
                255 => {
                    // 32-bit integer (Type 1 uses plain i32, not 16.16 Fixed).
                    let v = i32::from_be_bytes([
                        code.get(i).copied().unwrap_or(0),
                        code.get(i + 1).copied().unwrap_or(0),
                        code.get(i + 2).copied().unwrap_or(0),
                        code.get(i + 3).copied().unwrap_or(0),
                    ]);
                    i += 4;
                    self.stack.push(v as f64);
                }
                _ => self.stack.clear(),
            }
        }
    }

    fn exec_escape(&mut self, op: u8) {
        match op {
            0 => self.stack.clear(),     // dotsection
            1 | 2 => self.stack.clear(), // vstem3 / hstem3 (dropped)
            6 => {
                // seac: asb adx ady bchar achar → accent composition request.
                let asb = self.stack.first().copied().unwrap_or(0.0);
                let adx = self.stack.get(1).copied().unwrap_or(0.0);
                let ady = self.stack.get(2).copied().unwrap_or(0.0);
                let bchar = self.stack.get(3).copied().unwrap_or(0.0) as u8;
                let achar = self.stack.get(4).copied().unwrap_or(0.0) as u8;
                // Caller flattens base+accent; record sbx-relative accent shift.
                self.seac = Some((adx - asb + self.builder.sbx, ady, bchar, achar));
                self.builder.endchar();
                self.done = true;
                self.stack.clear();
            }
            7 => {
                // sbw: sbx sby wx wy
                let sbx = self.stack.first().copied().unwrap_or(0.0);
                let wx = self.stack.get(2).copied().unwrap_or(0.0);
                self.builder.sbx = sbx;
                self.x = sbx;
                self.y = self.stack.get(1).copied().unwrap_or(0.0);
                self.builder.width = Some(wx - self.builder.nominal_width);
                self.stack.clear();
            }
            12 => {
                // div: a b → a/b
                let b = self.stack.pop().unwrap_or(1.0);
                let a = self.stack.pop().unwrap_or(0.0);
                self.stack.push(if b != 0.0 { a / b } else { 0.0 });
            }
            16 => self.call_othersubr(),
            17 => {
                // pop: move one value from the PS stack back to the operand stack.
                if let Some(v) = self.ps_stack.pop() {
                    self.stack.push(v);
                }
            }
            33 => {
                // setcurrentpoint: x y — re-anchors the current point.
                self.y = self.stack.get(1).copied().unwrap_or(self.y);
                self.x = self.stack.first().copied().unwrap_or(self.x);
                self.stack.clear();
            }
            _ => self.stack.clear(),
        }
    }

    /// OtherSubrs protocol (`<args…> n othersubr# callothersubr`). We implement
    /// flex (0/1/2) and hint replacement (3); unknown subrs degrade to passing
    /// their arguments straight through to the PS stack for the following `pop`s.
    fn call_othersubr(&mut self) {
        let subr = self.stack.pop().unwrap_or(0.0).round() as i64;
        let n = self.stack.pop().unwrap_or(0.0).round().max(0.0) as usize;
        // Pop `n` arguments (top-first) off the operand stack.
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            args.push(self.stack.pop().unwrap_or(0.0));
        }
        // `args` is top-first; restore natural order for re-pushing.
        args.reverse();
        match subr {
            1 => {
                // Start flex: subsequent rmovetos collect points, none drawn.
                // Snapshot the current point now — the collection moves lose it.
                self.in_flex = true;
                self.flex_start = (self.x, self.y);
                self.flex_pts.clear();
            }
            0 => {
                // End flex: emit two cubic curves from the collected points.
                self.in_flex = false;
                self.emit_flex();
                // OtherSubr 0 leaves the final (x, y) for two `pop`s + setcurrentpoint.
                self.ps_stack.push(self.y);
                self.ps_stack.push(self.x);
            }
            2 => {
                // Flex add-point: no-op (points captured via rmoveto already).
            }
            3 => {
                // Hint replacement: push back the subr# for the trailing `pop`.
                self.ps_stack.push(args.last().copied().unwrap_or(3.0));
            }
            _ => {
                // Unknown OtherSubr: echo args so the following pops succeed.
                for v in args {
                    self.ps_stack.push(v);
                }
            }
        }
    }

    /// Emit the two Béziers of a flex from the 7 collected absolute points.
    /// Point 0 is the reference (ignored for the outline); points 1..=3 and
    /// 4..=6 are the two curves' control + end points. Deltas are taken from the
    /// pre-flex current point (`flex_start`), then onward between points.
    fn emit_flex(&mut self) {
        if self.flex_pts.len() < 7 {
            // Degenerate capture: fall back to a straight line to the last point.
            if let Some(&(ex, ey)) = self.flex_pts.last() {
                self.builder
                    .rlineto(ex - self.flex_start.0, ey - self.flex_start.1);
                self.x = ex;
                self.y = ey;
            }
            self.flex_pts.clear();
            return;
        }
        let p = self.flex_pts.clone();
        let start = self.flex_start;
        // First curve: start → p1 → p2 → p3.
        self.builder.rrcurveto([
            p[1].0 - start.0,
            p[1].1 - start.1,
            p[2].0 - p[1].0,
            p[2].1 - p[1].1,
            p[3].0 - p[2].0,
            p[3].1 - p[2].1,
        ]);
        // Second curve: p3 → p4 → p5 → p6.
        self.builder.rrcurveto([
            p[4].0 - p[3].0,
            p[4].1 - p[3].1,
            p[5].0 - p[4].0,
            p[5].1 - p[4].1,
            p[6].0 - p[5].0,
            p[6].1 - p[5].1,
        ]);
        self.x = p[6].0;
        self.y = p[6].1;
        self.flex_pts.clear();
    }

    fn take6(&mut self) -> [f64; 6] {
        let mut s = [0.0; 6];
        for (k, slot) in s.iter_mut().enumerate() {
            *slot = self.stack.get(k).copied().unwrap_or(0.0);
        }
        s
    }
}

/// Convert one Type 1 charstring into a Type 2 charstring. Returns the byte
/// stream and an optional `seac` accent request `(adx, ady, bchar, achar)`.
fn convert_charstring(
    charstring: &[u8],
    subrs: &[Vec<u8>],
    nominal_width: f64,
) -> (Vec<u8>, Option<SeacRequest>) {
    let mut interp = T1Interp::new(subrs, nominal_width);
    interp.exec(charstring, 0);
    if !interp.done {
        // Charstring without an explicit endchar: terminate cleanly.
        interp.builder.endchar();
    }
    let seac = interp.seac;
    (interp.builder.out, seac)
}

// ── CFF writer ──────────────────────────────────────────────────────────────

/// Build a bare CFF (re-parsable by [`super::cff::CffFont`]) from a parsed
/// Type 1 font. Returns `None` if no glyphs convert.
pub fn to_cff(font: &Type1Font) -> Option<Vec<u8>> {
    // Order glyphs with `.notdef` at GID 0.
    let mut names: Vec<&str> = Vec::with_capacity(font.glyphs.len());
    let mut bodies: Vec<&[u8]> = Vec::with_capacity(font.glyphs.len());
    let mut notdef: Option<&[u8]> = None;
    for (name, cs) in &font.glyphs {
        if name == ".notdef" && notdef.is_none() {
            notdef = Some(cs);
        } else {
            names.push(name);
            bodies.push(cs);
        }
    }

    // Convert every glyph's charstring to Type 2; pick the dominant advance as
    // nominalWidthX so per-glyph width deltas stay compact.
    let widths = collect_widths(font);
    let nominal_width = most_common(&widths).unwrap_or(0.0);
    let default_width = notdef.and_then(|_| widths.first().copied()).unwrap_or(0.0);

    // GID 0 = .notdef.
    let mut charstrings: Vec<Vec<u8>> = Vec::with_capacity(names.len() + 1);
    let notdef_t2 = match notdef {
        Some(cs) => convert_charstring(cs, &font.subrs, nominal_width).0,
        None => {
            // Synthesise a minimal `.notdef` (just endchar).
            let mut b = T2Builder::new(nominal_width);
            b.endchar();
            b.out
        }
    };
    charstrings.push(notdef_t2);

    // Resolve a name → GID map for seac accent flattening.
    let mut name_to_gid: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (idx, n) in names.iter().enumerate() {
        name_to_gid.entry((*n).to_string()).or_insert(idx + 1);
    }

    // First pass: convert, capturing seac requests for a second resolution pass.
    let mut seacs: Vec<Option<SeacRequest>> = Vec::with_capacity(names.len());
    for body in &bodies {
        let (t2, seac) = convert_charstring(body, &font.subrs, nominal_width);
        charstrings.push(t2);
        seacs.push(seac);
    }

    // Second pass: resolve seac (composite accented glyph) by concatenating the
    // base glyph's outline with the accent's, shifted. Best-effort: if either
    // component is missing we keep the base-only charstring already produced.
    for (gid_minus1, seac) in seacs.iter().enumerate() {
        let Some((adx, ady, bchar, achar)) = *seac else {
            continue;
        };
        let base_name = standard_encoding_name(bchar);
        let accent_name = standard_encoding_name(achar);
        let (Some(bn), Some(an)) = (base_name, accent_name) else {
            continue;
        };
        let (Some(&bg), Some(&ag)) = (name_to_gid.get(bn), name_to_gid.get(an)) else {
            continue;
        };
        if let Some(t2) = build_seac_glyph(font, &bodies, nominal_width, bg, ag, adx, ady) {
            charstrings[gid_minus1 + 1] = t2;
        }
    }

    // Assign SIDs: standard names use their predefined SID; the rest go into the
    // String INDEX (SID 391 + position). `wrap`'s sid_to_unicode relies on this.
    let mut string_index: Vec<Vec<u8>> = Vec::new();
    let mut charset_sids: Vec<u16> = Vec::with_capacity(names.len());
    for n in &names {
        let sid = match standard_sid(n) {
            Some(s) => s,
            None => {
                let pos = N_STANDARD_STRINGS + string_index.len();
                string_index.push(n.as_bytes().to_vec());
                pos as u16
            }
        };
        charset_sids.push(sid);
    }

    assemble_cff(
        &font.font_name,
        &charstrings,
        &charset_sids,
        &string_index,
        &font.font_bbox,
        default_width,
        nominal_width,
    )
}

/// The most frequent value in a slice (used to pick `nominalWidthX`, which
/// minimises per-glyph width deltas). `None` for an empty slice.
fn most_common(values: &[f64]) -> Option<f64> {
    let mut best: Option<(f64, usize)> = None;
    for &v in values {
        let count = values.iter().filter(|&&x| (x - v).abs() < 0.5).count();
        if best.map(|(_, c)| count > c).unwrap_or(true) {
            best = Some((v, count));
        }
    }
    best.map(|(v, _)| v)
}

/// Per-glyph advance width (the `wx` from `hsbw`/`sbw`), in glyph order
/// (excluding `.notdef`). Used to choose nominal/default widths.
fn collect_widths(font: &Type1Font) -> Vec<f64> {
    font.glyphs
        .iter()
        .filter(|(n, _)| n != ".notdef")
        .map(|(_, cs)| charstring_width(cs, &font.subrs).unwrap_or(0.0))
        .collect()
}

/// Read just the `wx` operand of a charstring's leading `hsbw`/`sbw`.
fn charstring_width(charstring: &[u8], subrs: &[Vec<u8>]) -> Option<f64> {
    // Re-run a lightweight interpreter that records the width and stops early.
    let mut interp = T1Interp::new(subrs, 0.0);
    interp.exec(charstring, 0);
    interp.builder.width // = wx - nominal(0) = wx.
}

/// Build the Type 2 charstring for a `seac` composite: base outline followed by
/// the accent outline translated by `(adx, ady)`. We re-render both components
/// with `nominalWidthX` and stitch them, using an extra moveto to position the
/// accent.
fn build_seac_glyph(
    font: &Type1Font,
    bodies: &[&[u8]],
    nominal_width: f64,
    base_gid: usize,
    accent_gid: usize,
    adx: f64,
    ady: f64,
) -> Option<Vec<u8>> {
    let base_cs = *bodies.get(base_gid.checked_sub(1)?)?;
    let accent_cs = *bodies.get(accent_gid.checked_sub(1)?)?;

    // Render the base glyph normally (keeps its width).
    let mut interp = T1Interp::new(&font.subrs, nominal_width);
    interp.exec(base_cs, 0);
    // Don't let the base's endchar terminate the combined glyph: strip it.
    if interp.builder.out.last() == Some(&14) {
        interp.builder.out.pop();
    }
    interp.done = false;

    // Render the accent on top, offset by (adx, ady). We translate by injecting
    // the offset into the accent's hsbw side-bearing handling: reset the start
    // point to (adx, ady) before executing.
    let saved_width = interp.builder.width;
    interp.builder.width_emitted = true; // width already emitted by the base.
    interp.builder.started = true; // base already opened the path.
    interp.x = 0.0;
    interp.y = 0.0;
    // The accent's own hsbw sets its sbx; we add the seac shift via a moveto to
    // (adx + accent_sbx, ady) handled inside the accent's first moveto. To keep
    // it simple we pre-position with an explicit rmoveto after reading hsbw.
    interp.exec_accent(accent_cs, adx, ady);
    interp.builder.width = saved_width;
    interp.builder.endchar();
    Some(interp.builder.out)
}

impl T1Interp<'_> {
    /// Execute an accent charstring for `seac`, translating its outline by
    /// `(adx, ady)`. The accent's leading `hsbw` resets the current point to its
    /// own `sbx`; the next drawing moveto is then shifted by `(adx, ady)` so the
    /// accent lands over the base glyph.
    fn exec_accent(&mut self, code: &[u8], adx: f64, ady: f64) {
        // Pre-load the translation so the first moveto lands at the shifted spot.
        self.accent_shift = Some((adx, ady));
        self.exec(code, 0);
        self.accent_shift = None;
        self.done = false;
    }
}

// ── CFF binary assembly ─────────────────────────────────────────────────────

const N_STANDARD_STRINGS: usize = 391;

/// CFF DICT operand encoding for an integer (mirrors what `cff::parse_dict`
/// decodes). Offsets use the fixed 5-byte op-29 form for a deterministic layout.
fn dict_int(out: &mut Vec<u8>, v: i32) {
    if (-107..=107).contains(&v) {
        out.push((v + 139) as u8);
    } else if (108..=1131).contains(&v) {
        let v = v - 108;
        out.push((v / 256 + 247) as u8);
        out.push((v % 256) as u8);
    } else if (-1131..=-108).contains(&v) {
        let v = -v - 108;
        out.push((v / 256 + 251) as u8);
        out.push((v % 256) as u8);
    } else {
        out.push(28);
        out.extend_from_slice(&(v as i16).to_be_bytes());
    }
}

/// Fixed-width 5-byte integer (CFF op 29) — used for every absolute offset so
/// the Top DICT keeps a constant size across the offset-resolution passes.
fn dict_offset(out: &mut Vec<u8>, v: i32) {
    out.push(29);
    out.extend_from_slice(&v.to_be_bytes());
}

/// Serialise a CFF INDEX (count, offSize, offsets[count+1], data).
fn write_index(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let count = items.len() as u16;
    out.extend_from_slice(&count.to_be_bytes());
    if count == 0 {
        return out; // empty INDEX is just the count (per spec).
    }
    let total: usize = items.iter().map(|i| i.len()).sum();
    // offSize must hold the largest 1-based offset, which is total + 1.
    let max_off = total + 1;
    let off_size: u8 = if max_off <= 0xFF {
        1
    } else if max_off <= 0xFFFF {
        2
    } else if max_off <= 0xFF_FFFF {
        3
    } else {
        4
    };
    out.push(off_size);
    let mut offset = 1usize; // offsets are 1-based.
    write_offset(&mut out, offset, off_size);
    for item in items {
        offset += item.len();
        write_offset(&mut out, offset, off_size);
    }
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

fn write_offset(out: &mut Vec<u8>, value: usize, off_size: u8) {
    let bytes = value.to_be_bytes(); // 8 bytes, big-endian.
    let start = bytes.len() - off_size as usize;
    out.extend_from_slice(&bytes[start..]);
}

/// Assemble the full bare-CFF byte stream.
#[allow(clippy::too_many_arguments)]
fn assemble_cff(
    font_name: &str,
    charstrings: &[Vec<u8>],
    charset_sids: &[u16],
    string_index: &[Vec<u8>],
    bbox: &[f64; 4],
    default_width: f64,
    nominal_width: f64,
) -> Option<Vec<u8>> {
    if charstrings.is_empty() {
        return None;
    }
    let num_glyphs = charstrings.len();

    // Pre-serialise the size-stable sections (everything except the Top DICT,
    // whose offsets we resolve below).
    let name_index = write_index(&[font_name.as_bytes().to_vec()]);
    let string_index_bytes = write_index(string_index);
    let gsubr_index = write_index(&[]); // empty global subrs.
    let charstrings_index = write_index(charstrings);

    // charset format 0: a u16 SID per glyph 1..N (GID 0 .notdef is implicit).
    let mut charset = Vec::with_capacity(1 + 2 * charset_sids.len());
    charset.push(0u8);
    for &sid in charset_sids {
        charset.extend_from_slice(&sid.to_be_bytes());
    }

    // Private DICT: defaultWidthX (op 20), nominalWidthX (op 21). No local subrs.
    let mut private = Vec::new();
    dict_int(&mut private, default_width.round() as i32);
    private.push(20);
    dict_int(&mut private, nominal_width.round() as i32);
    private.push(21);
    let private_size = private.len();

    // The header is 4 bytes; Name INDEX follows, then the Top DICT INDEX. The
    // Top DICT has a *fixed* size because all offsets use op 29 (5 bytes). Build
    // it once with placeholder offsets to learn its length, then compute the
    // real offsets and rebuild.
    let build_top = |charset_off: i32, charstrings_off: i32, private_off: i32| -> Vec<u8> {
        let mut d = Vec::new();
        // FontBBox (op 5): four integers.
        for v in bbox {
            dict_int(&mut d, v.round() as i32);
        }
        d.push(5);
        // charset (op 15), CharStrings (op 17): absolute offsets.
        dict_offset(&mut d, charset_off);
        d.push(15);
        dict_offset(&mut d, charstrings_off);
        d.push(17);
        // Private (op 18): [size, offset].
        dict_int(&mut d, private_size as i32);
        dict_offset(&mut d, private_off);
        d.push(18);
        d
    };

    // Measure the Top DICT INDEX size with placeholder offsets (op 29 is fixed
    // width, so the size is invariant to the actual offset values).
    let top_probe = build_top(0, 0, 0);
    let top_index_probe = write_index(std::slice::from_ref(&top_probe));

    let header_len = 4usize;
    let after_name = header_len + name_index.len();
    let after_top = after_name + top_index_probe.len();
    let after_strings = after_top + string_index_bytes.len();
    let after_gsubr = after_strings + gsubr_index.len();

    // Lay out: charset, then CharStrings INDEX, then Private DICT.
    let charset_off = after_gsubr;
    let charstrings_off = charset_off + charset.len();
    let private_off = charstrings_off + charstrings_index.len();

    let top = build_top(
        charset_off as i32,
        charstrings_off as i32,
        private_off as i32,
    );
    let top_index = write_index(&[top]);
    debug_assert_eq!(
        top_index.len(),
        top_index_probe.len(),
        "Top DICT size stable"
    );

    let mut out = Vec::new();
    // Header: major=1, minor=0, hdrSize=4, offSize=1.
    out.extend_from_slice(&[1, 0, 4, 1]);
    out.extend_from_slice(&name_index);
    out.extend_from_slice(&top_index);
    out.extend_from_slice(&string_index_bytes);
    out.extend_from_slice(&gsubr_index);
    out.extend_from_slice(&charset);
    out.extend_from_slice(&charstrings_index);
    out.extend_from_slice(&private);

    debug_assert_eq!(num_glyphs, charstrings.len());
    Some(out)
}

// ── standard string / encoding tables (mirrors cff.rs) ──────────────────────

// The predefined CFF Standard Strings (SID 0..390), as space-separated
// fragments joined at compile time — identical content/order to `cff.rs` so a
// glyph named here gets the SID that `cff_to_otf::wrap` maps back to Unicode.
const STD_A: &str = ".notdef space exclam quotedbl numbersign dollar percent ampersand quoteright parenleft parenright asterisk plus comma hyphen period slash zero one two three four five six seven eight nine colon semicolon less equal greater question at";
const STD_B: &str = "A B C D E F G H I J K L M N O P Q R S T U V W X Y Z bracketleft backslash bracketright asciicircum underscore quoteleft";
const STD_C: &str =
    "a b c d e f g h i j k l m n o p q r s t u v w x y z braceleft bar braceright asciitilde";
const STD_D: &str = "exclamdown cent sterling fraction yen florin section currency quotesingle quotedblleft guillemotleft guilsinglleft guilsinglright fi fl endash dagger daggerdbl periodcentered paragraph bullet quotesinglbase quotedblbase quotedblright guillemotright ellipsis perthousand questiondown grave acute circumflex tilde macron breve dotaccent dieresis ring cedilla hungarumlaut ogonek caron emdash";
const STD_E: &str = "AE ordfeminine Lslash Oslash OE ordmasculine ae dotlessi lslash oslash oe germandbls onesuperior logicalnot mu trademark Eth onehalf plusminus Thorn onequarter divide brokenbar degree thorn threequarters twosuperior registered minus eth multiply threesuperior copyright";
const STD_F: &str = "Aacute Acircumflex Adieresis Agrave Aring Atilde Ccedilla Eacute Ecircumflex Edieresis Egrave Iacute Icircumflex Idieresis Igrave Ntilde Oacute Ocircumflex Odieresis Ograve Otilde Scaron Uacute Ucircumflex Udieresis Ugrave Yacute Ydieresis Zcaron aacute acircumflex adieresis agrave aring atilde ccedilla eacute ecircumflex edieresis egrave iacute icircumflex idieresis igrave ntilde oacute ocircumflex odieresis ograve otilde scaron uacute ucircumflex udieresis ugrave yacute ydieresis zcaron";

const STD_FRAGMENTS: [&str; 6] = [STD_A, STD_B, STD_C, STD_D, STD_E, STD_F];

/// Resolve a glyph name to its predefined Standard String SID, if any. Mirrors
/// `cff::standard_string` in reverse so the produced charset round-trips.
fn standard_sid(name: &str) -> Option<u16> {
    let mut base = 0usize;
    for frag in STD_FRAGMENTS {
        for (i, candidate) in frag.split(' ').enumerate() {
            if candidate == name {
                return Some((base + i) as u16);
            }
        }
        base += frag.split(' ').count();
    }
    None
}

/// StandardEncoding code → glyph name, for `/Encoding StandardEncoding` and for
/// `seac` base/accent resolution. The printable ASCII run 0x20..=0x7E maps to
/// the Standard Strings SID 1..=95 names, which are the first STD_A..STD_C
/// entries; high codes use the Latin accent names.
fn standard_encoding_name(code: u8) -> Option<&'static str> {
    // 0x20..=0x7E follows StandardEncoding == Standard Strings SID 1..=95 order.
    if (0x20..=0x7E).contains(&code) {
        let sid = (code - 0x20 + 1) as usize; // SID 1 == space (0x20).
        return standard_string_name(sid);
    }
    // The accent codes used by seac live in the 0xC0..=0xFF Adobe StandardEncoding
    // region; map the common French/Latin ones by name.
    STD_ENC_HIGH
        .iter()
        .find(|&&(c, _)| c == code)
        .map(|&(_, n)| n)
}

/// Resolve a Standard String SID to its name (forward of `standard_sid`).
fn standard_string_name(sid: usize) -> Option<&'static str> {
    let mut remaining = sid;
    for frag in STD_FRAGMENTS {
        let count = frag.split(' ').count();
        if remaining < count {
            return frag.split(' ').nth(remaining);
        }
        remaining -= count;
    }
    None
}

/// High StandardEncoding codes used by `seac` accents (Adobe StandardEncoding,
/// TN #5040 Appendix E). Only the accent marks and common composed bases are
/// needed; absent codes simply skip seac flattening.
const STD_ENC_HIGH: &[(u8, &str)] = &[
    (0xC1, "grave"),
    (0xC2, "acute"),
    (0xC3, "circumflex"),
    (0xC4, "tilde"),
    (0xC5, "macron"),
    (0xC6, "breve"),
    (0xC7, "dotaccent"),
    (0xC8, "dieresis"),
    (0xCA, "ring"),
    (0xCB, "cedilla"),
    (0xCD, "hungarumlaut"),
    (0xCE, "ogonek"),
    (0xCF, "caron"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Encrypt the inverse of [`decrypt`] for building synthetic fixtures.
    fn encrypt(plain: &[u8], mut r: u16, lead: &[u8]) -> Vec<u8> {
        const C1: u16 = 52845;
        const C2: u16 = 22719;
        let mut out = Vec::with_capacity(plain.len() + lead.len());
        // Prepend `lead.len()` random bytes (they get skipped on decrypt).
        let mut full = Vec::with_capacity(lead.len() + plain.len());
        full.extend_from_slice(lead);
        full.extend_from_slice(plain);
        for &p in &full {
            let c = p ^ (r >> 8) as u8;
            r = (c as u16).wrapping_add(r).wrapping_mul(C1).wrapping_add(C2);
            out.push(c);
        }
        out
    }

    /// Build a Type 1 charstring number operand (Type 1 uses the same compact
    /// integer encoding as the CFF DICT for the −1131..=1131 range here).
    fn t1_num(out: &mut Vec<u8>, v: i32) {
        if (-107..=107).contains(&v) {
            out.push((v + 139) as u8);
        } else if (108..=1131).contains(&v) {
            let v = v - 108;
            out.push((v / 256 + 247) as u8);
            out.push((v % 256) as u8);
        } else if (-1131..=-108).contains(&v) {
            let v = -v - 108;
            out.push((v / 256 + 251) as u8);
            out.push((v % 256) as u8);
        } else {
            out.push(255);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }

    /// Charstring for `.notdef`: `0 0 hsbw endchar`.
    fn cs_notdef() -> Vec<u8> {
        let mut c = Vec::new();
        t1_num(&mut c, 0);
        t1_num(&mut c, 0);
        c.push(13); // hsbw
        c.push(14); // endchar
        c
    }

    /// Charstring for a square-ish `A`: `sbx wx hsbw`, an rmoveto, three lines,
    /// then endchar. Exercises move/line emission and width handling.
    fn cs_letter() -> Vec<u8> {
        let mut c = Vec::new();
        t1_num(&mut c, 50); // sbx
        t1_num(&mut c, 600); // wx
        c.push(13); // hsbw
        t1_num(&mut c, 100);
        t1_num(&mut c, 0);
        c.push(21); // rmoveto 100 0
        t1_num(&mut c, 300);
        t1_num(&mut c, 0);
        c.push(5); // rlineto 300 0
        t1_num(&mut c, 0);
        t1_num(&mut c, 700);
        c.push(5); // rlineto 0 700
        t1_num(&mut c, -300);
        t1_num(&mut c, 0);
        c.push(5); // rlineto -300 0
        c.push(14); // endchar
        c
    }

    /// Assemble a minimal but structurally valid raw Type 1 font with two
    /// glyphs (`.notdef`, `A`), eexec-encrypted just like a real PFA-less file.
    fn synthetic_type1() -> Vec<u8> {
        // Clear-text header.
        let header = b"%!FontType1-1.0\n/FontName /TestFont def\n/FontMatrix [0.001 0 0 0.001 0 0] readonly def\n/FontBBox {0 -200 700 800} readonly def\n/Encoding StandardEncoding def\ncurrentfile eexec\n";

        // Private dict (cleartext form before encryption).
        let notdef = cs_notdef();
        let letter = cs_letter();
        // Each charstring is itself encrypted (r=4330) with 4 lead bytes.
        let notdef_enc = encrypt(&notdef, 4330, &[1, 2, 3, 4]);
        let letter_enc = encrypt(&letter, 4330, &[5, 6, 7, 8]);

        let mut priv_plain: Vec<u8> = Vec::new();
        priv_plain.extend_from_slice(b"dup /Private 8 dict dup begin\n");
        priv_plain.extend_from_slice(b"/lenIV 4 def\n");
        priv_plain.extend_from_slice(b"/Subrs 0 array\n");
        priv_plain.extend_from_slice(b"2 index /CharStrings 2 dict dup begin\n");
        // /.notdef <len> RD <bytes> ND
        priv_plain.extend_from_slice(b"/.notdef ");
        priv_plain.extend_from_slice(notdef_enc.len().to_string().as_bytes());
        priv_plain.extend_from_slice(b" RD ");
        priv_plain.extend_from_slice(&notdef_enc);
        priv_plain.extend_from_slice(b" ND\n");
        // /A <len> RD <bytes> ND
        priv_plain.extend_from_slice(b"/A ");
        priv_plain.extend_from_slice(letter_enc.len().to_string().as_bytes());
        priv_plain.extend_from_slice(b" RD ");
        priv_plain.extend_from_slice(&letter_enc);
        priv_plain.extend_from_slice(b" ND\n");
        priv_plain.extend_from_slice(b"end end\nreadonly put\n");

        // Encrypt the Private dict with eexec (r=55665, 4 random lead bytes).
        let priv_enc = encrypt(&priv_plain, 55665, &[0x41, 0x42, 0x43, 0x44]);

        let mut font = Vec::new();
        font.extend_from_slice(header);
        font.extend_from_slice(&priv_enc);
        // 512 zeros trailer (standard, not parsed).
        font.extend_from_slice(&[b'0'; 64]);
        font
    }

    #[test]
    fn parses_and_converts_synthetic_type1() {
        let bytes = synthetic_type1();
        let font = parse_type1(&bytes).expect("synthetic Type1 parses");
        assert_eq!(font.font_name, "TestFont");
        assert_eq!(font.units_per_em, 1000.0);
        assert_eq!(font.glyphs.len(), 2, "two glyphs (.notdef + A)");
        assert!(
            font.glyphs.iter().any(|(n, _)| n == "A"),
            "glyph A present: {:?}",
            font.glyphs.iter().map(|(n, _)| n).collect::<Vec<_>>()
        );

        let cff = to_cff(&font).expect("CFF produced");
        let parsed = crate::font::cff::CffFont::parse(&cff).expect("CFF re-parses");
        assert_eq!(parsed.num_glyphs(), 2, "GID0 .notdef + A");

        // Find the GID whose charset SID is the standard SID of "A".
        let a_sid = standard_sid("A").expect("A is a standard string");
        let a_gid = (1..parsed.num_glyphs())
            .find(|&g| parsed.gid_to_sid(g) == a_sid)
            .expect("A present in charset");
        let polys = parsed.glyph_polygons(a_gid);
        assert!(!polys.is_empty(), "A has contours after conversion");
    }

    #[test]
    fn synthetic_roundtrips_through_wrap_to_metrics() {
        let bytes = synthetic_type1();
        let font = parse_type1(&bytes).expect("parse");
        let cff = to_cff(&font).expect("to_cff");
        let otf = crate::font::cff_to_otf::wrap(&cff).expect("wrap to OTTO");
        let metrics =
            crate::font::truetype::TrueTypeFont::parse_metrics(&otf).expect("parse_metrics");
        // 'A' must map to a glyph via the synthesised cmap.
        let gid = metrics.gid_for_unicode(0x41);
        assert!(gid.is_some(), "A (U+0041) resolves through the cmap");
        assert_eq!(metrics.num_glyphs(), 2);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_type1(b"not a font at all, no eexec here").is_none());
    }

    #[test]
    fn decrypt_is_inverse_of_encrypt() {
        let plain = b"hello type1 charstring";
        let enc = encrypt(plain, 4330, &[9, 9, 9, 9]);
        let dec = decrypt(&enc, 4330, 4);
        assert_eq!(&dec, plain);
    }

    /// System Type 1 fixtures (best-effort: skipped silently when absent, but
    /// asserted fully whenever at least one is readable on the build host).
    #[test]
    fn parses_system_pfb_if_present() {
        const CANDIDATES: &[&str] = &[
            "/usr/share/fonts/X11/Type1/NimbusSansNarrow-Bold.pfb",
            "/usr/share/fonts/X11/Type1/D050000L.pfb",
            "/usr/share/fonts/X11/Type1/c0419bt_.pfb",
            "/usr/share/fonts/X11/Type1/C059-Roman.pfb",
            "/usr/share/fonts/X11/Type1/qtmr.pfb",
        ];
        let mut parsed_any = 0;
        let mut latin_validated = 0;
        for path in CANDIDATES {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            // Skip the tiny TeX stub files (a few dozen bytes, no real outlines).
            if bytes.len() < 2000 {
                continue;
            }
            // Every real Type 1 font must parse and transcode to a valid CFF —
            // this includes symbol fonts (e.g. Standard Symbols) with no "A".
            let Some(font) = parse_type1(&bytes) else {
                panic!("failed to parse {path}");
            };
            assert!(!font.font_name.is_empty(), "{path}: font name");
            assert!(
                font.glyphs.len() > 10,
                "{path}: {} glyphs",
                font.glyphs.len()
            );

            let cff = to_cff(&font).unwrap_or_else(|| panic!("{path}: to_cff"));
            let parsed = crate::font::cff::CffFont::parse(&cff)
                .unwrap_or_else(|| panic!("{path}: CFF re-parse"));
            assert!(parsed.num_glyphs() >= 10, "{path}: CFF glyph count");
            parsed_any += 1;

            // The A→contours→wrap→U+0041 chain only applies to Latin text fonts;
            // a symbol font legitimately has no "A" and is skipped for this part.
            if !font.glyphs.iter().any(|(n, _)| n == "A") {
                continue;
            }
            let a_sid = standard_sid("A").unwrap();
            let a_gid = (1..parsed.num_glyphs())
                .find(|&g| parsed.gid_to_sid(g) == a_sid)
                .unwrap_or_else(|| panic!("{path}: A in charset"));
            assert!(
                !parsed.glyph_polygons(a_gid).is_empty(),
                "{path}: A has contours"
            );

            let otf = crate::font::cff_to_otf::wrap(&cff).unwrap_or_else(|| panic!("{path}: wrap"));
            let metrics = crate::font::truetype::TrueTypeFont::parse_metrics(&otf)
                .unwrap_or_else(|| panic!("{path}: parse_metrics"));
            assert_eq!(
                metrics.gid_for_unicode(0x41),
                Some(a_gid),
                "{path}: U+0041 → A's GID"
            );
            latin_validated += 1;
        }
        // When no system font exists (minimal CI image) the test is a no-op;
        // otherwise we expect at least one Latin font to fully round-trip.
        if parsed_any == 0 {
            eprintln!("no system Type1 fixture available; skipped");
        } else {
            assert!(
                latin_validated > 0,
                "parsed {parsed_any} Type1 font(s) but none Latin to validate A→U+0041"
            );
        }
    }
}
