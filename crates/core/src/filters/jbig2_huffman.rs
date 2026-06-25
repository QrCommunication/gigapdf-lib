//! JBIG2 Huffman coding (ITU-T T.88 Annex B) for the Huffman-coded symbol
//! dictionary / text-region path. Pure `std`, zero dependencies.
//!
//! A JBIG2 Huffman table is a list of *lines*, each describing a contiguous
//! range of integer values reachable through one prefix code. A line carries a
//! prefix length `PREFLEN`, a range length `RANGELEN` (number of value bits read
//! after the prefix) and a range low value `RANGELOW`; the decoded value is
//! `RANGELOW + read(RANGELEN)`. Two special range kinds extend a table to
//! ±infinity (the *lower-range* line subtracts the read magnitude from
//! `RANGELOW`, the *upper-range* line adds it), and an optional *OOB* line
//! signals out-of-band. Prefix codes are assigned canonically by ascending
//! `(PREFLEN, table order)` exactly as the standard prescribes (B.3), so a table
//! is fully specified by its lines' lengths.
//!
//! This module provides the fifteen standard tables B.1–B.15, a custom-table
//! builder for table segments (§7.4.13), the run-code-built symbol-ID table for
//! Huffman text regions (§7.4.3.1.7), and an MSB-first bit reader.

/// One line of a Huffman table.
#[derive(Clone, Copy)]
pub(crate) struct Line {
    /// Prefix code length in bits (0 = line unused / no code assigned).
    pub(crate) preflen: u8,
    /// Number of value bits read after the prefix.
    pub(crate) rangelen: u8,
    /// The low end of the value range (added to the read magnitude).
    pub(crate) rangelow: i32,
    /// Range kind: 0 = normal, 1 = lower (subtract), 2 = upper (add, 32-bit),
    /// 3 = OOB.
    pub(crate) kind: u8,
}

impl Line {
    const fn normal(preflen: u8, rangelen: u8, rangelow: i32) -> Self {
        Self {
            preflen,
            rangelen,
            rangelow,
            kind: 0,
        }
    }
    /// A lower-range line: value = `rangelow - read(32)`.
    const fn lower(preflen: u8, rangelow: i32) -> Self {
        Self {
            preflen,
            rangelen: 32,
            rangelow,
            kind: 1,
        }
    }
    /// An upper-range line: value = `rangelow + read(32)`.
    const fn upper(preflen: u8, rangelow: i32) -> Self {
        Self {
            preflen,
            rangelen: 32,
            rangelow,
            kind: 2,
        }
    }
    /// The out-of-band line.
    const fn oob(preflen: u8) -> Self {
        Self {
            preflen,
            rangelen: 0,
            rangelow: 0,
            kind: 3,
        }
    }
}

/// The result of a Huffman decode: a value or OOB.
pub(crate) enum HuffResult {
    Value(i32),
    Oob,
}

/// A built Huffman table: lines plus their assigned prefix codes.
pub(crate) struct HuffTable {
    lines: Vec<Line>,
    codes: Vec<u32>,
}

impl HuffTable {
    /// Build a table from its lines, assigning canonical prefix codes (B.3).
    pub(crate) fn new(lines: Vec<Line>) -> Self {
        let codes = assign_prefix_codes(&lines);
        Self { lines, codes }
    }

    /// Decode one value (or OOB) from `r` (B.4). Returns `None` on bit underflow
    /// or a code that matches no line.
    pub(crate) fn decode(&self, r: &mut BitReader) -> Option<HuffResult> {
        // Read prefix bits one at a time, matching against assigned codes.
        let mut len = 0u8;
        let mut code: u32 = 0;
        // The longest possible prefix bounds the loop.
        let max_len = self.lines.iter().map(|l| l.preflen).max().unwrap_or(0);
        while len <= max_len {
            let bit = r.bit()? as u32;
            code = (code << 1) | bit;
            len += 1;
            for (i, line) in self.lines.iter().enumerate() {
                if line.preflen == len && self.codes[i] == code {
                    return self.read_line_value(r, line);
                }
            }
        }
        None
    }

