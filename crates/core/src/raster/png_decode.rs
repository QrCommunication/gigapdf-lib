//! Pure-`std` PNG decoder — zero external dependencies.
//!
//! Decodes any conformant PNG into RGBA8 (4 bytes/pixel, row-major,
//! top-to-bottom):
//!
//! * colour types 0 (grey), 2 (truecolour), 3 (palette), 4 (grey+alpha),
//!   6 (truecolour+alpha);
//! * bit depths 1, 2, 4, 8 and 16 (16-bit samples are scaled down to 8-bit);
//! * both non-interlaced and Adam7-interlaced layouts;
//! * `tRNS` transparency for palette (3) and the single transparent colour key
//!   of greyscale (0) and truecolour (2) images.
//!
//! Returns `None` on any malformed or out-of-spec input — never panics.

use crate::filters::inflate::flate_decode;

// ─── Public API ────────────────────────────────────────────────────────────

/// A decoded PNG image in RGBA8 format.
///
/// `rgba.len() == width as usize * height as usize * 4`
#[derive(Debug)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Row-major, top-to-bottom, 4 bytes per pixel (R G B A).
    pub rgba: Vec<u8>,
}

/// Decode a PNG byte slice into an RGBA8 image.
///
/// Returns `None` if the input is not a valid PNG, uses an unsupported
/// colour-type/bit-depth combination, or is malformed in any way.
pub fn decode_png(bytes: &[u8]) -> Option<DecodedImage> {
    // ── 1. Signature ────────────────────────────────────────────────────
    let sig = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.get(..8)? != sig {
        return None;
    }
    let mut pos = 8usize;

    // ── 2. Chunk iteration ──────────────────────────────────────────────
    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut bit_depth: u8 = 0;
    let mut color_type: u8 = 0;
    let mut interlace: u8 = 0;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new(); // raw tRNS chunk bytes
    let mut idat_raw: Vec<u8> = Vec::new();
    let mut ihdr_seen = false;

    loop {
        // Each chunk: 4-byte length + 4-byte type + data + 4-byte CRC.
        let len = u32::from_be_bytes(*bytes.get(pos..pos + 4)?.first_chunk::<4>()?) as usize;
        pos += 4;
        let kind = bytes.get(pos..pos + 4)?;
        pos += 4;
        let data = bytes.get(pos..pos + len)?;
        pos += len;
        let _crc = bytes.get(pos..pos + 4)?; // skip CRC verification
        pos += 4;

        match kind {
            b"IHDR" => {
                if data.len() < 13 {
                    return None;
                }
                width = u32::from_be_bytes(*data.get(..4)?.first_chunk::<4>()?);
                height = u32::from_be_bytes(*data.get(4..8)?.first_chunk::<4>()?);
                bit_depth = *data.get(8)?;
                color_type = *data.get(9)?;
                // compression method (index 10) must be 0 — not checked (future-proof)
                // filter method (index 11) must be 0 — not checked
                interlace = *data.get(12)?;
                ihdr_seen = true;
            }
            b"PLTE" => {
                if data.len() % 3 != 0 {
                    return None;
                }
                palette = data.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            b"tRNS" => {
                trns = data.to_vec();
            }
            b"IDAT" => {
                idat_raw.extend_from_slice(data);
            }
            b"IEND" => break,
            _ => {} // unknown/ancillary chunks ignored
        }
    }

    // ── 3. Validate IHDR fields ─────────────────────────────────────────
    if !ihdr_seen || width == 0 || height == 0 {
        return None;
    }
    // Reject absurd dimensions before allocating (cap at 64M pixels ≈ 256 MiB
    // of RGBA output, plenty for any real document while bounding memory).
    let pixel_count = (width as usize).checked_mul(height as usize)?;
    if pixel_count > 64 * 1024 * 1024 {
        return None;
    }
    // Only the colour-type / bit-depth combinations the PNG spec permits.
    let depth_ok = match color_type {
        0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16), // greyscale
        3 => matches!(bit_depth, 1 | 2 | 4 | 8),      // palette (no 16-bit)
        2 | 4 | 6 => matches!(bit_depth, 8 | 16),     // truecolour / +alpha
        _ => false,
    };
    if !depth_ok {
        return None;
    }
    if interlace != 0 && interlace != 1 {
        return None;
    }
    // Palette required for colour type 3.
    if color_type == 3 && palette.is_empty() {
        return None;
    }

    // ── 4. Decompress IDAT ──────────────────────────────────────────────
    let raw = flate_decode(&idat_raw).ok()?;

    // ── 5. Transparency colour key (greyscale / truecolour) ─────────────
    // tRNS for type 0 holds one 16-bit grey sample; for type 2, three 16-bit
    // R/G/B samples. We compare against the *pre-scale* sample values.
    let trns_grey: Option<u16> = if color_type == 0 && trns.len() >= 2 {
        Some(u16::from_be_bytes([trns[0], trns[1]]))
    } else {
        None
    };
    let trns_rgb: Option<[u16; 3]> = if color_type == 2 && trns.len() >= 6 {
        Some([
            u16::from_be_bytes([trns[0], trns[1]]),
            u16::from_be_bytes([trns[2], trns[3]]),
            u16::from_be_bytes([trns[4], trns[5]]),
        ])
    } else {
        None
    };

    let ctx = ImageCtx {
        color_type,
        bit_depth,
        palette: &palette,
        trns: &trns,
        trns_grey,
        trns_rgb,
    };

    // ── 6. Decode passes into the RGBA grid ─────────────────────────────
    let mut rgba = vec![0u8; pixel_count * 4];
    let mut offset = 0usize; // consumed bytes of `raw`

    if interlace == 0 {
        decode_pass(
            &raw,
            &mut offset,
            &ctx,
            width,
            height,
            &mut rgba,
            width,
            |x, y| (x, y),
        )?;
    } else {
        // Adam7: 7 passes, each a sparse sub-image mapped onto the full grid.
        for &(x0, y0, dx, dy) in &ADAM7 {
            let pw = pass_count(width, x0, dx);
            let ph = pass_count(height, y0, dy);
            if pw == 0 || ph == 0 {
                continue;
            }
            decode_pass(
                &raw,
                &mut offset,
                &ctx,
                pw,
                ph,
                &mut rgba,
                width,
                |px, py| (x0 + px * dx, y0 + py * dy),
            )?;
        }
    }

    Some(DecodedImage {
        width,
        height,
        rgba,
    })
}

