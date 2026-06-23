//! WebP encoder (lossless **VP8L**) + decoder (lossless **VP8L** *and* lossy
//! **VP8** keyframes) — pure std, zero dependency.
//!
//! Encodes RGBA losslessly (no spatial/colour transforms, single Huffman group,
//! literal pixels — valid VP8L every decoder accepts). Decodes the **complete**
//! VP8L lossless format — the four transforms (predictor, colour, subtract-green,
//! colour-indexing with pixel bundling), meta-Huffman (multiple Huffman groups
//! via an entropy image), the colour cache, and LZ77 backward references — so
//! any conformant `cwebp`/libwebp lossless file decodes correctly, plus lossy
//! VP8 keyframes (intra-coded I-frames). The RIFF/WebP container is read and
//! written here. Extended (`VP8X`) and animated WebP are not handled —
//! `decode_webp` returns `None` for them. This is the native WebP path replacing
//! a third-party image library.

// ── bit reader / writer (VP8L is LSB-first) ───────────────────────────────────

struct BitR<'a> {
    d: &'a [u8],
    pos: usize, // bit position
}
impl BitR<'_> {
    fn read(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for i in 0..n {
            let byte = *self.d.get(self.pos / 8).unwrap_or(&0);
            v |= (((byte >> (self.pos % 8)) & 1) as u32) << i;
            self.pos += 1;
        }
        v
    }
}

struct BitW {
    out: Vec<u8>,
    acc: u32,
    nb: u32,
}
impl BitW {
    fn new() -> BitW {
        BitW {
            out: Vec::new(),
            acc: 0,
            nb: 0,
        }
    }
    fn write(&mut self, val: u32, n: u32) {
        if n == 0 {
            return;
        }
        let mask = if n >= 32 { u32::MAX } else { (1u32 << n) - 1 };
        self.acc |= (val & mask) << self.nb;
        self.nb += n;
        while self.nb >= 8 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.nb -= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.nb > 0 {
            self.out.push((self.acc & 0xFF) as u8);
        }
        self.out
    }
}

// ── canonical Huffman (build code lengths, codes; decode table) ───────────────

/// Length-limited (≤15) canonical Huffman code lengths from symbol frequencies.
fn huffman_lengths(freq: &[u32], limit: u8) -> Vec<u8> {
    let n = freq.len();
    let mut lengths = vec![0u8; n];
    let used: Vec<usize> = (0..n).filter(|&i| freq[i] > 0).collect();
    if used.is_empty() {
        return lengths;
    }
    if used.len() == 1 {
        lengths[used[0]] = 1;
        return lengths;
    }
    // Package-merge would be ideal; a simple Huffman + clamp suffices for our
    // small alphabets, then we clamp to `limit` and keep the Kraft sum valid.
    #[derive(Clone)]
    struct Node {
        freq: u64,
        sym: i32,
        left: i32,
        right: i32,
    }
    let mut nodes: Vec<Node> = used
        .iter()
        .map(|&s| Node {
            freq: freq[s] as u64,
            sym: s as i32,
            left: -1,
            right: -1,
        })
        .collect();
    let mut heap: Vec<usize> = (0..nodes.len()).collect();
    let pop_min = |heap: &mut Vec<usize>, nodes: &[Node]| -> usize {
        let mut mi = 0;
        for i in 1..heap.len() {
            if nodes[heap[i]].freq < nodes[heap[mi]].freq {
                mi = i;
            }
        }
        heap.swap_remove(mi)
    };
    while heap.len() > 1 {
        let a = pop_min(&mut heap, &nodes);
        let b = pop_min(&mut heap, &nodes);
        let node = Node {
            freq: nodes[a].freq + nodes[b].freq,
            sym: -1,
            left: a as i32,
            right: b as i32,
        };
        nodes.push(node);
        heap.push(nodes.len() - 1);
    }
    fn assign(nodes: &[Node], i: usize, depth: u8, lengths: &mut [u8]) {
        if nodes[i].sym >= 0 {
            lengths[nodes[i].sym as usize] = depth.max(1);
        } else {
            assign(nodes, nodes[i].left as usize, depth + 1, lengths);
            assign(nodes, nodes[i].right as usize, depth + 1, lengths);
        }
    }
    assign(&nodes, heap[0], 0, &mut lengths);
    // Clamp overlong codes (rare for our sizes) to `limit`.
    for l in lengths.iter_mut() {
        if *l > limit {
            *l = limit;
        }
    }
    lengths
}

/// Canonical codes (LSB-first bit order, as VP8L reads them) from code lengths.
fn canonical_codes(lengths: &[u8]) -> Vec<u16> {
    let max_len = *lengths.iter().max().unwrap_or(&0);
    let mut bl_count = vec![0u16; max_len as usize + 1];
    for &l in lengths {
        if l > 0 {
            bl_count[l as usize] += 1;
        }
    }
    let mut next = vec![0u16; max_len as usize + 1];
    let mut code = 0u16;
    for bits in 1..=max_len as usize {
        code = (code + bl_count[bits - 1]) << 1;
        next[bits] = code;
    }
    let mut codes = vec![0u16; lengths.len()];
    for (i, &l) in lengths.iter().enumerate() {
        if l > 0 {
            // VP8L bit order is the reverse of the MSB-first canonical code.
            let c = next[l as usize];
            next[l as usize] += 1;
            codes[i] = reverse_bits(c, l);
        }
    }
    codes
}

fn reverse_bits(mut v: u16, n: u8) -> u16 {
    let mut r = 0u16;
    for _ in 0..n {
        r = (r << 1) | (v & 1);
        v >>= 1;
    }
    r
}

/// A decode table: walk bits LSB-first, matching `(len, code)`. When the tree
/// holds a single implicit symbol (VP8L "simple code", one symbol) decoding
/// consumes **zero** bits — `single` carries that symbol.
struct Huff {
    map: std::collections::HashMap<(u8, u16), u16>,
    max_len: u8,
    single: Option<u16>,
}
impl Huff {
    fn from_lengths(lengths: &[u8]) -> Huff {
        let codes = canonical_codes(lengths);
        let mut map = std::collections::HashMap::new();
        let mut max_len = 0u8;
        for (sym, (&l, &c)) in lengths.iter().zip(codes.iter()).enumerate() {
            if l > 0 {
                map.insert((l, c), sym as u16);
                max_len = max_len.max(l);
            }
        }
        Huff {
            map,
            max_len,
            single: None,
        }
    }
    /// A 0-bit tree carrying exactly one implicit symbol (simple code, 1 sym).
    fn single(sym: u16) -> Huff {
        Huff {
            map: std::collections::HashMap::new(),
            max_len: 0,
            single: Some(sym),
        }
    }
    fn decode(&self, r: &mut BitR) -> Option<u16> {
        if let Some(s) = self.single {
            return Some(s);
        }
        let mut code = 0u16;
        for len in 1..=self.max_len {
            code |= (r.read(1) as u16) << (len - 1);
            if let Some(&s) = self.map.get(&(len, code)) {
                return Some(s);
            }
        }
        None
    }
}

// ── VP8L Huffman tree (de)serialization ───────────────────────────────────────

const CODE_LENGTH_ORDER: [usize; 19] = [
    17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];