    fn read_line_value(&self, r: &mut BitReader, line: &Line) -> Option<HuffResult> {
        match line.kind {
            3 => Some(HuffResult::Oob),
            1 => {
                // Lower range: value = rangelow - read(32).
                let mag = r.bits(line.rangelen as u32)? as i64;
                Some(HuffResult::Value((line.rangelow as i64 - mag) as i32))
            }
            2 => {
                // Upper range: value = rangelow + read(32).
                let mag = r.bits(line.rangelen as u32)? as i64;
                Some(HuffResult::Value((line.rangelow as i64 + mag) as i32))
            }
            _ => {
                let mag = if line.rangelen == 0 {
                    0
                } else {
                    r.bits(line.rangelen as u32)? as i64
                };
                Some(HuffResult::Value((line.rangelow as i64 + mag) as i32))
            }
        }
    }
}

/// Assign canonical prefix codes to a table's lines (T.88 B.3). Codes are given
/// in ascending order of prefix length, ties broken by table order; lines with
/// `PREFLEN == 0` get no code (left at 0, never matched since `preflen != len`).
pub(crate) fn assign_prefix_codes(lines: &[Line]) -> Vec<u32> {
    let max_len = lines.iter().map(|l| l.preflen).max().unwrap_or(0) as usize;
    // Histogram of prefix lengths.
    let mut len_count = vec![0u32; max_len + 1];
    for l in lines {
        if l.preflen > 0 {
            len_count[l.preflen as usize] += 1;
        }
    }
    // First code for each length (B.3 procedure).
    let mut first_code = vec![0u32; max_len + 2];
    let mut cur: u32 = 0;
    len_count[0] = 0;
    for len in 1..=max_len {
        first_code[len] = cur;
        cur = (cur + len_count[len]) << 1;
    }
    // Assign each line the next code for its length, in table order.
    let mut next_code = first_code.clone();
    let mut codes = vec![0u32; lines.len()];
    for (i, l) in lines.iter().enumerate() {
        if l.preflen > 0 {
            let len = l.preflen as usize;
            codes[i] = next_code[len];
            next_code[len] += 1;
        }
    }
    codes
}

/// An MSB-first bit reader over a byte slice, used for the Huffman-coded payload
/// (and for run-code length decoding). Distinct from the MQ coder's reader.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Bit position from the start of `data`.
    bitpos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, bitpos: 0 }
    }

    /// Read one bit (MSB first). `None` past the end.
    pub(crate) fn bit(&mut self) -> Option<u8> {
        let byte = self.bitpos / 8;
        if byte >= self.data.len() {
            return None;
        }
        let bit = 7 - (self.bitpos % 8);
        self.bitpos += 1;
        Some((self.data[byte] >> bit) & 1)
    }

    /// Read `n` bits (MSB first) into a `u32` (`n <= 32`). `None` past the end.
    pub(crate) fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v: u32 = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u32;
        }
        Some(v)
    }

    /// Advance to the next byte boundary (used between a Huffman prefix and an
    /// MMR/collective bitmap that must start byte-aligned).
    pub(crate) fn byte_align(&mut self) {
        if !self.bitpos.is_multiple_of(8) {
            self.bitpos = (self.bitpos / 8 + 1) * 8;
        }
    }

    /// The current byte offset (rounded down) — the start of the unread tail for
    /// handing a byte-aligned remainder to another decoder (e.g. MMR).
    pub(crate) fn byte_pos(&self) -> usize {
        self.bitpos / 8
    }

    pub(crate) fn data(&self) -> &'a [u8] {
        self.data
    }
}

// ---------------------------------------------------------------------------
// Standard tables B.1 – B.15 (T.88 Annex B.5, Tables B.1..B.15)
// ---------------------------------------------------------------------------

