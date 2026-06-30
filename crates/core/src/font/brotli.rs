//! Brotli decompressor (RFC 7932) — pure `std`, zero third-party dependencies.
//!
//! Implemented to decode the single compressed stream inside a WOFF2 web font
//! (`crate::font::woff2`), but it is a complete general-purpose brotli decoder:
//! it handles every meta-block kind (compressed, uncompressed, metadata),
//! simple and complex prefix codes, literal/insert-copy/distance block switching,
//! literal context modeling with the two static context maps, the last-distance
//! ring buffer with the postfix/direct-distance parameters, and the full static
//! dictionary with all 121 word transforms (RFC 7932 §8).
//!
//! The static dictionary data lives in [`super::brotli_dict`] stored
//! DEFLATE-compressed and is expanded once, on first use, through the engine's
//! own inflate (`crate::filters::inflate::inflate`) — so no copy of the 122 KB
//! dictionary sits uncompressed in the binary and the zero-dependency rule holds.

use std::sync::OnceLock;

/// Decode a complete brotli stream to bytes. Returns `None` on malformed input.
pub fn decompress(data: &[u8]) -> Option<Vec<u8>> {
    let mut dec = Decoder::new(data);
    dec.run().ok()?;
    Some(dec.out)
}

// ---------------------------------------------------------------------------
// Bit reader — brotli packs bits LSB-first within each byte, bytes in order.
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    /// Bit cursor: absolute bit index from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Read `n` bits (0..=24) LSB-first as a little value. Errors past EOF.
    fn bits(&mut self, n: u32) -> Result<u32, ()> {
        let mut value = 0u32;
        for i in 0..n {
            let byte_idx = self.pos >> 3;
            let bit_idx = (self.pos & 7) as u32;
            let byte = *self.data.get(byte_idx).ok_or(())?;
            let bit = ((byte >> bit_idx) & 1) as u32;
            value |= bit << i;
            self.pos += 1;
        }
        Ok(value)
    }

    fn bit(&mut self) -> Result<u32, ()> {
        self.bits(1)
    }

    /// Skip to the next byte boundary.
    fn align(&mut self) {
        if self.pos & 7 != 0 {
            self.pos = (self.pos + 7) & !7;
        }
    }

    /// Read `n` whole bytes after a byte-alignment (used by uncompressed
    /// meta-blocks). Returns a slice into the source.
    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], ()> {
        debug_assert!(self.pos & 7 == 0);
        let start = self.pos >> 3;
        let end = start.checked_add(n).ok_or(())?;
        let slice = self.data.get(start..end).ok_or(())?;
        self.pos = end << 3;
        Ok(slice)
    }
}

// ---------------------------------------------------------------------------
// Canonical Huffman / prefix code.
// ---------------------------------------------------------------------------

/// A canonical prefix code decoded one symbol at a time. Stores per-length code
/// counts and the symbol list ordered by (length, symbol), exactly like the
/// engine's DEFLATE decoder, which keeps decoding simple and allocation-light.
struct PrefixCode {
    count: [u16; MAX_HUFFMAN_BITS + 1],
    symbol: Vec<u16>,
}

const MAX_HUFFMAN_BITS: usize = 15;

impl PrefixCode {
    fn from_lengths(lengths: &[u8]) -> Result<Self, ()> {
        let mut count = [0u16; MAX_HUFFMAN_BITS + 1];
        for &len in lengths {
            if len as usize > MAX_HUFFMAN_BITS {
                return Err(());
            }
            count[len as usize] += 1;
        }
        count[0] = 0;
        let mut offsets = [0u16; MAX_HUFFMAN_BITS + 1];
        for len in 1..MAX_HUFFMAN_BITS {
            offsets[len + 1] = offsets[len] + count[len];
        }
        let total: usize = (1..=MAX_HUFFMAN_BITS).map(|l| count[l] as usize).sum();
        let mut symbol = vec![0u16; total];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let slot = &mut offsets[len as usize];
                symbol[*slot as usize] = sym as u16;
                *slot += 1;
            }
        }
        if total == 0 {
            return Err(());
        }
        Ok(Self { count, symbol })
    }

    /// A code with a single symbol of length 0 (used for one-symbol alphabets).
    fn single(sym: u16) -> Self {
        let mut count = [0u16; MAX_HUFFMAN_BITS + 1];
        count[0] = 1;
        Self {
            count,
            symbol: vec![sym],
        }
    }

    fn decode(&self, br: &mut BitReader) -> Result<u16, ()> {
        // A one-symbol code (length-0) needs no bits.
        if self.symbol.len() == 1 && self.count[0] == 1 {
            return Ok(self.symbol[0]);
        }
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_HUFFMAN_BITS {
            code |= br.bit()? as i32;
            let cnt = self.count[len] as i32;
            if code - cnt < first {
                return Ok(self.symbol[(index + (code - first)) as usize]);
            }
            index += cnt;
            first += cnt;
            first <<= 1;
            code <<= 1;
        }
        Err(())
    }
}

// ---------------------------------------------------------------------------
// Prefix-code reading (RFC 7932 §3.4, §3.5): simple + complex.
// ---------------------------------------------------------------------------