/// Write one Huffman tree (the "normal", non-simple form) given symbol lengths.
fn write_tree(w: &mut BitW, lengths: &[u8]) {
    w.write(0, 1); // not simple
                   // Code-length-code lengths: build a Huffman over the 19 length symbols of the
                   // run-length-encoded `lengths` stream.
    let rle = rle_lengths(lengths);
    let mut cl_freq = [0u32; 19];
    for &(s, _) in &rle {
        cl_freq[s as usize] += 1;
    }
    let cl_lengths = huffman_lengths(&cl_freq, 7);
    let cl_codes = canonical_codes(&cl_lengths);
    // num_code_lengths in CODE_LENGTH_ORDER; trim trailing zeros (min 4).
    let mut num = 19;
    while num > 4 && cl_lengths[CODE_LENGTH_ORDER[num - 1]] == 0 {
        num -= 1;
    }
    w.write((num - 4) as u32, 4);
    for &idx in CODE_LENGTH_ORDER.iter().take(num) {
        w.write(cl_lengths[idx] as u32, 3);
    }
    // max_symbol: 0 → use full alphabet.
    w.write(0, 1);
    // Emit the RLE stream with cl_codes.
    for &(s, extra) in &rle {
        w.write(cl_codes[s as usize] as u32, cl_lengths[s as usize] as u32);
        match s {
            16 => w.write(extra, 2),
            17 => w.write(extra, 3),
            18 => w.write(extra, 7),
            _ => {}
        }
    }
}

/// Run-length encode a code-length array into `(symbol, extra_bits)` per VP8L:
/// 0..15 literal, 16 = repeat previous 3–6, 17 = repeat 0 (3–10), 18 = repeat 0
/// (11–138).
fn rle_lengths(lengths: &[u8]) -> Vec<(u8, u32)> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut prev = 8u8; // VP8L "previous non-zero" starts at 8
    while i < lengths.len() {
        let v = lengths[i];
        let mut run = 1;
        while i + run < lengths.len() && lengths[i + run] == v {
            run += 1;
        }
        if v == 0 {
            // Use 17 (3..10) / 18 (11..138) for zero runs; else literal 0s.
            let mut rem = run;
            while rem >= 11 {
                let take = rem.min(138);
                out.push((18u8, (take - 11) as u32));
                rem -= take;
            }
            while rem >= 3 {
                let take = rem.min(10);
                out.push((17u8, (take - 3) as u32));
                rem -= take;
            }
            for _ in 0..rem {
                out.push((0u8, 0));
            }
        } else {
            out.push((v, 0));
            let mut rem = run - 1;
            // 16 repeats the *previous* length 3..6 times.
            if v == prev {
                while rem >= 3 {
                    let take = rem.min(6);
                    out.push((16u8, (take - 3) as u32));
                    rem -= take;
                }
            }
            for _ in 0..rem {
                out.push((v, 0));
            }
            prev = v;
        }
        i += run;
    }
    out
}

/// Read one Huffman tree's symbol lengths (non-simple path assumed; simple path
/// handled inline). `alphabet` is the symbol count.
fn read_tree(r: &mut BitR, alphabet: usize) -> Option<Huff> {
    if r.read(1) == 1 {
        // Simple code: 1 or 2 symbols.
        let num = r.read(1) + 1;
        let first_bits = if r.read(1) == 1 { 8 } else { 1 };
        let s0 = r.read(first_bits) as usize;
        if num == 1 {
            // One implicit symbol: decoding it consumes **zero** bits.
            return Some(Huff::single(s0.min(alphabet.saturating_sub(1)) as u16));
        }
        // Two symbols, each a 1-bit code.
        let mut lengths = vec![0u8; alphabet];
        if s0 < alphabet {
            lengths[s0] = 1;
        }
        let s1 = r.read(8) as usize;
        if s1 < alphabet {
            lengths[s1] = 1;
        }
        return Some(Huff::from_lengths(&lengths));
    }
    let num = (r.read(4) + 4) as usize;
    let mut cl_lengths = [0u8; 19];
    for &idx in CODE_LENGTH_ORDER.iter().take(num) {
        cl_lengths[idx] = r.read(3) as u8;
    }
    let cl_huff = Huff::from_lengths(&cl_lengths);
    let max_symbol = if r.read(1) == 1 {
        let len = 2 + 2 * r.read(3);
        2 + r.read(len) as usize
    } else {
        alphabet
    };
    let mut lengths = vec![0u8; alphabet];
    let mut prev = 8u8;
    let mut i = 0;
    let mut symbols_left = max_symbol;
    while i < alphabet && symbols_left > 0 {
        let s = cl_huff.decode(r)? as u8;
        symbols_left -= 1;
        match s {
            0..=15 => {
                lengths[i] = s;
                if s != 0 {
                    prev = s;
                }
                i += 1;
            }
            16 => {
                let rep = 3 + r.read(2) as usize;
                for _ in 0..rep {
                    if i < alphabet {
                        lengths[i] = prev;
                        i += 1;
                    }
                }
            }
            17 => {
                let rep = 3 + r.read(3) as usize;
                i += rep.min(alphabet - i);
            }
            18 => {
                let rep = 11 + r.read(7) as usize;
                i += rep.min(alphabet - i);
            }
            _ => return None,
        }
    }
    Some(Huff::from_lengths(&lengths))
}

// ── container ─────────────────────────────────────────────────────────────────

fn riff_wrap(vp8l: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::from(vp8l);
    if chunk.len() % 2 == 1 {
        chunk.push(0);
    }
    let mut out = Vec::with_capacity(chunk.len() + 20);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((chunk.len() + 12) as u32).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(b"VP8L");
    out.extend_from_slice(&(vp8l.len() as u32).to_le_bytes());
    out.extend_from_slice(vp8l);
    if vp8l.len() % 2 == 1 {
        out.push(0);
    }
    out
}

// ── encode ────────────────────────────────────────────────────────────────────

/// Encode RGBA (`width*height*4`) to a lossless WebP (VP8L). Empty on bad input.
pub fn encode_webp(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || rgba.len() != (width as usize * height as usize * 4)
    {
        return Vec::new();
    }
    let mut w = BitW::new();
    w.write(0x2F, 8); // VP8L signature
    w.write(width - 1, 14);
    w.write(height - 1, 14);
    w.write(1, 1); // alpha used
    w.write(0, 3); // version
    w.write(0, 1); // no transform

    // Single Huffman group, no colour cache.
    w.write(0, 1); // colour cache present = 0
    w.write(0, 1); // use meta-huffman = 0

    // Frequencies for the 5 trees (green has 256 + 24 length codes; literals only
    // use 0..255). Distance tree is present but unused → length 0 everywhere
    // except a dummy symbol so it serializes.
    let n = width as usize * height as usize;
    let (mut fg, mut fr, mut fb, mut fa) = ([0u32; 280], [0u32; 256], [0u32; 256], [0u32; 256]);
    for i in 0..n {
        fr[rgba[i * 4] as usize] += 1;
        fg[rgba[i * 4 + 1] as usize] += 1;
        fb[rgba[i * 4 + 2] as usize] += 1;
        fa[rgba[i * 4 + 3] as usize] += 1;
    }
    let lg = huffman_lengths(&fg, 15);
    let lr = huffman_lengths(&fr, 15);
    let lb = huffman_lengths(&fb, 15);
    let la = huffman_lengths(&fa, 15);
    let mut ld = vec![0u8; 40];
    ld[0] = 1; // a single dummy distance symbol (never emitted)
    write_tree(&mut w, &lg);
    write_tree(&mut w, &lr);
    write_tree(&mut w, &lb);
    write_tree(&mut w, &la);
    write_tree(&mut w, &ld);

    let cg = canonical_codes(&lg);
    let cr = canonical_codes(&lr);
    let cb = canonical_codes(&lb);
    let ca = canonical_codes(&la);
    for i in 0..n {
        let (r8, g8, b8, a8) = (
            rgba[i * 4] as usize,
            rgba[i * 4 + 1] as usize,
            rgba[i * 4 + 2] as usize,
            rgba[i * 4 + 3] as usize,
        );
        w.write(cg[g8] as u32, lg[g8] as u32);
        w.write(cr[r8] as u32, lr[r8] as u32);
        w.write(cb[b8] as u32, lb[b8] as u32);
        w.write(ca[a8] as u32, la[a8] as u32);
    }
    riff_wrap(&w.finish())
}