// ─── Internals ───────────────────────────────────────────────────────────────

/// Colour information shared across interlace passes.
struct ImageCtx<'a> {
    color_type: u8,
    bit_depth: u8,
    palette: &'a [[u8; 3]],
    trns: &'a [u8],
    trns_grey: Option<u16>,
    trns_rgb: Option<[u16; 3]>,
}

/// Adam7 pass origins and strides: `(x_start, y_start, x_step, y_step)`.
const ADAM7: [(u32, u32, u32, u32); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

/// Number of pixels along one axis covered by an Adam7 pass with the given
/// `start`/`step`, for an image extent of `extent`.
fn pass_count(extent: u32, start: u32, step: u32) -> u32 {
    if extent <= start {
        0
    } else {
        (extent - start).div_ceil(step)
    }
}

/// Channels in the raw stream per pixel for a colour type (before RGBA expand).
fn channels(color_type: u8) -> usize {
    match color_type {
        0 | 3 => 1, // grey / palette index
        2 => 3,     // RGB
        4 => 2,     // grey + alpha
        6 => 4,     // RGBA
        _ => 1,
    }
}

/// Decode one (sub-)image of `pw × ph` pixels from `raw` (advancing `*offset`),
/// expanding every pixel to RGBA8 and writing it into `out` (a full-image RGBA
/// buffer `out_w` pixels wide) at the position given by `place(px, py)`.
///
/// Handles bit depths 1/2/4/8/16, all colour types, and tRNS transparency.
#[allow(clippy::too_many_arguments)]
fn decode_pass(
    raw: &[u8],
    offset: &mut usize,
    ctx: &ImageCtx,
    pw: u32,
    ph: u32,
    out: &mut [u8],
    out_w: u32,
    place: impl Fn(u32, u32) -> (u32, u32),
) -> Option<()> {
    let ch = channels(ctx.color_type);
    let depth = ctx.bit_depth as usize;
    let bits_per_pixel = ch * depth;
    // Bytes per scanline (sub-byte depths pack pixels, rounding up to a byte).
    let stride = (pw as usize * bits_per_pixel).div_ceil(8);
    // Filter unit: bytes per pixel rounded up to ≥1 (the PNG "bpp" used by
    // Sub/Average/Paeth for the left neighbour).
    let bpp = bits_per_pixel.div_ceil(8);
    let row_len = stride + 1; // filter byte + scanline bytes

    let needed = row_len.checked_mul(ph as usize)?;
    if raw.len() < (*offset).checked_add(needed)? {
        return None;
    }

    // Unfilter every scanline of this pass into a contiguous buffer.
    let mut unfiltered = vec![0u8; stride * ph as usize];
    for row in 0..ph as usize {
        let row_start = *offset + row * row_len;
        let filter_type = raw[row_start];
        let src = &raw[row_start + 1..row_start + 1 + stride];
        let dst_start = row * stride;
        for i in 0..stride {
            let left = if i >= bpp {
                unfiltered[dst_start + i - bpp]
            } else {
                0
            };
            let above = if row > 0 {
                unfiltered[dst_start - stride + i]
            } else {
                0
            };
            let upper_left = if row > 0 && i >= bpp {
                unfiltered[dst_start - stride + i - bpp]
            } else {
                0
            };
            let recon = match filter_type {
                0 => src[i],                                              // None
                1 => src[i].wrapping_add(left),                           // Sub
                2 => src[i].wrapping_add(above),                          // Up
                3 => src[i].wrapping_add(avg(left, above)),               // Average
                4 => src[i].wrapping_add(paeth(left, above, upper_left)), // Paeth
                _ => return None,                                         // unknown filter
            };
            unfiltered[dst_start + i] = recon;
        }
    }
    *offset += needed;

    // Expand each pixel of the unfiltered scanlines to RGBA8 in `out`.
    let max_val: u32 = (1u32 << depth) - 1; // for 16-bit this is u32 (65535)
    for py in 0..ph {
        let row = &unfiltered[py as usize * stride..py as usize * stride + stride];
        let mut reader = SampleReader::new(row, depth);
        for px in 0..pw {
            // Read `ch` raw samples (each scaled to a u16 in 0..=max_val range
            // for key comparison; to u8 for the output channel value).
            let mut samples16 = [0u16; 4];
            let mut samples8 = [0u8; 4];
            for (s16, s8) in samples16.iter_mut().zip(samples8.iter_mut()).take(ch) {
                let s = reader.next_sample()?; // u16, already in 0..=max_val
                *s16 = s;
                *s8 = scale_to_u8(s, max_val);
            }

            let [r, g, b, a] = match ctx.color_type {
                0 => {
                    let y = samples8[0];
                    let a = match ctx.trns_grey {
                        Some(key) if samples16[0] == key => 0,
                        _ => 255,
                    };
                    [y, y, y, a]
                }
                2 => {
                    let a = match ctx.trns_rgb {
                        Some(k) if samples16[..3] == k[..] => 0,
                        _ => 255,
                    };
                    [samples8[0], samples8[1], samples8[2], a]
                }
                3 => {
                    // The raw sample is a palette index (already 0..=max_val for
                    // depths ≤ 8); look up colour + per-index tRNS alpha.
                    let idx = samples16[0] as usize;
                    let entry = *ctx.palette.get(idx)?;
                    let alpha = ctx.trns.get(idx).copied().unwrap_or(255);
                    [entry[0], entry[1], entry[2], alpha]
                }
                4 => {
                    let y = samples8[0];
                    [y, y, y, samples8[1]]
                }
                6 => [samples8[0], samples8[1], samples8[2], samples8[3]],
                _ => return None,
            };

            let (ox, oy) = place(px, py);
            let base = (oy as usize * out_w as usize + ox as usize) * 4;
            out[base] = r;
            out[base + 1] = g;
            out[base + 2] = b;
            out[base + 3] = a;
        }
    }

    Some(())
}

/// Reads consecutive PNG samples of a fixed bit depth from a packed scanline,
/// MSB-first for sub-byte depths. Each returned sample is the raw value in
/// `0..=(2^depth - 1)` (for 16-bit, the big-endian two-byte value).
struct SampleReader<'a> {
    row: &'a [u8],
    depth: usize,
    bit_pos: usize,  // for depths < 8
    byte_pos: usize, // for depths 8 and 16
}