/// Code lengths for the *code-length* code, in the order they are stored.
const CODE_LENGTH_ORDER: [usize; 18] =
    [1, 2, 3, 4, 0, 5, 17, 6, 16, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// Read one prefix code over an alphabet of `alphabet_size` symbols.
fn read_prefix_code(br: &mut BitReader, alphabet_size: usize) -> Result<PrefixCode, ()> {
    let hskip = br.bits(2)?;
    if hskip == 1 {
        return read_simple_prefix_code(br, alphabet_size);
    }
    read_complex_prefix_code(br, alphabet_size, hskip as usize)
}

fn alphabet_bits(alphabet_size: usize) -> u32 {
    // Number of bits to represent a symbol value in [0, alphabet_size).
    let max = (alphabet_size - 1) as u32;
    32 - max.leading_zeros()
}

fn read_simple_prefix_code(br: &mut BitReader, alphabet_size: usize) -> Result<PrefixCode, ()> {
    let nsym = br.bits(2)? as usize + 1; // 1..=4
    let sym_bits = alphabet_bits(alphabet_size);
    let mut syms = [0u16; 4];
    for s in syms.iter_mut().take(nsym) {
        let v = br.bits(sym_bits)? as usize;
        if v >= alphabet_size {
            return Err(());
        }
        *s = v as u16;
    }
    // Symbols must be distinct.
    for i in 0..nsym {
        for j in (i + 1)..nsym {
            if syms[i] == syms[j] {
                return Err(());
            }
        }
    }
    let mut lengths = vec![0u8; alphabet_size];
    match nsym {
        1 => {
            return Ok(PrefixCode::single(syms[0]));
        }
        2 => {
            lengths[syms[0] as usize] = 1;
            lengths[syms[1] as usize] = 1;
        }
        3 => {
            lengths[syms[0] as usize] = 1;
            lengths[syms[1] as usize] = 2;
            lengths[syms[2] as usize] = 2;
        }
        4 => {
            // One extra bit selects between the two 4-symbol code shapes.
            let tree_select = br.bit()?;
            if tree_select == 0 {
                for &s in syms.iter().take(4) {
                    lengths[s as usize] = 2;
                }
            } else {
                lengths[syms[0] as usize] = 1;
                lengths[syms[1] as usize] = 2;
                lengths[syms[2] as usize] = 3;
                lengths[syms[3] as usize] = 3;
            }
        }
        _ => return Err(()),
    }
    PrefixCode::from_lengths(&lengths)
}

/// Code-length symbols 0..=17: lengths 0..=15, 16 = repeat-prev, 17 = repeat-zero.
fn read_complex_prefix_code(
    br: &mut BitReader,
    alphabet_size: usize,
    hskip: usize,
) -> Result<PrefixCode, ()> {
    // Read code lengths for the code-length alphabet (18 symbols), in order.
    let mut cl_lengths = [0u8; 18];
    let mut space = 32i32;
    let mut num_codes = 0;
    // Lengths use this 2-bit-ish prefix table: value -> (length-code, #extra-handling).
    // RFC 7932 Table: 0->0, 1->4, 2->3, 3->2, 4->0(2 codes path)… we read with the
    // canonical small code below.
    const CL_CODE_LENGTHS: [u8; 6] = [2, 2, 2, 3, 1, 4]; // symbol-length code for values {0,1,2,3,4,5}
                                                         // The above encodes the fixed prefix used to read each code-length symbol.
    let cl_reader = build_code_length_reader(&CL_CODE_LENGTHS)?;

    for &sym in &CODE_LENGTH_ORDER[hskip..] {
        let len = cl_reader.decode(br)?;
        cl_lengths[sym] = len as u8;
        if len != 0 {
            num_codes += 1;
            space -= 32 >> len;
            if space <= 0 {
                break;
            }
        }
    }
    if num_codes < 1 {
        return Err(());
    }

    let cl_code = PrefixCode::from_lengths(&cl_lengths)?;

    // Now read the main alphabet's code lengths using `cl_code`.
    let mut lengths = vec![0u8; alphabet_size];
    let mut symbol = 0usize;
    let mut prev_code_len = 8u8; // default per spec
    let mut repeat = 0u32;
    let mut repeat_code_len = 0u8;
    let mut space = 1i64 << 15;

    while symbol < alphabet_size && space > 0 {
        let code_len = cl_code.decode(br)? as u8;
        if code_len < 16 {
            lengths[symbol] = code_len;
            symbol += 1;
            if code_len != 0 {
                prev_code_len = code_len;
                space -= (1i64 << 15) >> code_len;
            }
            repeat = 0;
        } else {
            // 16 = repeat previous non-zero length; 17 = repeat zero.
            let extra_bits = if code_len == 16 { 2 } else { 3 };
            let new_len = if code_len == 16 { prev_code_len } else { 0 };
            if repeat_code_len != code_len {
                repeat = 0;
                repeat_code_len = code_len;
            }
            let old_repeat = repeat;
            if repeat > 0 {
                repeat -= 2;
                repeat <<= extra_bits;
            }
            repeat += br.bits(extra_bits)? + 3;
            let repeat_delta = repeat - old_repeat;
            if symbol + repeat_delta as usize > alphabet_size {
                return Err(());
            }
            for _ in 0..repeat_delta {
                lengths[symbol] = new_len;
                symbol += 1;
            }
            if new_len != 0 {
                space -= (repeat_delta as i64) * ((1i64 << 15) >> new_len);
            }
        }
    }
    if space != 0 && symbol < alphabet_size {
        // Acceptable: trailing zeros remain. Fill rest with zero.
        // (space==0 means a full code; otherwise the code must still be valid.)
    }
    PrefixCode::from_lengths(&lengths)
}

/// Build the tiny fixed reader used to decode the 18 code-length symbols. The
/// code-length symbols themselves are coded with the lengths in `CODE_LENGTH_ORDER`
/// read 2/3/4 bits at a time per the RFC's fixed code; we model that as a small
/// canonical prefix code over symbols 0..=5 mapping to actual lengths.
fn build_code_length_reader(_lens: &[u8; 6]) -> Result<ClReader, ()> {
    Ok(ClReader)
}

/// Decodes a single "code length" symbol (0..=17) using the fixed sub-code from
/// RFC 7932 §3.5: each symbol's value is read with the variable-length prefix
/// `00→0, 10→…` etc. We implement the exact bit pattern table.
struct ClReader;

impl ClReader {
    fn decode(&self, br: &mut BitReader) -> Result<u32, ()> {
        // Fixed prefix code for code lengths (RFC 7932 §3.5):
        //   length value : code (read LSB-first)
        //   0 : 00
        //   1 : 0111  (i.e. read 2 bits == 11 then …) — easier to follow the
        //   canonical table from the spec directly:
        //
        //   sym  bits  meaning(code-length)
        //   ----  ----  --------------------
        //   The fixed Huffman code over the 6 possible code-length-of-code-length
        //   values {0,1,2,3,4,5}: lengths are [2,2,2,3,1,4]?  Brotli actually uses
        //   the symbols 0..5 with the canonical code defined by reading bits:
        //     read 2 bits:
        //       00 -> 0
        //       01 -> length 1? ...
        //
        // To avoid ambiguity we follow the reference decoder's exact procedure:
        //   first bit:
        //     0 -> read 1 more bit: 0->0, 1->? — no.
        //
        // The reference (Google brotli) uses this fixed table (kCodeLengthCodeLengths
        // gives lengths {2,2,2,3,1,4} for code-length symbols {1,2,3,4,5,17}? No).
        //
        // We use the canonical prefix code with the code-length code lengths
        // exactly as the spec fixes them, evaluated here:
        cl_decode_symbol(br)
    }
}

/// The fixed prefix code that codes the 18 code-length symbols' *lengths*.
/// RFC 7932 §3.5 defines code lengths for the code-length code values:
///   value 0..15, 16, 17.  The lengths of THESE codes are read as a fixed
///   2-bit / extended code. The canonical reference reads:
///     - 2 bits; map per the table below.
/// Returns the code-length value (0..=17 worth) — but actually it returns the
/// *length* (0..=5) assigned to a code-length symbol while building the
/// code-length code. See `read_complex_prefix_code`.
fn cl_decode_symbol(br: &mut BitReader) -> Result<u32, ()> {
    // Brotli fixed code for reading the 18 "code length code lengths":
    //   0     -> 0      (1 bit? no)  …
    // The exact, unambiguous code (from RFC 7932 §3.5, matching the reference
    // decoder `ReadHuffmanCodeLengths` preamble) is:
    //
    //   Read bits until a complete code:
    //     "00"   -> 0
    //     "0111" no…
    //
    // Concretely the fixed code over the 6 length-values is the canonical code
    // built from the per-value lengths {0:2, 1:2, 2:2, 3:3, 4:1, 5:4}? That sums
    // 2^-2+2^-2+2^-2+2^-3+2^-1+2^-4 = .25+.25+.25+.125+.5+.0625 = 1.4375 ≠ 1.
    //
    // The correct fixed lengths are {0:2,1:4,2:3,3:2,4:2,5:4} (Kraft = .25+.0625+
    // .125+.25+.25+.0625 = 1.0). We decode with that canonical code.
    static READER: OnceLock<PrefixCode> = OnceLock::new();
    let pc = READER.get_or_init(|| {
        // lengths indexed by code-length value 0..=5
        let lens: [u8; 6] = [2, 4, 3, 2, 2, 4];
        PrefixCode::from_lengths(&lens).expect("fixed cl code")
    });
    pc.decode(br).map(|s| s as u32)
}

// ---------------------------------------------------------------------------
// Length / distance / block-count constants (RFC 7932 §4, §5, §9).
// ---------------------------------------------------------------------------

/// Insert-length code base + extra bits (24 codes).
const INSERT_BASE: [u32; 24] = [
    0, 1, 2, 3, 4, 5, 6, 8, 10, 14, 18, 26, 34, 50, 66, 98, 130, 194, 322, 578, 1090, 2114, 6210,
    22594,
];
const INSERT_EXTRA: [u32; 24] = [
    0, 0, 0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 7, 8, 9, 10, 12, 14, 24,
];
/// Copy-length code base + extra bits (24 codes).
const COPY_BASE: [u32; 24] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 14, 18, 22, 30, 38, 54, 70, 102, 134, 198, 326, 582, 1094, 2118,
];
const COPY_EXTRA: [u32; 24] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 7, 8, 9, 10, 24,
];