// ── decode (full VP8L: transforms, meta-huffman, LZ77, colour cache) ──────────
//
// The encoder above writes a minimal subset (no transforms, single Huffman
// group, literal pixels), but real `cwebp`/`libwebp` lossless files routinely
// use the four spatial/colour transforms, a meta-Huffman "entropy image" that
// selects a different Huffman group per block, and LZ77 backward references.
// The decoder below handles the complete VP8L lossless format (ISO of the open
// WebP-lossless spec) so any conformant file decodes correctly.

/// Maps the first 120 LZ77 distance prefixes to a 2-D `(xoffset, yoffset)`
/// plane (libwebp `kCodeToPlane`). Entry `v` → `yoffset = v >> 4`,
/// `xoffset = 8 - (v & 0xf)`; `distance = yoffset * width + xoffset` (≥ 1).
#[rustfmt::skip]
const DISTANCE_PLANE: [u8; 120] = [
    0x18, 0x07, 0x17, 0x19, 0x28, 0x06, 0x27, 0x29, 0x16, 0x1a,
    0x26, 0x2a, 0x38, 0x05, 0x37, 0x39, 0x15, 0x1b, 0x36, 0x3a,
    0x25, 0x2b, 0x48, 0x04, 0x47, 0x49, 0x14, 0x1c, 0x35, 0x3b,
    0x46, 0x4a, 0x24, 0x2c, 0x58, 0x45, 0x4b, 0x34, 0x3c, 0x03,
    0x57, 0x59, 0x13, 0x1d, 0x56, 0x5a, 0x23, 0x2d, 0x44, 0x4c,
    0x55, 0x5b, 0x33, 0x3d, 0x68, 0x02, 0x67, 0x69, 0x12, 0x1e,
    0x66, 0x6a, 0x22, 0x2e, 0x54, 0x5c, 0x43, 0x4d, 0x65, 0x6b,
    0x32, 0x3e, 0x78, 0x01, 0x77, 0x79, 0x53, 0x5d, 0x11, 0x1f,
    0x64, 0x6c, 0x42, 0x4e, 0x76, 0x7a, 0x21, 0x2f, 0x75, 0x7b,
    0x31, 0x3f, 0x63, 0x6d, 0x52, 0x5e, 0x00, 0x74, 0x7c, 0x41,
    0x4f, 0x10, 0x20, 0x62, 0x6e, 0x30, 0x73, 0x7d, 0x51, 0x5f,
    0x40, 0x72, 0x7e, 0x61, 0x6f, 0x50, 0x71, 0x7f, 0x60, 0x70,
];

/// Decode a VP8L prefix code into its actual length/distance value (1-based).
/// `prefix < 4` → `prefix + 1`; otherwise `extra = (prefix - 2) >> 1`,
/// `offset = (2 + (prefix & 1)) << extra`, value = `offset + read(extra) + 1`.
fn prefix_value(prefix: u32, r: &mut BitR) -> u32 {
    if prefix < 4 {
        return prefix + 1;
    }
    let extra = (prefix - 2) >> 1;
    let offset = (2 + (prefix & 1)) << extra;
    offset + r.read(extra) + 1
}

/// Translate a 1-based LZ77 `dist_code` into an actual pixel distance, applying
/// the small-distance 2-D plane remapping for codes ≤ 120.
fn dist_code_to_pixels(dist_code: u32, width: u32) -> u32 {
    if dist_code > 120 {
        return dist_code - 120;
    }
    let v = DISTANCE_PLANE[(dist_code - 1) as usize] as i32;
    let yoffset = v >> 4;
    let xoffset = 8 - (v & 0x0f);
    let d = yoffset * width as i32 + xoffset;
    if d < 1 {
        1
    } else {
        d as u32
    }
}

/// One Huffman group: the five trees used to decode an ARGB pixel.
struct HGroup {
    green: Huff, // 256 literals + 24 length codes + (1 << cache_bits) cache codes
    red: Huff,
    blue: Huff,
    alpha: Huff,
    dist: Huff,
}

fn read_hgroup(r: &mut BitR, cache_bits: u32) -> Option<HGroup> {
    let green_alphabet = 256 + 24 + if cache_bits > 0 { 1usize << cache_bits } else { 0 };
    Some(HGroup {
        green: read_tree(r, green_alphabet)?,
        red: read_tree(r, 256)?,
        blue: read_tree(r, 256)?,
        alpha: read_tree(r, 256)?,
        dist: read_tree(r, 40)?,
    })
}

#[inline]
fn cache_hash(argb: u32, cache_bits: u32) -> usize {
    (argb.wrapping_mul(0x1e35a7bd) >> (32 - cache_bits)) as usize
}