impl<'a> SampleReader<'a> {
    fn new(row: &'a [u8], depth: usize) -> Self {
        SampleReader {
            row,
            depth,
            bit_pos: 0,
            byte_pos: 0,
        }
    }

    fn next_sample(&mut self) -> Option<u16> {
        match self.depth {
            16 => {
                let hi = *self.row.get(self.byte_pos)?;
                let lo = *self.row.get(self.byte_pos + 1)?;
                self.byte_pos += 2;
                Some(u16::from_be_bytes([hi, lo]))
            }
            8 => {
                let v = *self.row.get(self.byte_pos)?;
                self.byte_pos += 1;
                Some(v as u16)
            }
            d => {
                // Sub-byte: pull `d` bits MSB-first from the current byte.
                let byte = *self.row.get(self.bit_pos / 8)?;
                let shift = 8 - d - (self.bit_pos % 8);
                let mask = (1u16 << d) - 1;
                let v = ((byte as u16) >> shift) & mask;
                self.bit_pos += d;
                Some(v)
            }
        }
    }
}

/// Scale a raw sample value (range `0..=max_val`) to 8 bits.
fn scale_to_u8(v: u16, max_val: u32) -> u8 {
    match max_val {
        255 => v as u8,          // 8-bit: identity
        65535 => (v >> 8) as u8, // 16-bit: take the high byte
        0 => 0,                  // unreachable (depth ≥ 1)
        m => {
            // 1/2/4-bit: spread 0..=max_val across 0..=255.
            ((v as u32 * 255 + m / 2) / m) as u8
        }
    }
}

