//! WebP encoder (lossless **VP8L**) + decoder (lossless **VP8L** *and* lossy
//! **VP8** keyframes) — pure std, zero dependency.
//!
//! Encodes RGBA losslessly (no spatial/colour transforms, single Huffman group,
//! literal pixels — valid VP8L every decoder accepts). Decodes VP8L streams (no
//! transforms, optional colour cache, LZ77 back-references, single Huffman
//! group) and lossy VP8 keyframes (intra-coded I-frames). The RIFF/WebP
//! container is read and written here. Extended (`VP8X`) and animated WebP are
//! not handled — `decode_webp` returns `None` for them. This is the native WebP
//! path replacing a third-party image library.

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

/// A decode table: walk bits LSB-first, matching `(len, code)`.
struct Huff {
    map: std::collections::HashMap<(u8, u16), u16>,
    max_len: u8,
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
        Huff { map, max_len }
    }
    fn decode(&self, r: &mut BitR) -> Option<u16> {
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
        let mut lengths = vec![0u8; alphabet];
        let s0 = r.read(first_bits) as usize;
        if s0 < alphabet {
            lengths[s0] = 1;
        }
        if num == 2 {
            let s1 = r.read(8) as usize;
            if s1 < alphabet {
                lengths[s1] = 1;
            }
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

// ── decode ────────────────────────────────────────────────────────────────────

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
    // Transforms unsupported in this minimal decoder (our encoder writes none).
    if r.read(1) == 1 {
        return None;
    }
    let cache_bits = if r.read(1) == 1 { r.read(4) } else { 0 };
    if r.read(1) == 1 {
        return None; // meta-huffman (multiple groups) unsupported
    }
    let green_alphabet = 256 + 24 + if cache_bits > 0 { 1 << cache_bits } else { 0 };
    let green = read_tree(&mut r, green_alphabet as usize)?;
    let red = read_tree(&mut r, 256)?;
    let blue = read_tree(&mut r, 256)?;
    let alpha = read_tree(&mut r, 256)?;
    let dist = read_tree(&mut r, 40)?;

    let n = width as usize * height as usize;
    let mut out = vec![0u8; n * 4];
    let mut cache = vec![0u32; if cache_bits > 0 { 1 << cache_bits } else { 0 }];
    let mut i = 0;
    while i < n {
        let g = green.decode(&mut r)? as usize;
        if g < 256 {
            let rr = red.decode(&mut r)? as u8;
            let bb = blue.decode(&mut r)? as u8;
            let aa = alpha.decode(&mut r)? as u8;
            out[i * 4] = rr;
            out[i * 4 + 1] = g as u8;
            out[i * 4 + 2] = bb;
            out[i * 4 + 3] = aa;
            if cache_bits > 0 {
                let argb = ((aa as u32) << 24)
                    | ((rr as u32) << 16)
                    | ((g as u32) << 8)
                    | bb as u32;
                cache[((argb.wrapping_mul(0x1e35a7bd)) >> (32 - cache_bits)) as usize] = argb;
            }
            i += 1;
        } else if g < 256 + 24 {
            // LZ77 back-reference — not produced by this encoder; decoding
            // arbitrary back-referenced VP8L is a future extension.
            let _ = &dist;
            return None;
        } else {
            // Colour-cache reference.
            let idx = g - 256 - 24;
            let argb = *cache.get(idx)?;
            out[i * 4] = (argb >> 16) as u8;
            out[i * 4 + 1] = (argb >> 8) as u8;
            out[i * 4 + 2] = argb as u8;
            out[i * 4 + 3] = (argb >> 24) as u8;
            i += 1;
        }
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
}