/// Return one of the fifteen standard Huffman tables by its 1-based number
/// (1..=15). Numbers outside that range yield `None`.
pub(crate) fn standard_table(n: u8) -> Option<HuffTable> {
    let lines: Vec<Line> = match n {
        1 => vec![
            Line::normal(1, 4, 0),
            Line::normal(2, 8, 16),
            Line::normal(3, 16, 272),
            Line::upper(3, 65808),
        ],
        2 => vec![
            Line::normal(1, 0, 0),
            Line::normal(2, 0, 1),
            Line::normal(3, 0, 2),
            Line::normal(4, 3, 3),
            Line::normal(5, 6, 11),
            Line::upper(6, 75),
            Line::oob(6),
        ],
        3 => vec![
            Line::normal(8, 8, -256),
            Line::normal(1, 0, 0),
            Line::normal(2, 0, 1),
            Line::normal(3, 0, 2),
            Line::normal(4, 3, 3),
            Line::normal(5, 6, 11),
            Line::lower(8, -257),
            Line::upper(7, 75),
            Line::oob(6),
        ],
        4 => vec![
            Line::normal(1, 0, 1),
            Line::normal(2, 0, 2),
            Line::normal(3, 0, 3),
            Line::normal(4, 3, 4),
            Line::normal(5, 6, 12),
            Line::upper(5, 76),
        ],
        5 => vec![
            Line::normal(7, 8, -255),
            Line::normal(1, 0, 1),
            Line::normal(2, 0, 2),
            Line::normal(3, 0, 3),
            Line::normal(4, 3, 4),
            Line::normal(5, 6, 12),
            Line::lower(7, -256),
            Line::upper(6, 76),
        ],
        6 => vec![
            Line::normal(5, 10, -2048),
            Line::normal(4, 9, -1024),
            Line::normal(4, 8, -512),
            Line::normal(4, 7, -256),
            Line::normal(5, 6, -128),
            Line::normal(5, 5, -64),
            Line::normal(4, 5, -32),
            Line::normal(2, 7, 0),
            Line::normal(3, 7, 128),
            Line::normal(3, 8, 256),
            Line::normal(4, 9, 512),
            Line::normal(4, 10, 1024),
            Line::lower(6, -2049),
            Line::upper(6, 2048),
        ],
        7 => vec![
            Line::normal(4, 9, -1024),
            Line::normal(3, 8, -512),
            Line::normal(4, 7, -256),
            Line::normal(5, 6, -128),
            Line::normal(5, 5, -64),
            Line::normal(4, 5, -32),
            Line::normal(4, 5, 0),
            Line::normal(5, 5, 32),
            Line::normal(5, 6, 64),
            Line::normal(4, 7, 128),
            Line::normal(3, 8, 256),
            Line::normal(3, 9, 512),
            Line::normal(3, 10, 1024),
            Line::lower(5, -1025),
            Line::upper(5, 2048),
        ],
        8 => vec![
            Line::normal(8, 3, -15),
            Line::normal(9, 1, -7),
            Line::normal(8, 1, -5),
            Line::normal(9, 0, -3),
            Line::normal(7, 0, -2),
            Line::normal(4, 0, -1),
            Line::normal(2, 1, 0),
            Line::normal(5, 0, 2),
            Line::normal(6, 0, 3),
            Line::normal(3, 4, 4),
            Line::normal(6, 1, 20),
            Line::normal(4, 4, 22),
            Line::normal(4, 5, 38),
            Line::normal(5, 6, 70),
            Line::normal(5, 7, 134),
            Line::normal(6, 7, 262),
            Line::normal(7, 8, 390),
            Line::normal(6, 10, 646),
            Line::lower(9, -16),
            Line::upper(9, 1670),
            Line::oob(2),
        ],
        9 => vec![
            Line::normal(8, 4, -31),
            Line::normal(9, 2, -15),
            Line::normal(8, 2, -11),
            Line::normal(9, 1, -7),
            Line::normal(7, 1, -5),
            Line::normal(4, 1, -3),
            Line::normal(3, 1, -1),
            Line::normal(3, 1, 1),
            Line::normal(5, 1, 3),
            Line::normal(6, 1, 5),
            Line::normal(3, 5, 7),
            Line::normal(6, 2, 39),
            Line::normal(4, 5, 43),
            Line::normal(4, 6, 75),
            Line::normal(5, 7, 139),
            Line::normal(5, 8, 267),
            Line::normal(6, 8, 523),
            Line::normal(7, 9, 779),
            Line::normal(6, 11, 1291),
            Line::lower(9, -32),
            Line::upper(9, 3339),
            Line::oob(2),
        ],
        10 => vec![
            Line::normal(7, 4, -21),
            Line::normal(8, 0, -5),
            Line::normal(7, 0, -4),
            Line::normal(5, 0, -3),
            Line::normal(2, 2, -2),
            Line::normal(5, 0, 2),
            Line::normal(6, 0, 3),
            Line::normal(7, 0, 4),
            Line::normal(8, 0, 5),
            Line::normal(2, 6, 6),
            Line::normal(5, 5, 70),
            Line::normal(6, 5, 102),
            Line::normal(6, 6, 134),
            Line::normal(6, 7, 198),
            Line::normal(6, 8, 326),
            Line::normal(6, 9, 582),
            Line::normal(6, 10, 1094),
            Line::normal(7, 11, 2118),
            Line::lower(8, -22),
            Line::upper(8, 4166),
            Line::oob(2),
        ],
        11 => vec![
            Line::normal(1, 0, 1),
            Line::normal(2, 1, 2),
            Line::normal(4, 0, 4),
            Line::normal(4, 1, 5),
            Line::normal(5, 1, 7),
            Line::normal(5, 2, 9),
            Line::normal(6, 2, 13),
            Line::normal(7, 2, 17),
            Line::normal(7, 3, 21),
            Line::normal(7, 4, 29),
            Line::normal(7, 5, 45),
            Line::normal(7, 6, 77),
            Line::upper(7, 141),
        ],
        12 => vec![
            Line::normal(1, 0, 1),
            Line::normal(2, 0, 2),
            Line::normal(3, 1, 3),
            Line::normal(5, 0, 5),
            Line::normal(5, 1, 6),
            Line::normal(6, 1, 8),
            Line::normal(7, 0, 10),
            Line::normal(7, 1, 11),
            Line::normal(7, 2, 13),
            Line::normal(7, 3, 17),
            Line::normal(7, 4, 25),
            Line::normal(8, 5, 41),
            Line::normal(8, 6, 73),
            Line::upper(8, 137),
        ],
        13 => vec![
            Line::normal(1, 0, 1),
            Line::normal(3, 0, 2),
            Line::normal(4, 0, 3),
            Line::normal(5, 0, 4),
            Line::normal(4, 1, 5),
            Line::normal(3, 3, 7),
            Line::normal(6, 1, 15),
            Line::normal(6, 2, 17),
            Line::normal(6, 3, 21),
            Line::normal(6, 4, 29),
            Line::normal(6, 5, 45),
            Line::normal(7, 6, 77),
            Line::upper(7, 141),
        ],
        14 => vec![
            Line::normal(3, 0, -2),
            Line::normal(3, 0, -1),
            Line::normal(1, 0, 0),
            Line::normal(3, 0, 1),
            Line::normal(3, 0, 2),
        ],
        15 => vec![
            Line::normal(7, 4, -24),
            Line::normal(6, 2, -8),
            Line::normal(5, 1, -4),
            Line::normal(4, 0, -2),
            Line::normal(3, 0, -1),
            Line::normal(1, 0, 0),
            Line::normal(3, 0, 1),
            Line::normal(4, 0, 2),
            Line::normal(5, 1, 3),
            Line::normal(6, 2, 5),
            Line::normal(7, 4, 9),
            Line::lower(7, -25),
            Line::upper(7, 25),
        ],
        _ => return None,
    };
    Some(HuffTable::new(lines))
}