/// Decode a full VP8L entropy-coded image into ARGB pixels. Reads the optional
/// colour cache and (when `allow_meta` is set) the meta-Huffman entropy image;
/// then the per-group trees; then the literal / cache / LZ77 symbol stream.
/// Shared by the main image and the transform sub-images (which pass
/// `allow_meta = false`).
fn decode_vp8l_image(r: &mut BitR, width: u32, height: u32, allow_meta: bool) -> Option<Vec<u32>> {
    let n = (width as usize).checked_mul(height as usize)?;

    // Colour cache.
    let cache_bits = if r.read(1) == 1 {
        let b = r.read(4);
        if b == 0 || b > 11 {
            return None;
        }
        b
    } else {
        0
    };

    // Meta-Huffman: an entropy image at `huff_bits` resolution routes each pixel
    // block to one of `num_groups` Huffman groups.
    let mut huff_bits = 0u32;
    let mut entropy_image: Vec<u32> = Vec::new();
    let mut huff_xsize = 1u32;
    let mut num_groups = 1usize;
    if allow_meta && r.read(1) == 1 {
        huff_bits = r.read(3) + 2;
        huff_xsize = sub_size(width, huff_bits);
        let huff_ysize = sub_size(height, huff_bits);
        // The entropy image is itself a VP8L image (no transforms, no meta, but
        // a colour cache is permitted); the group index lives in the red+green
        // bytes of each pixel.
        let img = decode_vp8l_image(r, huff_xsize, huff_ysize, false)?;
        let mut max_group = 0usize;
        for &px in &img {
            let g = (((px >> 16) & 0xff) << 8 | ((px >> 8) & 0xff)) as usize;
            if g > max_group {
                max_group = g;
            }
        }
        num_groups = max_group + 1;
        entropy_image = img;
    }

    // Per-group trees.
    let mut groups = Vec::with_capacity(num_groups);
    for _ in 0..num_groups {
        groups.push(read_hgroup(r, cache_bits)?);
    }

    let cache_size = if cache_bits > 0 { 1usize << cache_bits } else { 0 };
    let mut cache = vec![0u32; cache_size];
    let mut out = vec![0u32; n];

    let group_at = |x: u32, y: u32, entropy: &[u32]| -> usize {
        if entropy.is_empty() {
            return 0;
        }
        let idx = (y >> huff_bits) * huff_xsize + (x >> huff_bits);
        let px = entropy.get(idx as usize).copied().unwrap_or(0);
        (((px >> 16) & 0xff) << 8 | ((px >> 8) & 0xff)) as usize
    };

    let mut x = 0u32;
    let mut y = 0u32;
    let mut i = 0usize;
    while i < n {
        let g = groups.get(group_at(x, y, &entropy_image))?;
        let sym = g.green.decode(r)? as usize;
        if sym < 256 {
            // Literal pixel: green already decoded; read R, B, A.
            let rr = g.red.decode(r)? as u32;
            let bb = g.blue.decode(r)? as u32;
            let aa = g.alpha.decode(r)? as u32;
            let argb = (aa << 24) | (rr << 16) | ((sym as u32) << 8) | bb;
            out[i] = argb;
            if cache_bits > 0 {
                cache[cache_hash(argb, cache_bits)] = argb;
            }
            i += 1;
            x += 1;
            if x == width {
                x = 0;
                y += 1;
            }
        } else if sym < 256 + 24 {
            // LZ77 backward reference: length, then distance.
            let length = prefix_value((sym - 256) as u32, r) as usize;
            let dist_sym = g.dist.decode(r)? as u32;
            let dist_code = prefix_value(dist_sym, r);
            let dist = dist_code_to_pixels(dist_code, width) as usize;
            if dist == 0 || dist > i || i + length > n {
                return None;
            }
            for k in 0..length {
                let argb = out[i - dist + k];
                out[i + k] = argb;
                if cache_bits > 0 {
                    cache[cache_hash(argb, cache_bits)] = argb;
                }
            }
            i += length;
            // Advance the (x, y) raster cursor by `length`.
            let nx = x as usize + length;
            x = (nx as u32) % width;
            y += (nx as u32) / width;
        } else {
            // Colour-cache reference.
            if cache_bits == 0 {
                return None;
            }
            let idx = sym - 256 - 24;
            let argb = *cache.get(idx)?;
            out[i] = argb;
            i += 1;
            x += 1;
            if x == width {
                x = 0;
                y += 1;
            }
        }
    }
    Some(out)
}

// ── VP8L transforms (applied in reverse order at the end of decoding) ─────────

/// Sub-resolution dimension for a transform/entropy image at `bits` block size.
#[inline]
fn sub_size(size: u32, bits: u32) -> u32 {
    (size + (1 << bits) - 1) >> bits
}

#[inline]
fn add_argb(a: u32, b: u32) -> u32 {
    let aa = ((a >> 24) + (b >> 24)) & 0xff;
    let ar = (((a >> 16) & 0xff) + ((b >> 16) & 0xff)) & 0xff;
    let ag = (((a >> 8) & 0xff) + ((b >> 8) & 0xff)) & 0xff;
    let ab = ((a & 0xff) + (b & 0xff)) & 0xff;
    (aa << 24) | (ar << 16) | (ag << 8) | ab
}

#[inline]
fn avg2(a: u32, b: u32) -> u32 {
    let mut out = 0u32;
    for s in [0u32, 8, 16, 24] {
        let v = (((a >> s) & 0xff) + ((b >> s) & 0xff)) / 2;
        out |= v << s;
    }
    out
}

/// `Clamp_Add_Subtract_Full` predictor helper for modes 11–13.
#[inline]
fn clamp_add_sub_full(a: i32, b: i32, c: i32) -> u32 {
    let mut out = 0u32;
    for s in [0u32, 8, 16, 24] {
        let av = (a >> s) & 0xff;
        let bv = (b >> s) & 0xff;
        let cv = (c >> s) & 0xff;
        let v = (av + bv - cv).clamp(0, 255) as u32;
        out |= v << s;
    }
    out
}

/// `Clamp_Add_Subtract_Half` predictor helper for mode 13.
#[inline]
fn clamp_add_sub_half(a: u32, b: u32) -> u32 {
    let mut out = 0u32;
    for s in [0u32, 8, 16, 24] {
        let av = ((a >> s) & 0xff) as i32;
        let bv = ((b >> s) & 0xff) as i32;
        let v = (av + (av - bv) / 2).clamp(0, 255) as u32;
        out |= v << s;
    }
    out
}

/// Mode 12 select predictor: choose `t` or `l` based on the gradient.
#[inline]
fn select_pred(t: u32, l: u32, tl: u32) -> u32 {
    let mut p_l = 0i32;
    let mut p_t = 0i32;
    for s in [0u32, 8, 16, 24] {
        let tv = ((t >> s) & 0xff) as i32;
        let lv = ((l >> s) & 0xff) as i32;
        let tlv = ((tl >> s) & 0xff) as i32;
        p_l += (tv - tlv).abs();
        p_t += (lv - tlv).abs();
    }
    if p_l < p_t {
        l
    } else {
        t
    }
}

/// Undo the predictor transform: each pixel had its spatial prediction
/// subtracted; add it back. `pred_img` is the sub-resolution mode image.
fn apply_predictor(px: &mut [u32], width: u32, height: u32, bits: u32, pred_img: &[u32]) -> Option<()> {
    let w = width as usize;
    let xsize = sub_size(width, bits) as usize;
    for y in 0..height as usize {
        for x in 0..w {
            let i = y * w + x;
            // Top-left pixel uses a fixed predictor (opaque black ARGB add).
            let pred = if x == 0 && y == 0 {
                0xff00_0000
            } else if y == 0 {
                px[i - 1] // left
            } else if x == 0 {
                px[i - w] // top
            } else {
                let block = (y >> bits) * xsize + (x >> bits);
                let mode = ((pred_img.get(block)? >> 8) & 0xff) as u8;
                let l = px[i - 1];
                let t = px[i - w];
                let tl = px[i - w - 1];
                // Top-right: at the last column VP8L reads the contiguous
                // `top[width]`, which is the leftmost pixel of the *current*
                // row — `px[i - w + 1]` yields that automatically.
                let tr = px[i - w + 1];
                predict(mode, l, t, tl, tr)
            };
            px[i] = add_argb(px[i], pred);
        }
    }
    Some(())
}

/// The 14 VP8L spatial predictors given left `l`, top `t`, top-left `tl`,
/// top-right `tr` neighbours.
fn predict(mode: u8, l: u32, t: u32, tl: u32, tr: u32) -> u32 {
    match mode {
        0 => 0xff00_0000,
        1 => l,
        2 => t,
        3 => tr,
        4 => tl,
        5 => avg2(avg2(l, tr), t),
        6 => avg2(l, tl),
        7 => avg2(l, t),
        8 => avg2(tl, t),
        9 => avg2(t, tr),
        10 => avg2(avg2(l, tl), avg2(t, tr)),
        11 => select_pred(t, l, tl),
        12 => clamp_add_sub_full(l as i32, t as i32, tl as i32),
        13 => clamp_add_sub_half(avg2(l, t), tl),
        _ => l,
    }
}