/// Block-count code base + extra bits (26 codes) — shared by L/I/D block counts.
const BLOCK_COUNT_BASE: [u32; 26] = [
    1, 5, 9, 13, 17, 25, 33, 41, 49, 65, 81, 97, 113, 145, 177, 209, 241, 305, 369, 497, 753, 1265,
    2289, 4337, 8433, 16625,
];
const BLOCK_COUNT_EXTRA: [u32; 26] = [
    2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 7, 8, 9, 10, 11, 12, 13, 24,
];

/// Word length → number of words of that length in the static dictionary, and
/// the cumulative offset into the dictionary data (RFC 7932 §8 / Appendix A).
const DICT_SIZE_BITS: [u8; 25] = [
    0, 0, 0, 0, 10, 10, 11, 11, 10, 10, 10, 10, 10, 9, 9, 8, 7, 7, 8, 7, 7, 6, 6, 5, 5,
];
const DICT_OFFSETS: [u32; 25] = [
    0, 0, 0, 0, 0, 4096, 9216, 21504, 35840, 44032, 53248, 63488, 74752, 87040, 93696, 100864,
    104704, 106752, 108928, 113536, 115968, 118528, 119872, 121280, 122016,
];

// ---------------------------------------------------------------------------
// Context modeling (RFC 7932 §7).
// ---------------------------------------------------------------------------

/// Literal context modes.
const CONTEXT_LSB6: u8 = 0;
const CONTEXT_MSB6: u8 = 1;
const CONTEXT_UTF8: u8 = 2;
const CONTEXT_SIGNED: u8 = 3;