// ---------------------------------------------------------------------------
// Custom table segment (§7.4.13)
// ---------------------------------------------------------------------------

/// Build a Huffman table from a *table segment*'s data (§7.4.13). The segment
/// flags select OOB presence and the bit-widths of the per-line prefix lengths
/// (`HTPS`) and range lengths (`HTRS`); `HTLOW`/`HTHIGH` bound the explicit
/// value lines, each carrying an `HTPS`-bit prefix length and an `HTRS`-bit
/// range length implied by the running low value. Two boundary lines (lower and
/// upper range) and an optional OOB line complete the table. Returns `None` on
/// malformed input.
pub(crate) fn build_custom_table(data: &[u8]) -> Option<HuffTable> {
    let mut r = BitReader::new(data);
    let flags = r.bits(8)?;
    let htoob = (flags & 0x01) != 0;
    let htps = ((flags >> 1) & 0x07) + 1; // prefix-size bits
    let htrs = ((flags >> 4) & 0x07) + 1; // range-size bits
    let htlow = read_i32(&mut r)?;
    let hthigh = read_i32(&mut r)?;

    let mut lines: Vec<Line> = Vec::new();
    let mut cur: i64 = htlow as i64;
    // Explicit value lines until the running low reaches HTHIGH.
    while cur < hthigh as i64 {
        let preflen = r.bits(htps)? as u8;
        let rangelen = r.bits(htrs)? as u8;
        lines.push(Line::normal(preflen, rangelen, cur as i32));
        cur += 1i64 << rangelen;
    }
    // Lower-range line.
    let low_pref = r.bits(htps)? as u8;
    lines.push(Line::lower(low_pref, htlow - 1));
    // Upper-range line.
    let high_pref = r.bits(htps)? as u8;
    lines.push(Line::upper(high_pref, hthigh));
    // Optional OOB line.
    if htoob {
        let oob_pref = r.bits(htps)? as u8;
        lines.push(Line::oob(oob_pref));
    }
    Some(HuffTable::new(lines))
}