/// Undo the colour transform: re-correlate red/blue from green per block.
fn apply_color_transform(px: &mut [u32], width: u32, height: u32, bits: u32, cimg: &[u32]) -> Option<()> {
    let w = width as usize;
    let xsize = sub_size(width, bits) as usize;
    for y in 0..height as usize {
        for x in 0..w {
            let i = y * w + x;
            let block = (y >> bits) * xsize + (x >> bits);
            let c = *cimg.get(block)?;
            // ColorTransformElement packed in the sub-image pixel:
            //   green_to_red   = c & 0xff   (blue byte)
            //   green_to_blue  = (c >> 8) & 0xff   (green byte)
            //   red_to_blue    = (c >> 16) & 0xff  (red byte)
            let g2r = (c & 0xff) as i8 as i32;
            let g2b = ((c >> 8) & 0xff) as i8 as i32;
            let r2b = ((c >> 16) & 0xff) as i8 as i32;
            let argb = px[i];
            let green = ((argb >> 8) & 0xff) as i32;
            let mut red = ((argb >> 16) & 0xff) as i32;
            let mut blue = (argb & 0xff) as i32;
            red += (g2r * sign_ext8(green)) >> 5;
            blue += (g2b * sign_ext8(green)) >> 5;
            blue += (r2b * sign_ext8(red & 0xff)) >> 5;
            let red = (red & 0xff) as u32;
            let blue = (blue & 0xff) as u32;
            px[i] = (argb & 0xff00_ff00) | (red << 16) | blue;
        }
    }
    Some(())
}

/// Sign-extend the low byte of a value to a full `i32` (the colour transform
/// multiplies by a signed 8-bit channel delta).
#[inline]
fn sign_ext8(v: i32) -> i32 {
    (v as u8) as i8 as i32
}

/// Undo subtract-green: add the green channel back to red and blue.
fn apply_subtract_green(px: &mut [u32]) {
    for p in px.iter_mut() {
        let argb = *p;
        let green = (argb >> 8) & 0xff;
        let red = ((argb >> 16) + green) & 0xff;
        let blue = (argb + green) & 0xff;
        *p = (argb & 0xff00_ff00) | (red << 16) | blue;
    }
}

/// Undo colour-indexing: replace each (green) index by its palette entry,
/// un-bundling packed pixels when the palette has ≤ 16 entries.
fn apply_color_indexing(
    px: &[u32],
    width: u32,
    height: u32,
    palette: &[u32],
) -> Option<Vec<u32>> {
    let table_size = palette.len();
    let bits = if table_size <= 2 {
        3u32
    } else if table_size <= 4 {
        2
    } else if table_size <= 16 {
        1
    } else {
        0
    };
    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0u32; w * h];
    if bits == 0 {
        for (o, &p) in out.iter_mut().zip(px.iter()) {
            let idx = ((p >> 8) & 0xff) as usize;
            *o = palette.get(idx).copied().unwrap_or(0);
        }
        return Some(out);
    }
    // Packed: each byte of green holds `1 << bits` indices, low bits first.
    let per = 1usize << bits; // pixels per packed sample
    let mask = (1u32 << (8 >> bits)) - 1; // index field width = 8 >> bits bits
    let packed_w = sub_size(width, bits) as usize;
    for y in 0..h {
        for x in 0..w {
            let packed = px.get(y * packed_w + x / per)?;
            let green = (packed >> 8) & 0xff;
            let sub = (x % per) as u32;
            let idx = ((green >> (sub * (8 >> bits))) & mask) as usize;
            out[y * w + x] = palette.get(idx).copied().unwrap_or(0);
        }
    }
    Some(out)
}

/// A decoded transform, kept until the pixel stream is read, then unwound in
/// reverse order.
enum Transform {
    Predictor { bits: u32, image: Vec<u32> },
    Color { bits: u32, image: Vec<u32> },
    SubtractGreen,
    ColorIndexing { palette: Vec<u32> },
}

/// Read a VP8L palette (1-row sub-image of `size` ARGB entries) and undo its
/// delta-coding (each entry is stored as a component-wise delta of the prior).
fn read_palette(r: &mut BitR, size: u32) -> Option<Vec<u32>> {
    let mut pal = decode_vp8l_image(r, size, 1, false)?;
    for k in 1..pal.len() {
        pal[k] = add_argb(pal[k], pal[k - 1]);
    }
    Some(pal)
}