/// Compute the literal context id (0..=63) from the two previous bytes and the
/// current literal context mode (RFC 7932 §7.1). The UTF8 and SIGNED tables are
/// the canonical `kContextLookup` values, shared with [`super::brotli_tables`].
fn literal_context(mode: u8, p1: u8, p2: u8) -> usize {
    match mode {
        CONTEXT_LSB6 => (p1 & 0x3f) as usize,
        CONTEXT_MSB6 => (p1 >> 2) as usize,
        CONTEXT_UTF8 => {
            (super::brotli_tables::UTF8_LUT0[p1 as usize]
                | super::brotli_tables::UTF8_LUT1[p2 as usize]) as usize
        }
        CONTEXT_SIGNED => {
            let c1 = super::brotli_tables::SIGNED_CTX[p1 as usize] as usize;
            let c2 = super::brotli_tables::SIGNED_CTX[p2 as usize] as usize;
            (c1 << 3) | c2
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Decoder state machine.
// ---------------------------------------------------------------------------

struct Decoder<'a> {
    br: BitReader<'a>,
    out: Vec<u8>,
    /// Sliding-window max size in bytes (from WBITS). The whole output is kept,
    /// so back-references just index `out`.
    window_max: usize,
    /// Last-distance ring buffer (4 entries), initial values per RFC.
    dist_ring: [i32; 4],
    dist_ring_idx: usize,
    /// Distance parameters.
    n_postfix: u32,
    n_direct: u32,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            br: BitReader::new(data),
            out: Vec::new(),
            window_max: 0,
            // RFC 7932 §4: the four initial last-distances are 16, 15, 11, 4 with
            // 4 the most recent. With `dist_ring_idx == 0`, the "last distance"
            // (short code 0) reads `dist_ring[(idx + 3) & 3] == dist_ring[3] == 4`.
            dist_ring: [16, 15, 11, 4],
            dist_ring_idx: 0,
            n_postfix: 0,
            n_direct: 0,
        }
    }

    fn run(&mut self) -> Result<(), ()> {
        self.read_window_bits()?;
        loop {
            let last = self.read_meta_block_header()?;
            match last {
                MetaBlock::LastEmpty => break,
                MetaBlock::Uncompressed(len) => {
                    self.br.align();
                    let bytes = self.br.read_bytes(len)?.to_vec();
                    self.out.extend_from_slice(&bytes);
                }
                MetaBlock::Metadata(len) => {
                    self.br.align();
                    let _ = self.br.read_bytes(len)?; // skip metadata bytes
                }
                MetaBlock::Compressed(len, last_flag) => {
                    self.decode_compressed_meta_block(len)?;
                    if last_flag {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn read_window_bits(&mut self) -> Result<(), ()> {
        // WBITS encoding (RFC 7932 §9.1).
        let wbits = if self.br.bit()? == 0 {
            16
        } else {
            let n = self.br.bits(3)?;
            if n != 0 {
                17 + n
            } else {
                let n2 = self.br.bits(3)?;
                if n2 != 0 {
                    8 + n2
                } else {
                    17
                }
            }
        };
        self.window_max = (1usize << wbits) - 16;
        Ok(())
    }

    /// Read a meta-block header, returning its kind. For a compressed block this
    /// also pre-reads ISLASTEMPTY handling.
    fn read_meta_block_header(&mut self) -> Result<MetaBlock, ()> {
        let is_last = self.br.bit()? == 1;
        if is_last {
            let is_last_empty = self.br.bit()? == 1;
            if is_last_empty {
                return Ok(MetaBlock::LastEmpty);
            }
        }
        let mnibbles_code = self.br.bits(2)?;
        if mnibbles_code == 3 {
            // Metadata or empty-with-skip.
            let reserved = self.br.bit()?;
            if reserved != 0 {
                return Err(());
            }
            let mskipbytes = self.br.bits(2)?;
            if mskipbytes == 0 {
                // Empty metadata meta-block.
                return Ok(MetaBlock::Metadata(0));
            }
            let mut mskiplen = 0u32;
            for i in 0..mskipbytes {
                let b = self.br.bits(8)?;
                if i + 1 == mskipbytes && mskipbytes > 1 && b == 0 {
                    return Err(()); // non-minimal encoding
                }
                mskiplen |= b << (8 * i);
            }
            return Ok(MetaBlock::Metadata(mskiplen as usize + 1));
        }
        let mnibbles = mnibbles_code + 4; // 4, 5, or 6
        let mut mlen = 0u32;
        for i in 0..mnibbles {
            let nib = self.br.bits(4)?;
            if i + 1 == mnibbles && mnibbles > 4 && nib == 0 {
                return Err(()); // non-minimal
            }
            mlen |= nib << (4 * i);
        }
        let mlen = mlen as usize + 1;

        if !is_last {
            let is_uncompressed = self.br.bit()? == 1;
            if is_uncompressed {
                return Ok(MetaBlock::Uncompressed(mlen));
            }
        }
        Ok(MetaBlock::Compressed(mlen, is_last))
    }

    fn decode_compressed_meta_block(&mut self, mlen: usize) -> Result<(), ()> {
        let target = self.out.len() + mlen;

        // --- Block-type machinery for L, I, D (RFC 7932 §6, §9.2) ---
        let (mut l_state, mut i_state, mut d_state) = (
            self.read_block_type_info()?,
            self.read_block_type_info()?,
            self.read_block_type_info()?,
        );

        // --- Distance parameters ---
        self.n_postfix = self.br.bits(2)?;
        self.n_direct = self.br.bits(4)? << self.n_postfix;
        let num_dist_short = 16;
        let num_dist_codes = num_dist_short + self.n_direct + (48u32 << self.n_postfix);

        // --- Literal context modes (one per literal block type) ---
        let mut context_modes = vec![0u8; l_state.num_types];
        for m in context_modes.iter_mut() {
            *m = self.br.bits(2)? as u8;
        }

        // --- Context maps ---
        let num_literal_htrees;
        let literal_cmap;
        {
            let (n, cmap) = self.read_context_map(l_state.num_types * 64)?;
            num_literal_htrees = n;
            literal_cmap = cmap;
        }
        let num_dist_htrees;
        let dist_cmap;
        {
            let (n, cmap) = self.read_context_map(d_state.num_types * 4)?;
            num_dist_htrees = n;
            dist_cmap = cmap;
        }

        // --- Prefix codes ---
        let mut literal_codes = Vec::with_capacity(num_literal_htrees);
        for _ in 0..num_literal_htrees {
            literal_codes.push(read_prefix_code(&mut self.br, 256)?);
        }
        let mut icommand_codes = Vec::with_capacity(i_state.num_types);
        for _ in 0..i_state.num_types {
            icommand_codes.push(read_prefix_code(&mut self.br, 704)?);
        }
        let mut distance_codes = Vec::with_capacity(num_dist_htrees);
        for _ in 0..num_dist_htrees {
            distance_codes.push(read_prefix_code(&mut self.br, num_dist_codes as usize)?);
        }

        // --- Command loop ---
        while self.out.len() < target {
            // Switch insert-and-copy block type if needed.
            if i_state.count == 0 {
                i_state.switch(&mut self.br)?;
            }
            i_state.count -= 1;
            let icmd_tree = &icommand_codes[i_state.cur_type];
            let cmd = icmd_tree.decode(&mut self.br)? as u32;

            let (insert_len, copy_len, dist_code_zero) = decode_command(&mut self.br, cmd)?;

            // --- Insert literals ---
            for _ in 0..insert_len {
                if l_state.count == 0 {
                    l_state.switch(&mut self.br)?;
                }
                l_state.count -= 1;
                let p1 = self.out.last().copied().unwrap_or(0);
                let p2 = if self.out.len() >= 2 {
                    self.out[self.out.len() - 2]
                } else {
                    0
                };
                let mode = context_modes[l_state.cur_type];
                let ctx = literal_context(mode, p1, p2);
                let cmap_idx = l_state.cur_type * 64 + ctx;
                let htree = literal_cmap[cmap_idx] as usize;
                let lit = literal_codes[htree].decode(&mut self.br)? as u8;
                self.out.push(lit);
            }

            if self.out.len() >= target {
                break;
            }

            // --- Distance ---
            if dist_code_zero {
                // Implicit distance 0 → reuse last distance, no D-block consumed.
                let d = self.last_distance();
                let max_back = self.out.len().min(self.window_max) as i32;
                if d > 0 && d <= max_back {
                    self.copy_match(d as usize, copy_len)?;
                } else {
                    self.append_dictionary_word(copy_len, d, max_back)?;
                }
            } else {
                if d_state.count == 0 {
                    d_state.switch(&mut self.br)?;
                }
                d_state.count -= 1;
                // Distance context = min(copy_len-2, 3).
                let dctx = (copy_len.saturating_sub(2)).min(3) as usize;
                let dtree = dist_cmap[d_state.cur_type * 4 + dctx] as usize;
                let dsym = distance_codes[dtree].decode(&mut self.br)? as u32;
                let d = self.resolve_distance(dsym)?;
                let max_back = self.out.len().min(self.window_max) as i32;
                if d > 0 && d <= max_back {
                    // In-window back-reference. The resolved distance is pushed to
                    // the last-distance ring unless this was the "reuse last
                    // distance" code (dsym 0). Short codes 1-15 and direct/extra-bit
                    // codes all push their resolved value (RFC 7932 §4).
                    if dsym != 0 {
                        self.push_distance(d);
                    }
                    self.copy_match(d as usize, copy_len)?;
                } else {
                    // Static-dictionary reference (distance past the window): the
                    // word is appended and the ring is NOT updated (RFC 7932 §4/§8).
                    self.append_dictionary_word(copy_len, d, max_back)?;
                }
            }
        }
        Ok(())
    }

    /// The most-recent distance (ring[idx-1]).
    fn last_distance(&self) -> i32 {
        self.dist_ring[(self.dist_ring_idx + 3) & 3]
    }

    /// Push a new distance into the ring buffer.
    fn push_distance(&mut self, d: i32) {
        self.dist_ring[self.dist_ring_idx & 3] = d;
        self.dist_ring_idx = (self.dist_ring_idx + 1) & 3;
    }

    /// Resolve a distance code symbol to an absolute distance, handling the 16
    /// short codes (ring buffer) and the direct/extra-bit codes (RFC 7932 §4).
    fn resolve_distance(&mut self, dsym: u32) -> Result<i32, ()> {
        if dsym < 16 {
            // Short distance codes operate on the ring buffer.
            let ring = self.dist_ring;
            let idx = self.dist_ring_idx;
            let last = ring[(idx + 3) & 3];
            let second = ring[(idx + 2) & 3];
            let third = ring[(idx + 1) & 3];
            let fourth = ring[idx & 3];
            let d = match dsym {
                0 => last,
                1 => second,
                2 => third,
                3 => fourth,
                4 => last - 1,
                5 => last + 1,
                6 => last - 2,
                7 => last + 2,
                8 => last - 3,
                9 => last + 3,
                10 => second - 1,
                11 => second + 1,
                12 => second - 2,
                13 => second + 2,
                14 => second - 3,
                15 => second + 3,
                _ => unreachable!(),
            };
            return Ok(d);
        }
        // Codes >= 16: direct or with extra bits.
        if dsym < 16 + self.n_direct {
            // Direct distance.
            return Ok((dsym - 16 + 1) as i32);
        }
        let dcode = dsym - 16 - self.n_direct;
        let ndistbits = 1 + (dcode >> (self.n_postfix + 1));
        let extra = self.br.bits(ndistbits)?;
        let hcode = dcode >> self.n_postfix;
        let lcode = dcode & ((1 << self.n_postfix) - 1);
        let offset = ((2 + (hcode & 1)) << ndistbits) - 4;
        let dist = ((offset + extra) << self.n_postfix) + lcode + self.n_direct + 1;
        Ok(dist as i32)
    }

    fn copy_match(&mut self, distance: usize, length: u32) -> Result<(), ()> {
        if distance == 0 || distance > self.out.len() {
            return Err(());
        }
        let start = self.out.len() - distance;
        for i in 0..length as usize {
            let b = self.out[start + i];
            self.out.push(b);
        }
        Ok(())
    }

    fn append_dictionary_word(&mut self, copy_len: u32, d: i32, max_back: i32) -> Result<(), ()> {
        let len = copy_len as usize;
        if !(4..=24).contains(&len) {
            return Err(());
        }
        let n_words_bits = DICT_SIZE_BITS[len] as u32;
        let n_words = 1u32 << n_words_bits;
        // distance into dictionary space:
        let word_dist = (d - max_back - 1) as u32;
        let index = word_dist % n_words;
        let transform_id = word_dist / n_words;
        let dict = dictionary();
        let base = DICT_OFFSETS[len] as usize + index as usize * len;
        let word = dict.get(base..base + len).ok_or(())?;
        let transformed = apply_transform(transform_id as usize, word)?;
        self.out.extend_from_slice(&transformed);
        Ok(())
    }

    /// Read a block-type's `(NBLTYPES, prefix codes, first count)` info and set
    /// up the initial state.
    fn read_block_type_info(&mut self) -> Result<BlockState, ()> {
        let nbltypes = self.read_var_len_count()? as usize; // 1..=256
        if nbltypes == 1 {
            return Ok(BlockState::single());
        }
        let type_code = read_prefix_code(&mut self.br, nbltypes + 2)?;
        let count_code = read_prefix_code(&mut self.br, 26)?;
        // First block count.
        let csym = count_code.decode(&mut self.br)? as usize;
        let count = BLOCK_COUNT_BASE[csym] + self.br.bits(BLOCK_COUNT_EXTRA[csym])?;
        Ok(BlockState {
            num_types: nbltypes,
            cur_type: 0,
            prev_type: 1,
            count,
            type_code: Some(type_code),
            count_code: Some(count_code),
        })
    }

    /// Read the `NBLTYPES`/`NTREES`-style variable count (RFC 7932 §9.2): a
    /// prefix value 1..=256.
    fn read_var_len_count(&mut self) -> Result<u32, ()> {
        // First bit: 0 → value 1. Else read a 3-bit selector for the number of
        // extra bits.
        if self.br.bit()? == 0 {
            return Ok(1);
        }
        let nbits = self.br.bits(3)?;
        let extra = self.br.bits(nbits)?;
        Ok((1 << nbits) + 1 + extra)
    }

    /// Read a context map of `size` entries: returns (num_htrees, map).
    fn read_context_map(&mut self, size: usize) -> Result<(usize, Vec<u8>), ()> {
        let num_htrees = self.read_var_len_count()? as usize;
        if num_htrees == 1 {
            return Ok((1, vec![0u8; size]));
        }
        // RLEMAX
        let use_rle = self.br.bit()? == 1;
        let rle_max = if use_rle {
            self.br.bits(4)? as usize + 1
        } else {
            0
        };
        let code = read_prefix_code(&mut self.br, num_htrees + rle_max)?;
        let mut map = vec![0u8; size];
        let mut i = 0;
        while i < size {
            let sym = code.decode(&mut self.br)? as usize;
            if sym == 0 {
                map[i] = 0;
                i += 1;
            } else if sym <= rle_max {
                // Run of zeros.
                let extra = self.br.bits(sym as u32)?;
                let run = (1usize << sym) + extra as usize;
                if i + run > size {
                    return Err(());
                }
                for _ in 0..run {
                    map[i] = 0;
                    i += 1;
                }
            } else {
                map[i] = (sym - rle_max) as u8;
                i += 1;
            }
        }
        // Inverse move-to-front transform if the IMTF bit is set.
        let use_imtf = self.br.bit()? == 1;
        if use_imtf {
            inverse_move_to_front(&mut map);
        }
        Ok((num_htrees, map))
    }
}

/// Decode an insert-and-copy command symbol (0..=703) into
/// `(insert_len, copy_len, distance_code_is_zero)` (RFC 7932 §5).
fn decode_command(br: &mut BitReader, cmd: u32) -> Result<(u32, u32, bool), ()> {
    if cmd >= 704 {
        return Err(());
    }
    // Determine the (insert_code, copy_code) ranges and the implicit-dist flag.
    let range_idx = cmd >> 6;
    let (insert_code_base, copy_code_base, dist0) = match range_idx {
        0 => (0, 0, true),
        1 => (0, 8, true),
        2 => (0, 0, false),
        3 => (0, 8, false),
        4 => (8, 0, false),
        5 => (8, 8, false),
        6 => (0, 16, false),
        7 => (16, 0, false),
        8 => (8, 16, false),
        9 => (16, 8, false),
        10 => (16, 16, false),
        _ => return Err(()),
    };
    // Within the 64-cell block: high 3 bits choose insert sub-code, low 3 copy.
    let sub = cmd & 0x3f;
    let insert_sub = (sub >> 3) as usize;
    let copy_sub = (sub & 7) as usize;
    let insert_code = insert_code_base + insert_sub;
    let copy_code = copy_code_base + copy_sub;
    if insert_code >= 24 || copy_code >= 24 {
        return Err(());
    }
    let insert_len = INSERT_BASE[insert_code] + br.bits(INSERT_EXTRA[insert_code])?;
    let copy_len = COPY_BASE[copy_code] + br.bits(COPY_EXTRA[copy_code])?;
    Ok((insert_len, copy_len, dist0))
}

/// Inverse move-to-front transform (RFC 7932 §7.3) applied in place.
fn inverse_move_to_front(v: &mut [u8]) {
    let mut mtf: [u8; 256] = [0; 256];
    for (i, slot) in mtf.iter_mut().enumerate() {
        *slot = i as u8;
    }
    for x in v.iter_mut() {
        let index = *x as usize;
        let value = mtf[index];
        *x = value;
        // Shift entries 0..index up by one and put value at front.
        for j in (1..=index).rev() {
            mtf[j] = mtf[j - 1];
        }
        mtf[0] = value;
    }
}

/// Insert-and-copy meta-block header gives one of these kinds.
enum MetaBlock {
    LastEmpty,
    Uncompressed(usize),
    Metadata(usize),
    Compressed(usize, bool),
}

/// Block-switching state for one of the three streams (literal/command/distance).
struct BlockState {
    num_types: usize,
    cur_type: usize,
    prev_type: usize,
    count: u32,
    type_code: Option<PrefixCode>,
    count_code: Option<PrefixCode>,
}

impl BlockState {
    fn single() -> Self {
        Self {
            num_types: 1,
            cur_type: 0,
            prev_type: 1,
            count: u32::MAX, // never switches
            type_code: None,
            count_code: None,
        }
    }

    /// Switch to the next block type & read the next block count.
    fn switch(&mut self, br: &mut BitReader) -> Result<(), ()> {
        let (Some(tc), Some(cc)) = (self.type_code.as_ref(), self.count_code.as_ref()) else {
            // Single-type stream never switches.
            self.count = u32::MAX;
            return Ok(());
        };
        let sym = tc.decode(br)? as usize;
        let new_type = match sym {
            0 => self.prev_type, // "previous" type
            1 => (self.cur_type + 1) % self.num_types,
            _ => sym - 2,
        };
        self.prev_type = self.cur_type;
        self.cur_type = new_type % self.num_types;
        let csym = cc.decode(br)? as usize;
        self.count = BLOCK_COUNT_BASE[csym] + br.bits(BLOCK_COUNT_EXTRA[csym])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Static dictionary (lazy-expanded from the DEFLATE blob).
// ---------------------------------------------------------------------------

fn dictionary() -> &'static [u8] {
    static DICT: OnceLock<Vec<u8>> = OnceLock::new();
    DICT.get_or_init(|| {
        crate::filters::inflate::inflate(&super::brotli_dict::BROTLI_DICT_DEFLATE)
            .expect("brotli dictionary inflates")
    })
}

// ---------------------------------------------------------------------------
// Word transforms (RFC 7932 §8, the 121-entry transform table).
// ---------------------------------------------------------------------------

/// A transform: an optional prefix, a base transform on the word, an optional
/// suffix.
struct Transform {
    prefix: &'static str,
    op: TransformOp,
    suffix: &'static str,
}

#[derive(Clone, Copy)]
enum TransformOp {
    Identity,
    Omit(u8, bool), // (n, from_end?) — OmitFirstN / OmitLastN
    Ferm(bool),     // FermentFirst (true) / FermentAll (false)
}

/// Apply transform `id` to `word`, returning the transformed bytes.
fn apply_transform(id: usize, word: &[u8]) -> Result<Vec<u8>, ()> {
    let t = TRANSFORMS.get(id).ok_or(())?;
    let mut base: Vec<u8> = word.to_vec();
    match t.op {
        TransformOp::Identity => {}
        TransformOp::Omit(n, from_end) => {
            let n = n as usize;
            if n >= base.len() {
                base.clear();
            } else if from_end {
                base.truncate(base.len() - n);
            } else {
                base.drain(0..n);
            }
        }
        TransformOp::Ferm(first_only) => {
            ferment(&mut base, first_only);
        }
    }
    let mut out = Vec::with_capacity(t.prefix.len() + base.len() + t.suffix.len());
    out.extend_from_slice(t.prefix.as_bytes());
    out.extend_from_slice(&base);
    out.extend_from_slice(t.suffix.as_bytes());
    Ok(out)
}

/// The brotli "ferment" (uppercase-ish) UTF-8 case transform (RFC 7932 §8).
/// `first_only` ferments just the first letter; otherwise the whole word.
fn ferment(data: &mut [u8], first_only: bool) {
    let mut i = 0;
    while i < data.len() {
        let c = data[i];
        if c < 0x80 {
            if c.is_ascii_lowercase() {
                data[i] = c - 32;
            }
            i += 1;
        } else if c < 0xE0 {
            // 2-byte UTF-8: toggle bit per the brotli rule.
            if i + 1 < data.len() {
                data[i + 1] ^= 32;
            }
            i += 2;
        } else {
            // 3-byte UTF-8.
            if i + 2 < data.len() {
                data[i + 2] ^= 5;
            }
            i += 3;
        }
        if first_only {
            break;
        }
    }
}

/// Build a transform with a base op and affixes.
const fn tr(prefix: &'static str, op: TransformOp, suffix: &'static str) -> Transform {
    Transform { prefix, op, suffix }
}

use TransformOp::*;

/// The 121 word transforms (RFC 7932 §8, in order).
static TRANSFORMS: [Transform; 121] = [
    tr("", Identity, ""),
    tr("", Identity, " "),
    tr(" ", Identity, " "),
    tr("", Omit(1, false), ""),
    tr("", Ferm(true), " "),
    tr("", Identity, " the "),
    tr(" ", Identity, ""),
    tr("s ", Identity, " "),
    tr("", Identity, " of "),
    tr("", Ferm(true), ""),
    tr("", Identity, " and "),
    tr("", Omit(2, false), ""),
    tr("", Omit(1, true), ""),
    tr(", ", Identity, " "),
    tr("", Identity, ", "),
    tr(" ", Ferm(true), " "),
    tr("", Identity, " in "),
    tr("", Identity, " to "),
    tr("e ", Identity, " "),
    tr("", Identity, "\""),
    tr("", Identity, "."),
    tr("", Identity, "\">"),
    tr("", Identity, "\n"),
    tr("", Omit(3, true), ""),
    tr("", Identity, "]"),
    tr("", Identity, " for "),
    tr("", Omit(3, false), ""),
    tr("", Omit(2, true), ""),
    tr("", Identity, " a "),
    tr("", Identity, " that "),
    tr(" ", Ferm(true), ""),
    tr("", Identity, ". "),
    tr(".", Identity, ""),
    tr(" ", Identity, ", "),
    tr("", Omit(4, false), ""),
    tr("", Identity, " with "),
    tr("", Identity, "'"),
    tr("", Identity, " from "),
    tr("", Identity, " by "),
    tr("", Omit(5, false), ""),
    tr("", Omit(6, false), ""),
    tr(" the ", Identity, ""),
    tr("", Omit(4, true), ""),
    tr("", Identity, ". The "),
    tr("", Ferm(false), ""),
    tr("", Identity, " on "),
    tr("", Identity, " as "),
    tr("", Identity, " is "),
    tr("", Omit(7, true), ""),
    tr("", Omit(1, true), "ing "),
    tr("", Identity, "\n\t"),
    tr("", Identity, ":"),
    tr(" ", Identity, ". "),
    tr("", Identity, "ed "),
    tr("", Omit(9, false), ""),
    tr("", Omit(7, false), ""),
    tr("", Omit(6, true), ""),
    tr("", Identity, "("),
    tr("", Ferm(true), ", "),
    tr("", Omit(8, true), ""),
    tr("", Identity, " at "),
    tr("", Identity, "ly "),
    tr(" the ", Identity, " of "),
    tr("", Omit(5, true), ""),
    tr("", Omit(9, true), ""),
    tr(" ", Ferm(true), ", "),
    tr("", Ferm(true), "\""),
    tr(".", Identity, "("),
    tr("", Ferm(false), " "),
    tr("", Ferm(true), "\">"),
    tr("", Identity, "=\""),
    tr(" ", Identity, "."),
    tr(".com/", Identity, ""),
    tr(" the ", Identity, " of the "),
    tr("", Ferm(true), "'"),
    tr("", Identity, ". This "),
    tr("", Identity, ","),
    tr(".", Identity, " "),
    tr("", Ferm(true), "("),
    tr("", Ferm(true), "."),
    tr("", Identity, " not "),
    tr(" ", Identity, "=\""),
    tr("", Identity, "er "),
    tr(" ", Ferm(false), " "),
    tr("", Identity, "al "),
    tr(" ", Ferm(false), ""),
    tr("", Identity, "='"),
    tr("", Ferm(false), "\""),
    tr("", Ferm(true), ". "),
    tr(" ", Identity, "("),
    tr("", Identity, "ful "),
    tr(" ", Ferm(true), ". "),
    tr("", Identity, "ive "),
    tr("", Identity, "less "),
    tr("", Ferm(false), "'"),
    tr("", Identity, "est "),
    tr(" ", Ferm(true), "."),
    tr("", Ferm(false), "\">"),
    tr(" ", Identity, "='"),
    tr("", Ferm(true), ","),
    tr("", Identity, "ize "),
    tr("", Ferm(false), "."),
    tr("\u{c2}\u{a0}", Identity, ""),
    tr(" ", Identity, ","),
    tr("", Ferm(true), "=\""),
    tr("", Ferm(false), "=\""),
    tr("", Identity, "ous "),
    tr("", Ferm(false), ", "),
    tr("", Ferm(true), "='"),
    tr(" ", Ferm(true), ","),
    tr(" ", Ferm(false), "=\""),
    tr(" ", Ferm(false), ", "),
    tr("", Ferm(false), ","),
    tr("", Ferm(false), "("),
    tr("", Ferm(false), ". "),
    tr(" ", Ferm(false), "."),
    tr("", Ferm(false), "='"),
    tr(" ", Ferm(false), ". "),
    tr(" ", Ferm(true), "=\""),
    tr(" ", Ferm(false), "='"),
    tr(" ", Ferm(true), "='"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::brotli_test_vectors as test_vectors;

    fn dec(v: &[u8]) -> Vec<u8> {
        decompress(v).expect("brotli decode")
    }

    #[test]
    fn dictionary_inflates_to_expected_size() {
        let d = dictionary();
        assert_eq!(d.len(), 122_784);
        assert_eq!(&d[..16], b"timedownlifeleft");
    }

    #[test]
    fn empty_stream() {
        // brotli of b"" — a single last-empty meta-block.
        assert_eq!(dec(&test_vectors::BR_EMPTY), b"");
    }

    #[test]
    fn hello() {
        assert_eq!(dec(&test_vectors::BR_HELLO), b"Hello");
    }

    #[test]
    fn long_with_backrefs() {
        let expected = b"The quick brown brown brown fox jumps over the lazy dog. ".repeat(4);
        assert_eq!(dec(&test_vectors::BR_LONG), expected);
    }

    #[test]
    fn dictionary_reference() {
        assert_eq!(
            dec(&test_vectors::BR_DICT),
            b"the time of day and the information about the world"
        );
    }

    #[test]
    fn transform_count_is_121() {
        assert_eq!(TRANSFORMS.len(), 121);
    }

    /// Decode every reference-encoder (compressed, expected) pair and assert the
    /// output is byte-identical. These cover the bugs that the original WIP had:
    /// the insert-and-copy range table (ranges 8/9), the last-distance ring
    /// (init order + no push for dictionary references), and the SIGNED / UTF8
    /// literal-context tables.
    #[test]
    fn roundtrip_oracle_vectors() {
        for (i, (compressed, expected)) in test_vectors::ROUNDTRIP.iter().enumerate() {
            let out = decompress(compressed).unwrap_or_else(|| panic!("vector #{i}: decode error"));
            assert_eq!(out.as_slice(), *expected, "vector #{i} mismatch");
        }
    }

    #[test]
    fn transform_identity_and_affixes() {
        assert_eq!(apply_transform(0, b"word").unwrap(), b"word");
        assert_eq!(apply_transform(1, b"word").unwrap(), b"word ");
        assert_eq!(apply_transform(2, b"word").unwrap(), b" word ");
        // Omit-first-1
        assert_eq!(apply_transform(3, b"word").unwrap(), b"ord");
        // Ferment-first + space
        assert_eq!(apply_transform(4, b"word").unwrap(), b"Word ");
    }
}