/// Read a big-endian signed 32-bit integer from the bit reader.
fn read_i32(r: &mut BitReader) -> Option<i32> {
    Some(r.bits(32)? as i32)
}

// ---------------------------------------------------------------------------
// Test-only encoders (build Huffman bitstreams to round-trip the decoders)
// ---------------------------------------------------------------------------

/// An MSB-first bit *writer*, the inverse of [`BitReader`]. Test-only; used by
/// the JBIG2 Huffman round-trip tests to build symbol-dictionary / text-region
/// bitstreams.
#[cfg(test)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    nbits: usize,
}

#[cfg(test)]
impl BitWriter {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            nbits: 0,
        }
    }
    pub(crate) fn put(&mut self, value: u32, len: u32) {
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
    /// Pad to a byte boundary with zero bits.
    pub(crate) fn byte_align(&mut self) {
        while !self.nbits.is_multiple_of(8) {
            self.put(0, 1);
        }
    }
    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
impl HuffTable {
    /// Encode `value` through this table: find the line covering it, emit its
    /// assigned prefix code, then the range bits. Test-only (inverse of
    /// [`HuffTable::decode`]).
    pub(crate) fn encode(&self, w: &mut BitWriter, value: i32) {
        for (i, line) in self.lines.iter().enumerate() {
            let covers = match line.kind {
                3 => false,
                1 => value <= line.rangelow,
                2 => value >= line.rangelow,
                _ => {
                    value >= line.rangelow
                        && (value as i64) < line.rangelow as i64 + (1i64 << line.rangelen)
                }
            };
            if line.preflen > 0 && covers {
                w.put(self.codes[i], line.preflen as u32);
                match line.kind {
                    1 => w.put((line.rangelow - value) as u32, line.rangelen as u32),
                    2 => w.put((value - line.rangelow) as u32, line.rangelen as u32),
                    _ if line.rangelen > 0 => {
                        w.put((value - line.rangelow) as u32, line.rangelen as u32)
                    }
                    _ => {}
                }
                return;
            }
        }
        panic!("no Huffman line covers value {value}");
    }

    /// Emit the OOB code (panics if the table has no OOB line). Test-only.
    pub(crate) fn encode_oob(&self, w: &mut BitWriter) {
        for (i, line) in self.lines.iter().enumerate() {
            if line.kind == 3 && line.preflen > 0 {
                w.put(self.codes[i], line.preflen as u32);
                return;
            }
        }
        panic!("table has no OOB line");
    }
}

/// Build a symbol-ID table directly from per-symbol code lengths (each symbol
/// index becomes a value line with the given length). Test-only convenience so
/// the JBIG2 tests can mirror the decoder's symbol-ID table without constructing
/// `Line`s directly.
#[cfg(test)]
pub(crate) fn id_table_from_lengths(code_lengths: &[u8]) -> HuffTable {
    let lines: Vec<Line> = code_lengths
        .iter()
        .enumerate()
        .map(|(idx, &len)| Line::normal(len, 0, idx as i32))
        .collect();
    HuffTable::new(lines)
}

/// Build the 35 four-bit run-code lengths + symbol code lengths for a Huffman
/// text region's symbol-ID table, the inverse of [`build_symbol_id_table`].
/// Test-only: assigns each of the `code_lengths` directly with a simple scheme
/// (no run-length compression), emitting the run-code header then the per-symbol
/// lengths through the run-code table.
#[cfg(test)]
pub(crate) fn encode_symbol_id_table(w: &mut BitWriter, code_lengths: &[u8]) {
    // Use run codes 0..=31 to express literal lengths directly (no 32/33/34
    // repeats). Give run codes their own canonical lengths via a small table: we
    // assign every run-code length value present in `code_lengths` a 5-bit code.
    // Simpler: make all 35 run codes have length 5 except we only use codes equal
    // to the literal lengths. Build a run-code table where run code `c` has a
    // prefix length that makes it decodable, then emit each symbol's length as the
    // run code equal to that length.
    // Determine which literal lengths occur (0..=31).
    let mut used = [false; 35];
    for &l in code_lengths {
        used[l as usize] = true;
    }
    // Assign a uniform prefix length to every used run code so they form a valid
    // (canonical) prefix set: count = number used, prefix length = ceil(log2).
    let count = used.iter().filter(|&&u| u).count().max(1);
    let mut plen = 0u32;
    while (1usize << plen) < count {
        plen += 1;
    }
    plen = plen.max(1);
    // Emit the 35 run-code prefix lengths (4 bits each): used codes get `plen`,
    // others 0.
    let mut runcode_lines: Vec<Line> = Vec::with_capacity(35);
    for (i, u) in used.iter().enumerate() {
        let len = if *u { plen as u8 } else { 0 };
        w.put(len as u32, 4);
        runcode_lines.push(Line::normal(len, 0, i as i32));
    }
    // Build the run-code table to obtain each code's assigned prefix code.
    let runcode_table = HuffTable::new(runcode_lines);
    // Emit each symbol's length as the run code equal to that length.
    for &l in code_lengths {
        // Find the run-code line for value `l` and emit its prefix code.
        let idx = l as usize;
        w.put(runcode_table.codes[idx], plen);
    }
}

// ---------------------------------------------------------------------------
// Symbol-ID run-code table (§7.4.3.1.7) for Huffman text regions
// ---------------------------------------------------------------------------

/// Build the per-symbol Huffman code-length table for a Huffman text region
/// (§7.4.3.1.7). The bitstream first carries 35 four-bit *run-code* lengths;
/// those form a run-code Huffman table that is then used to read `n_symbols`
/// symbol code lengths (with codes 32/33/34 meaning "repeat" runs). The decoded
/// code lengths are assembled into the final symbol-ID table. Returns the table
/// (decoding a symbol index) on success.
pub(crate) fn build_symbol_id_table(r: &mut BitReader, n_symbols: usize) -> Option<HuffTable> {
    // 35 run-code prefix lengths, 4 bits each.
    let mut runcode_lines: Vec<Line> = Vec::with_capacity(35);
    for i in 0..35 {
        let len = r.bits(4)? as u8;
        runcode_lines.push(Line::normal(len, 0, i));
    }
    let runcode_table = HuffTable::new(runcode_lines);

    // Decode `n_symbols` code lengths using the run-code table.
    let mut code_lengths = vec![0u8; n_symbols];
    let mut prev_len: u8 = 0;
    let mut i = 0usize;
    while i < n_symbols {
        let code = match runcode_table.decode(r)? {
            HuffResult::Value(v) => v,
            HuffResult::Oob => return None,
        };
        if code < 32 {
            code_lengths[i] = code as u8;
            if code > 0 {
                prev_len = code as u8;
            }
            i += 1;
        } else if code == 32 {
            // Repeat the previous length 3 + read(2) times.
            let n = 3 + r.bits(2)? as usize;
            for _ in 0..n {
                if i >= n_symbols {
                    break;
                }
                code_lengths[i] = prev_len;
                i += 1;
            }
        } else if code == 33 {
            // Repeat length 0, 3 + read(3) times.
            let n = 3 + r.bits(3)? as usize;
            for _ in 0..n {
                if i >= n_symbols {
                    break;
                }
                code_lengths[i] = 0;
                i += 1;
            }
        } else {
            // code == 34: repeat length 0, 11 + read(7) times.
            let n = 11 + r.bits(7)? as usize;
            for _ in 0..n {
                if i >= n_symbols {
                    break;
                }
                code_lengths[i] = 0;
                i += 1;
            }
        }
    }
    // Build the symbol-ID table: each symbol index is a value line with the
    // decoded code length, no range bits.
    let lines: Vec<Line> = code_lengths
        .iter()
        .enumerate()
        .map(|(idx, &len)| Line::normal(len, 0, idx as i32))
        .collect();
    Some(HuffTable::new(lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a value through a built table (delegates to [`HuffTable::encode`]);
    /// the `codes` argument is the table's own assigned codes.
    fn encode_value(table: &HuffTable, _codes: &[u32], w: &mut BitWriter, value: i32) {
        table.encode(w, value);
    }

    #[test]
    fn assign_prefix_codes_is_canonical() {
        // Table B.1: lengths 1,2,3,3 → codes 0, 10, 110, 111.
        let table = standard_table(1).unwrap();
        assert_eq!(table.codes, vec![0b0, 0b10, 0b110, 0b111]);
    }

    #[test]
    fn standard_table_b1_roundtrip() {
        let table = standard_table(1).unwrap();
        let codes = table.codes.clone();
        for &v in &[0, 5, 15, 16, 100, 271, 272, 1000, 65807, 70000] {
            let mut w = BitWriter::new();
            encode_value(&table, &codes, &mut w, v);
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            match table.decode(&mut r).unwrap() {
                HuffResult::Value(got) => assert_eq!(got, v, "B.1 value {v}"),
                HuffResult::Oob => panic!("unexpected OOB for {v}"),
            }
        }
    }

    #[test]
    fn standard_table_b2_oob_roundtrip() {
        let table = standard_table(2).unwrap();
        let codes = table.codes.clone();
        // Values plus the OOB line.
        for &v in &[0, 1, 2, 3, 10, 11, 74, 75, 200] {
            let mut w = BitWriter::new();
            encode_value(&table, &codes, &mut w, v);
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            match table.decode(&mut r).unwrap() {
                HuffResult::Value(got) => assert_eq!(got, v, "B.2 value {v}"),
                HuffResult::Oob => panic!("unexpected OOB for {v}"),
            }
        }
        // The OOB line is the last; emit its code directly.
        let oob_idx = table.lines.len() - 1;
        let mut w = BitWriter::new();
        w.put(codes[oob_idx], table.lines[oob_idx].preflen as u32);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert!(matches!(table.decode(&mut r).unwrap(), HuffResult::Oob));
    }

    #[test]
    fn standard_table_b3_lower_range_roundtrip() {
        // B.3 has a lower-range line reaching negative infinity.
        let table = standard_table(3).unwrap();
        let codes = table.codes.clone();
        for &v in &[-300, -257, -256, -100, 0, 1, 11, 74, 75, 500] {
            let mut w = BitWriter::new();
            encode_value(&table, &codes, &mut w, v);
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            match table.decode(&mut r).unwrap() {
                HuffResult::Value(got) => assert_eq!(got, v, "B.3 value {v}"),
                HuffResult::Oob => panic!("unexpected OOB for {v}"),
            }
        }
    }

    #[test]
    fn custom_table_builds_expected_code_lengths() {
        // Build a tiny custom table by hand: HTLOW=0, HTHIGH=4, HTPS=3, HTRS=2,
        // no OOB. Four value lines (rangelen=0 each → low advances by 1), so the
        // explicit lines cover 0,1,2,3 then the boundary lines.
        let mut w = BitWriter::new();
        // flags: HTOOB=0, HTPS-1=2 (bits1..3), HTRS-1=1 (bits4..6) → HTPS=3,HTRS=2.
        // byte = (1 << 4) | (2 << 1) | 0 = 0x14.
        w.put(0x14, 8);
        w.put(0, 32); // HTLOW = 0
        w.put(4, 32); // HTHIGH = 4
                      // Four explicit lines, each: preflen(3 bits), rangelen(2 bits)=0.
                      // Give lengths 1,2,3,3 (a canonical prefix-free set with the
                      // boundary lines getting longer codes).
        w.put(2, 3); // line 0 preflen=2
        w.put(0, 2); // rangelen=0
        w.put(2, 3); // line 1 preflen=2
        w.put(0, 2);
        w.put(3, 3); // line 2 preflen=3
        w.put(0, 2);
        w.put(3, 3); // line 3 preflen=3
        w.put(0, 2);
        // Lower-range prefix length (give it 0 = unused since values >= 0 here).
        w.put(0, 3);
        // Upper-range prefix length.
        w.put(3, 3);
        let bytes = w.finish();
        let table = build_custom_table(&bytes).expect("custom table");
        // Expect 4 explicit + lower + upper = 6 lines; the explicit lines carry
        // rangelow 0,1,2,3 with the lengths we set.
        assert_eq!(table.lines.len(), 6);
        assert_eq!(table.lines[0].rangelow, 0);
        assert_eq!(table.lines[0].preflen, 2);
        assert_eq!(table.lines[1].rangelow, 1);
        assert_eq!(table.lines[2].rangelow, 2);
        assert_eq!(table.lines[3].rangelow, 3);
        assert_eq!(table.lines[3].rangelen, 0);
        // Round-trip the explicit values via the canonical codes.
        let codes = table.codes.clone();
        for v in [0, 1, 2, 3] {
            let mut bw = BitWriter::new();
            encode_value(&table, &codes, &mut bw, v);
            let b = bw.finish();
            let mut br = BitReader::new(&b);
            match table.decode(&mut br).unwrap() {
                HuffResult::Value(got) => assert_eq!(got, v),
                HuffResult::Oob => panic!("unexpected OOB"),
            }
        }
    }
}