/// PNG Average-filter predictor: floor((left + above) / 2).
#[inline]
fn avg(left: u8, above: u8) -> u8 {
    ((left as u16 + above as u16) / 2) as u8
}

/// PNG Paeth predictor over the three neighbouring reconstructed bytes.
#[inline]
fn paeth(left: u8, above: u8, upper_left: u8) -> u8 {
    let a = left as i32;
    let b = above as i32;
    let c = upper_left as i32;
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        above
    } else {
        upper_left
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::png::encode_png;

    /// Round-trip: encode a 3×2 image, then decode it back.
    /// Uses varied colours and alpha values to exercise RGBA channels.
    #[test]
    fn round_trip_3x2() {
        #[rustfmt::skip]
        let original: Vec<u8> = vec![
            // row 0: red, semi-transparent green, blue
            255,   0,   0, 255,
              0, 255,   0, 128,
              0,   0, 255, 255,
            // row 1: white fully opaque, black transparent, yellow opaque
            255, 255, 255, 255,
              0,   0,   0,   0,
            255, 255,   0, 255,
        ];
        let png = encode_png(3, 2, &original);
        let img = decode_png(&png).expect("round-trip decode must succeed");
        assert_eq!(img.width, 3);
        assert_eq!(img.height, 2);
        assert_eq!(img.rgba, original, "RGBA pixels must survive round-trip");
    }

    /// Round-trip: minimal 1×1 image.
    #[test]
    fn round_trip_1x1() {
        let original = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let png = encode_png(1, 1, &original);
        let img = decode_png(&png).expect("1×1 round-trip must succeed");
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.rgba, original);
    }

    /// Rejection: not-a-PNG and truncated-but-valid-signature both return None.
    #[test]
    fn rejects_invalid_inputs() {
        // Garbage input.
        assert!(
            decode_png(b"not a png").is_none(),
            "garbage must return None"
        );
        // Correct 8-byte PNG signature but nothing else.
        let truncated = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert!(
            decode_png(&truncated).is_none(),
            "truncated PNG must return None"
        );
        // Empty slice.
        assert!(decode_png(b"").is_none(), "empty input must return None");
    }

    // ── Helpers to forge spec-conformant PNGs of arbitrary depth/type ──────

    fn crc32(bytes: &[u8]) -> u32 {
        // Standard PNG CRC-32 (IEEE 802.3, reflected, init 0xFFFFFFFF).
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in bytes {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        crc ^ 0xFFFF_FFFF
    }

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_in = Vec::new();
        crc_in.extend_from_slice(kind);
        crc_in.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
    }

    /// Build a PNG from already-filtered (filter byte 0 per row) + zlib-stored
    /// IDAT, so the decoder's inflate path is exercised on real zlib framing.
    #[allow(clippy::too_many_arguments)]
    fn make_png(
        w: u32,
        h: u32,
        depth: u8,
        color_type: u8,
        interlace: u8,
        plte: Option<&[u8]>,
        trns: Option<&[u8]>,
        idat_uncompressed: &[u8],
    ) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[depth, color_type, 0, 0, interlace]);
        chunk(&mut out, b"IHDR", &ihdr);
        if let Some(p) = plte {
            chunk(&mut out, b"PLTE", p);
        }
        if let Some(t) = trns {
            chunk(&mut out, b"tRNS", t);
        }
        chunk(&mut out, b"IDAT", &zlib_store(idat_uncompressed));
        chunk(&mut out, b"IEND", &[]);
        out
    }

    /// Wrap bytes in a zlib stream of stored (uncompressed) DEFLATE blocks.
    fn zlib_store(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let mut i = 0;
        while i < data.len() || data.is_empty() {
            let chunk = (data.len() - i).min(0xFFFF);
            let last = i + chunk >= data.len();
            out.push(if last { 1 } else { 0 });
            out.extend_from_slice(&(chunk as u16).to_le_bytes());
            out.extend_from_slice(&(!(chunk as u16)).to_le_bytes());
            out.extend_from_slice(&data[i..i + chunk]);
            i += chunk;
            if last {
                break;
            }
        }
        // Adler-32 trailer.
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    #[test]
    fn decodes_16bit_rgba() {
        // 2×1, 16-bit RGBA. Filter byte 0 + 2 pixels × 4 channels × 2 bytes.
        // Pixel 0 = (0xFFFF, 0x0000, 0x8000, 0xFFFF) → (255, 0, 128, 255)
        // Pixel 1 = (0x0000, 0xFFFF, 0x0000, 0x8000) → (0, 255, 0, 128)
        let row: Vec<u8> = vec![
            0x00, // filter None
            0xFF, 0xFF, 0x00, 0x00, 0x80, 0x00, 0xFF, 0xFF, // px0
            0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x80, 0x00, // px1
        ];
        let png = make_png(2, 1, 16, 6, 0, None, None, &row);
        let img = decode_png(&png).expect("16-bit RGBA must decode");
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(&img.rgba[..4], &[255, 0, 128, 255]);
        assert_eq!(&img.rgba[4..], &[0, 255, 0, 128]);
    }

    #[test]
    fn decodes_16bit_rgb() {
        // 1×1, 16-bit RGB (no alpha) → opaque.
        let row: Vec<u8> = vec![0x00, 0x12, 0x34, 0xAB, 0xCD, 0x00, 0xFF];
        let png = make_png(1, 1, 16, 2, 0, None, None, &row);
        let img = decode_png(&png).expect("16-bit RGB must decode");
        assert_eq!(&img.rgba, &[0x12, 0xAB, 0x00, 255]);
    }

    #[test]
    fn decodes_1bit_greyscale() {
        // 8×1, 1-bit grey: bits 1,0,1,1,0,0,1,0 → black/white. One packed byte.
        // MSB first: 0b1011_0010 = 0xB2.
        let row = vec![0x00, 0xB2];
        let png = make_png(8, 1, 1, 0, 0, None, None, &row);
        let img = decode_png(&png).expect("1-bit grey must decode");
        let alphas: Vec<u8> = img.rgba.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(alphas, vec![255, 0, 255, 255, 0, 0, 255, 0]);
    }

    #[test]
    fn decodes_4bit_palette() {
        // 4×1, 4-bit palette indices 0,1,2,3 packed into 2 bytes: 0x01, 0x23.
        let plte = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
        let row = vec![0x00, 0x01, 0x23];
        let png = make_png(4, 1, 4, 3, 0, Some(&plte), None, &row);
        let img = decode_png(&png).expect("4-bit palette must decode");
        assert_eq!(&img.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&img.rgba[4..8], &[0, 255, 0, 255]);
        assert_eq!(&img.rgba[8..12], &[0, 0, 255, 255]);
        assert_eq!(&img.rgba[12..16], &[255, 255, 0, 255]);
    }

    #[test]
    fn honours_truecolour_trns_colour_key() {
        // 2×1, 8-bit RGB with tRNS keying out pure red → that pixel transparent.
        let row = vec![0x00, 255, 0, 0, 0, 0, 255];
        let trns = [0x00, 0xFF, 0x00, 0x00, 0x00, 0x00]; // R=255,G=0,B=0
        let png = make_png(2, 1, 8, 2, 0, None, Some(&trns), &row);
        let img = decode_png(&png).expect("RGB+tRNS must decode");
        assert_eq!(&img.rgba[0..4], &[255, 0, 0, 0], "keyed colour → alpha 0");
        assert_eq!(&img.rgba[4..8], &[0, 0, 255, 255], "other colour opaque");
    }

    #[test]
    fn decodes_interlaced_rgba() {
        // 8×8 RGBA, Adam7 interlaced, built from a known image, then compared
        // against the same pixels decoded from a non-interlaced encoding.
        let mut rgba = Vec::new();
        for y in 0u32..8 {
            for x in 0u32..8 {
                rgba.extend_from_slice(&[
                    (x * 32) as u8,
                    (y * 32) as u8,
                    ((x + y) * 16) as u8,
                    if (x + y) % 2 == 0 { 128 } else { 255 },
                ]);
            }
        }
        // Reference: non-interlaced PNG via the engine encoder.
        let baseline = decode_png(&encode_png(8, 8, &rgba)).unwrap().rgba;

        // Forge the 7 Adam7 passes as filter-0 scanlines of the source image.
        let mut idat = Vec::new();
        for &(x0, y0, dx, dy) in &super::ADAM7 {
            let pw = super::pass_count(8, x0, dx);
            let ph = super::pass_count(8, y0, dy);
            for py in 0..ph {
                idat.push(0u8); // filter None
                for px in 0..pw {
                    let sx = x0 + px * dx;
                    let sy = y0 + py * dy;
                    let base = (sy as usize * 8 + sx as usize) * 4;
                    idat.extend_from_slice(&rgba[base..base + 4]);
                }
            }
        }
        let png = make_png(8, 8, 8, 6, 1, None, None, &idat);
        let img = decode_png(&png).expect("interlaced RGBA must decode");
        assert_eq!((img.width, img.height), (8, 8));
        assert_eq!(
            img.rgba, baseline,
            "interlaced output matches non-interlaced"
        );
    }

    #[test]
    fn rejects_bad_depth_colour_combo() {
        // Colour type 3 (palette) at 16-bit is illegal per spec.
        let png = make_png(1, 1, 16, 3, 0, Some(&[0, 0, 0]), None, &[0, 0, 0]);
        assert!(
            decode_png(&png).is_none(),
            "palette@16-bit must be rejected"
        );
    }
}