/// Decode a WebP to `(width, height, rgba)` — both lossless (`VP8L`) and lossy
/// (`VP8 `, a VP8 keyframe). `None` for extended WebP or a malformed stream.
pub fn decode_webp(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if data.len() < 20 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return None;
    }
    // Find the VP8/VP8L chunk; a lossy `VP8 ` chunk routes to the VP8 decoder.
    let mut pos = 12;
    let mut vp8l: Option<&[u8]> = None;
    while pos + 8 <= data.len() {
        let tag = &data[pos..pos + 4];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().ok()?) as usize;
        let body = data.get(pos + 8..pos + 8 + size)?;
        if tag == b"VP8 " {
            return super::vp8::decode(body);
        }
        if tag == b"VP8L" {
            vp8l = Some(body);
            break;
        }
        pos += 8 + size + (size & 1);
    }
    let vp8l = vp8l?;
    let mut r = BitR { d: vp8l, pos: 0 };
    if r.read(8) != 0x2F {
        return None;
    }
    let width = r.read(14) + 1;
    let height = r.read(14) + 1;
    let _alpha = r.read(1);
    if r.read(3) != 0 {
        return None; // unknown version
    }

    // Read the transform chain. Colour-indexing may shrink the decoded image
    // width (pixel bundling), so track the working width separately.
    let mut transforms: Vec<Transform> = Vec::new();
    let mut work_width = width;
    let mut seen = [false; 4];
    while r.read(1) == 1 {
        let kind = r.read(2);
        if seen[kind as usize] {
            return None; // each transform may appear at most once
        }
        seen[kind as usize] = true;
        match kind {
            0 => {
                // Predictor.
                let bits = r.read(3) + 2;
                let xs = sub_size(work_width, bits);
                let ys = sub_size(height, bits);
                let image = decode_vp8l_image(&mut r, xs, ys, false)?;
                transforms.push(Transform::Predictor { bits, image });
            }
            1 => {
                // Colour.
                let bits = r.read(3) + 2;
                let xs = sub_size(work_width, bits);
                let ys = sub_size(height, bits);
                let image = decode_vp8l_image(&mut r, xs, ys, false)?;
                transforms.push(Transform::Color { bits, image });
            }
            2 => transforms.push(Transform::SubtractGreen),
            3 => {
                // Colour indexing.
                let table_size = r.read(8) + 1;
                let palette = read_palette(&mut r, table_size)?;
                let bits = if table_size <= 2 {
                    3
                } else if table_size <= 4 {
                    2
                } else if table_size <= 16 {
                    1
                } else {
                    0
                };
                work_width = sub_size(work_width, bits);
                transforms.push(Transform::ColorIndexing { palette });
            }
            _ => return None,
        }
    }

    // Decode the (possibly bundled) entropy-coded image at the working width.
    let mut px = decode_vp8l_image(&mut r, work_width, height, true)?;
    let mut cur_w = work_width;

    // Unwind transforms in reverse declaration order.
    for t in transforms.into_iter().rev() {
        match t {
            Transform::ColorIndexing { palette } => {
                px = apply_color_indexing(&px, width, height, &palette)?;
                cur_w = width;
            }
            Transform::SubtractGreen => apply_subtract_green(&mut px),
            Transform::Color { bits, image } => {
                apply_color_transform(&mut px, cur_w, height, bits, &image)?;
            }
            Transform::Predictor { bits, image } => {
                apply_predictor(&mut px, cur_w, height, bits, &image)?;
            }
        }
    }

    if cur_w != width || px.len() != (width as usize * height as usize) {
        return None;
    }

    // ARGB → RGBA.
    let n = width as usize * height as usize;
    let mut out = vec![0u8; n * 4];
    for (i, &argb) in px.iter().enumerate() {
        out[i * 4] = (argb >> 16) as u8;
        out[i * 4 + 1] = (argb >> 8) as u8;
        out[i * 4 + 2] = argb as u8;
        out[i * 4 + 3] = (argb >> 24) as u8;
    }
    Some((width, height, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossless_round_trip() {
        let (w, h) = (12u32, 9u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let p = ((y * w + x) * 4) as usize;
                rgba[p] = (x * 20) as u8;
                rgba[p + 1] = (y * 25) as u8;
                rgba[p + 2] = ((x + y) * 10) as u8;
                rgba[p + 3] = 255;
            }
        }
        let webp = encode_webp(w, h, &rgba);
        assert_eq!(&webp[0..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
        let (dw, dh, dec) = decode_webp(&webp).expect("decodes");
        assert_eq!((dw, dh), (w, h));
        assert_eq!(dec, rgba, "lossless: exact round-trip");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(encode_webp(0, 0, &[]).is_empty());
        assert!(decode_webp(b"not webp").is_none());
    }

    // ── hand-crafted VP8L bitstreams exercising each transform ────────────────
    //
    // These build real VP8L streams with `BitW` (the same writer the encoder
    // uses) and assert the decoder undoes each transform to the expected
    // pixels. They prove the transform decode paths independently of any
    // external `cwebp`/`libwebp` binary (zero-dependency at test time).

    /// Emit a single-Huffman-group, no-cache VP8L image *body* (everything
    /// after the per-image header) carrying `pixels` as literals. The
    /// meta-Huffman bit is written only for a top-level image (`with_meta`);
    /// sub-images (transform/entropy images) never carry it, matching the
    /// decoder's `allow_meta` gating.
    fn emit_literal_body(w: &mut BitW, with_meta: bool, pixels: &[u32]) {
        w.write(0, 1); // colour cache present = 0
        if with_meta {
            w.write(0, 1); // meta-huffman = 0
        }
        let (mut fg, mut fr, mut fb, mut fa) =
            ([0u32; 280], [0u32; 256], [0u32; 256], [0u32; 256]);
        for &p in pixels {
            fr[((p >> 16) & 0xff) as usize] += 1;
            fg[((p >> 8) & 0xff) as usize] += 1;
            fb[(p & 0xff) as usize] += 1;
            fa[((p >> 24) & 0xff) as usize] += 1;
        }
        let lg = huffman_lengths(&fg, 15);
        let lr = huffman_lengths(&fr, 15);
        let lb = huffman_lengths(&fb, 15);
        let la = huffman_lengths(&fa, 15);
        let mut ld = vec![0u8; 40];
        ld[0] = 1; // single dummy distance symbol (never emitted)
        write_tree(w, &lg);
        write_tree(w, &lr);
        write_tree(w, &lb);
        write_tree(w, &la);
        write_tree(w, &ld);
        let cg = canonical_codes(&lg);
        let cr = canonical_codes(&lr);
        let cb = canonical_codes(&lb);
        let ca = canonical_codes(&la);
        for &p in pixels {
            let (r8, g8, b8, a8) = (
                ((p >> 16) & 0xff) as usize,
                ((p >> 8) & 0xff) as usize,
                (p & 0xff) as usize,
                ((p >> 24) & 0xff) as usize,
            );
            w.write(cg[g8] as u32, lg[g8] as u32);
            w.write(cr[r8] as u32, lr[r8] as u32);
            w.write(cb[b8] as u32, lb[b8] as u32);
            w.write(ca[a8] as u32, la[a8] as u32);
        }
    }

    /// Build a complete VP8L WebP: header + `transforms` (each a closure that
    /// writes its transform sub-stream) + the literal main image `pixels` at
    /// `main_w`×`height`.
    fn build_vp8l(
        width: u32,
        height: u32,
        transforms: &[&dyn Fn(&mut BitW)],
        main_w: u32,
        pixels: &[u32],
    ) -> Vec<u8> {
        let mut w = BitW::new();
        w.write(0x2F, 8);
        w.write(width - 1, 14);
        w.write(height - 1, 14);
        w.write(1, 1); // alpha used
        w.write(0, 3); // version
        for t in transforms {
            w.write(1, 1); // transform present
            t(&mut w);
        }
        w.write(0, 1); // no more transforms
        let _ = main_w;
        emit_literal_body(&mut w, true, pixels);
        riff_wrap(&w.finish())
    }

    fn decoded_argb(webp: &[u8], width: u32, height: u32) -> Vec<u32> {
        let (dw, dh, rgba) = decode_webp(webp).expect("decode");
        assert_eq!((dw, dh), (width, height));
        rgba
            .chunks_exact(4)
            .map(|c| {
                ((c[3] as u32) << 24)
                    | ((c[0] as u32) << 16)
                    | ((c[1] as u32) << 8)
                    | c[2] as u32
            })
            .collect()
    }

    #[test]
    fn transform_subtract_green() {
        // 2×2 residuals; decoder must add green back into red & blue.
        // residual ARGB → expected: red = (r+g)&0xff, blue = (b+g)&0xff.
        let (w, h) = (2u32, 2u32);
        let resid = [0xff10_2030u32, 0xff00_ff00, 0xff40_0801, 0xfffe_0102];
        let sg = |bw: &mut BitW| bw.write(2, 2); // type 2 = subtract-green
        let webp = build_vp8l(w, h, &[&sg], w, &resid);
        let got = decoded_argb(&webp, w, h);
        let expect: Vec<u32> = resid
            .iter()
            .map(|&p| {
                let g = (p >> 8) & 0xff;
                let r = ((p >> 16) + g) & 0xff;
                let b = (p + g) & 0xff;
                (p & 0xff00_ff00) | (r << 16) | b
            })
            .collect();
        assert_eq!(got, expect, "subtract-green inverse");
    }

    #[test]
    fn transform_predictor_modes() {
        // 3×2 image, one 4×4 block (size_bits=2 → block 4 covers the image), so
        // a single mode pixel applies to all interior pixels. Use mode 1 (left).
        let (w, h) = (3u32, 2u32);
        // residuals (these are added to the spatial prediction)
        let resid = [
            0xff01_0203u32, 0xff00_0001, 0xff00_0001, // row 0
            0xff00_0102, 0xff00_0000, 0xff00_0000, // row 1
        ];
        // sub-image is 1×1 (ceil(3/4)=1, ceil(2/4)=1); green byte = mode 1.
        let pred = |bw: &mut BitW| {
            bw.write(0, 2); // type 0 = predictor
            bw.write(0, 3); // size_bits - 2 = 0 → size_bits 2 → block 4
            emit_literal_body(bw, false, &[0x0000_0100]); // green = 1 (mode 1 = left)
        };
        let webp = build_vp8l(w, h, &[&pred], w, &resid);
        let got = decoded_argb(&webp, w, h);
        // Reconstruct expected by the spec rules.
        let mut exp = vec![0u32; (w * h) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = y * w as usize + x;
                let pred = if x == 0 && y == 0 {
                    0xff00_0000
                } else if y == 0 {
                    exp[i - 1] // left
                } else if x == 0 {
                    exp[i - w as usize] // top
                } else {
                    exp[i - 1] // mode 1 = left
                };
                exp[i] = add_argb(resid[i], pred);
            }
        }
        assert_eq!(got, exp, "predictor mode-1 inverse");
    }

    #[test]
    fn transform_color() {
        // 2×2; colour transform with a known ColorTransformElement.
        // sub-image pixel packs: blue=g2r, green=g2b, red=r2b (signed 8-bit).
        let (w, h) = (2u32, 2u32);
        let resid = [0xff20_4060u32, 0xff10_2030, 0xff80_1008, 0xff00_70a0];
        let g2r = 12i32;
        let g2b = -8i32;
        let r2b = 5i32;
        let cte = (((r2b as u8 as u32) << 16) | ((g2b as u8 as u32) << 8) | (g2r as u8 as u32))
            & 0x00ff_ffff;
        let color = move |bw: &mut BitW| {
            bw.write(1, 2); // type 1 = colour transform
            bw.write(0, 3); // size_bits 2 → 1×1 sub-image
            emit_literal_body(bw, false, &[cte]);
        };
        let webp = build_vp8l(w, h, &[&color], w, &resid);
        let got = decoded_argb(&webp, w, h);
        let expect: Vec<u32> = resid
            .iter()
            .map(|&p| {
                let green = ((p >> 8) & 0xff) as i32;
                let mut red = ((p >> 16) & 0xff) as i32;
                let mut blue = (p & 0xff) as i32;
                red += (g2r * (green as u8 as i8 as i32)) >> 5;
                blue += (g2b * (green as u8 as i8 as i32)) >> 5;
                blue += (r2b * ((red & 0xff) as u8 as i8 as i32)) >> 5;
                (p & 0xff00_ff00) | (((red & 0xff) as u32) << 16) | ((blue & 0xff) as u32)
            })
            .collect();
        assert_eq!(got, expect, "colour transform inverse");
    }

    #[test]
    fn transform_color_indexing_unbundled() {
        // 4 colours but a 32-entry palette forces no bundling (bits=0): each
        // pixel's green byte is a direct palette index.
        let (w, h) = (3u32, 2u32);
        let palette: Vec<u32> = (0..20u32)
            .map(|k| 0xff00_0000 | (k * 0x010203))
            .collect();
        let size = palette.len() as u32; // 20 > 16 → unbundled
        // palette delta-coded for transmission
        let mut deltas = vec![palette[0]];
        for k in 1..palette.len() {
            // delta = entry - prev (component-wise, mod 256)
            let a = palette[k];
            let b = palette[k - 1];
            let d = (((a >> 24).wrapping_sub(b >> 24) & 0xff) << 24)
                | ((((a >> 16) & 0xff).wrapping_sub((b >> 16) & 0xff) & 0xff) << 16)
                | ((((a >> 8) & 0xff).wrapping_sub((b >> 8) & 0xff) & 0xff) << 8)
                | ((a & 0xff).wrapping_sub(b & 0xff) & 0xff);
            deltas.push(d);
        }
        let cidx = move |bw: &mut BitW| {
            bw.write(3, 2); // type 3 = colour indexing
            bw.write(size - 1, 8);
            emit_literal_body(bw, false, &deltas);
        };
        // main image carries indices in the green byte.
        let idx = [3u32, 7, 19, 0, 11, 5];
        let main: Vec<u32> = idx.iter().map(|&k| 0xff00_0000 | (k << 8)).collect();
        let webp = build_vp8l(w, h, &[&cidx], w, &main);
        let got = decoded_argb(&webp, w, h);
        let expect: Vec<u32> = idx.iter().map(|&k| palette[k as usize]).collect();
        assert_eq!(got, expect, "colour-indexing (unbundled) inverse");
    }

    #[test]
    fn transform_color_indexing_bundled() {
        // 2-colour palette → 3-bit bundling (8 pixels per packed byte). Verify
        // the un-bundling un-packs the correct indices.
        let (w, h) = (5u32, 2u32); // 10 px, packs into ceil(5/8)=1 sample/row
        let palette = [0xff10_2030u32, 0xfffa_fbfc];
        let deltas = [palette[0], {
            let a = palette[1];
            let b = palette[0];
            (((a >> 24).wrapping_sub(b >> 24) & 0xff) << 24)
                | ((((a >> 16) & 0xff).wrapping_sub((b >> 16) & 0xff) & 0xff) << 16)
                | ((((a >> 8) & 0xff).wrapping_sub((b >> 8) & 0xff) & 0xff) << 8)
                | ((a & 0xff).wrapping_sub(b & 0xff) & 0xff)
        }];
        let cidx = move |bw: &mut BitW| {
            bw.write(3, 2);
            bw.write(1, 8); // table_size - 1 = 1 → 2 colours
            emit_literal_body(bw, false, &deltas);
        };
        // Choose per-pixel indices, then pack 8 per byte (low bits first).
        let idx_row0 = [1u32, 0, 1, 1, 0]; // 5 indices in row 0
        let idx_row1 = [0u32, 1, 0, 0, 1];
        let pack = |idxs: &[u32]| -> u32 {
            let mut g = 0u32;
            for (j, &v) in idxs.iter().enumerate() {
                g |= v << (j as u32); // 1 bit per index (8 >> 3 = 1)
            }
            0xff00_0000 | (g << 8)
        };
        // packed image is 1 wide × 2 tall (one byte per row).
        let main = [pack(&idx_row0), pack(&idx_row1)];
        let webp = build_vp8l(w, h, &[&cidx], 1, &main);
        let got = decoded_argb(&webp, w, h);
        let mut expect = Vec::new();
        for &k in idx_row0.iter() {
            expect.push(palette[k as usize]);
        }
        for &k in idx_row1.iter() {
            expect.push(palette[k as usize]);
        }
        assert_eq!(got, expect, "colour-indexing (3-bit bundle) inverse");
    }

    /// The four channel length/code tables for one Huffman group, built from
    /// the symbols it must encode.
    struct GroupCodes {
        lg: Vec<u8>,
        lr: Vec<u8>,
        lb: Vec<u8>,
        la: Vec<u8>,
        cg: Vec<u16>,
        cr: Vec<u16>,
        cb: Vec<u16>,
        ca: Vec<u16>,
    }
    fn group_codes(pixels: &[u32]) -> GroupCodes {
        let (mut fg, mut fr, mut fb, mut fa) =
            ([0u32; 280], [0u32; 256], [0u32; 256], [0u32; 256]);
        for &p in pixels {
            fr[((p >> 16) & 0xff) as usize] += 1;
            fg[((p >> 8) & 0xff) as usize] += 1;
            fb[(p & 0xff) as usize] += 1;
            fa[((p >> 24) & 0xff) as usize] += 1;
        }
        let lg = huffman_lengths(&fg, 15);
        let lr = huffman_lengths(&fr, 15);
        let lb = huffman_lengths(&fb, 15);
        let la = huffman_lengths(&fa, 15);
        GroupCodes {
            cg: canonical_codes(&lg),
            cr: canonical_codes(&lr),
            cb: canonical_codes(&lb),
            ca: canonical_codes(&la),
            lg,
            lr,
            lb,
            la,
        }
    }
    /// Write a group's five trees (the dummy distance tree included).
    fn write_group_trees(w: &mut BitW, gc: &GroupCodes) {
        write_tree(w, &gc.lg);
        write_tree(w, &gc.lr);
        write_tree(w, &gc.lb);
        write_tree(w, &gc.la);
        let mut ld = vec![0u8; 40];
        ld[0] = 1;
        write_tree(w, &ld);
    }
    /// Emit one literal pixel with a group's codes.
    fn write_pixel(w: &mut BitW, gc: &GroupCodes, p: u32) {
        let (r8, g8, b8, a8) = (
            ((p >> 16) & 0xff) as usize,
            ((p >> 8) & 0xff) as usize,
            (p & 0xff) as usize,
            ((p >> 24) & 0xff) as usize,
        );
        w.write(gc.cg[g8] as u32, gc.lg[g8] as u32);
        w.write(gc.cr[r8] as u32, gc.lr[r8] as u32);
        w.write(gc.cb[b8] as u32, gc.lb[b8] as u32);
        w.write(gc.ca[a8] as u32, gc.la[a8] as u32);
    }

    #[test]
    fn meta_huffman_two_groups() {
        // 8×1 image, huff_bits=2 (block size 4) → a 2×1 entropy image routes the
        // left half to Huffman group 0 and the right half to group 1. VP8L writes
        // ALL group trees first, then one combined raster pixel stream, so the
        // test proves both the entropy-image sub-decode and per-pixel group
        // routing are correct.
        let (w, h) = (8u32, 1u32);
        let main = [
            0xff11_2233u32, 0xff44_5566, 0xff11_2233, 0xff44_5566, // block 0 → group 0
            0xff77_8899, 0xffaa_bbcc, 0xff77_8899, 0xffaa_bbcc, // block 1 → group 1
        ];
        // Entropy image 2×1; group index = (red<<8 | green). Left=0, right=1.
        let entropy = [0x0000_0000u32, 0x0000_0100];
        let g0 = group_codes(&main[0..4]);
        let g1 = group_codes(&main[4..8]);
        let mut bw = BitW::new();
        bw.write(0x2F, 8);
        bw.write(w - 1, 14);
        bw.write(h - 1, 14);
        bw.write(1, 1); // alpha used
        bw.write(0, 3); // version
        bw.write(0, 1); // no transforms
        // Main image header.
        bw.write(0, 1); // colour cache present = 0
        bw.write(1, 1); // use meta-huffman = 1
        bw.write(0, 3); // huff_bits - 2 = 0 → block size 4
        emit_literal_body(&mut bw, false, &entropy); // entropy image (2×1)
        // All group trees first…
        write_group_trees(&mut bw, &g0);
        write_group_trees(&mut bw, &g1);
        // …then the raster pixel stream, each pixel via its routed group.
        for (x, &p) in main.iter().enumerate() {
            let gc = if (x >> 2) == 0 { &g0 } else { &g1 };
            write_pixel(&mut bw, gc, p);
        }
        let webp = riff_wrap(&bw.finish());
        let got = decoded_argb(&webp, w, h);
        assert_eq!(got, main, "meta-huffman group routing");
    }

    // ── real libwebp lossless fixtures (decode `cwebp`/libwebp output) ────────
    //
    // These byte arrays are genuine lossless WebP files produced by libwebp
    // (PIL `save(lossless=True, method=6)`), so they exercise libwebp's *own*
    // tree encoding (simple codes, max_symbol, etc.) and transforms — not just
    // this crate's encoder. They are fully opaque (alpha = 255), so the decoded
    // pixels equal the generating formula exactly (a true round-trip).

    /// 32×24 gradient, PREDICTOR transform (libwebp). RGB = (x·7, y·9, (x+y)·5).
    const GRAD_WEBP: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x30, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x4c,
        0x24, 0x00, 0x00, 0x00, 0x2f, 0x1f, 0xc0, 0x05, 0x00, 0xb9, 0x8c, 0xe8, 0x7f, 0xec, 0x22, 0xa2,
        0xff, 0x01, 0x21, 0x01, 0xa1, 0xfa, 0xbf, 0x5a, 0x95, 0xe6, 0x40, 0x41, 0xda, 0x06, 0x2c, 0x6c,
        0x77, 0x62, 0x02, 0x00, 0x5f, 0xeb, 0x7b, 0x03,
    ];

    /// 19×11 black/white checkerboard, COLOR_INDEXING transform with 3-bit
    /// pixel bundling (2-colour palette). Index = `(x·y) % 2`.
    const PAL2_WEBP: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x2e, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x4c,
        0x22, 0x00, 0x00, 0x00, 0x2f, 0x12, 0x80, 0x02, 0x00, 0x0f, 0x30, 0xff, 0xf3, 0x3f, 0xff, 0xf3,
        0x1f, 0x78, 0x20, 0xc8, 0xb6, 0x69, 0xee, 0x4f, 0x32, 0xd3, 0x1b, 0x44, 0xf4, 0x9f, 0x60, 0x92,
        0xa6, 0xda, 0x8e, 0x41, 0xed, 0x39,
    ];

    #[test]
    fn real_libwebp_predictor() {
        let (w, h) = (32u32, 24u32);
        let (dw, dh, rgba) = decode_webp(GRAD_WEBP).expect("decode real predictor WebP");
        assert_eq!((dw, dh), (w, h));
        let mut expect = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let p = ((y * w + x) * 4) as usize;
                expect[p] = ((x * 7) & 0xff) as u8;
                expect[p + 1] = ((y * 9) & 0xff) as u8;
                expect[p + 2] = (((x + y) * 5) & 0xff) as u8;
                expect[p + 3] = 255;
            }
        }
        assert_eq!(rgba, expect, "libwebp predictor decode");
    }

    #[test]
    fn real_libwebp_color_indexing() {
        let (w, h) = (19u32, 11u32);
        let (dw, dh, rgba) = decode_webp(PAL2_WEBP).expect("decode real colour-indexed WebP");
        assert_eq!((dw, dh), (w, h));
        let mut expect = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let p = ((y * w + x) * 4) as usize;
                let white = (x * y) % 2 != 0;
                let v = if white { 255 } else { 0 };
                expect[p] = v;
                expect[p + 1] = v;
                expect[p + 2] = v;
                expect[p + 3] = 255;
            }
        }
        assert_eq!(rgba, expect, "libwebp colour-indexing (bundled) decode");
    }
}
