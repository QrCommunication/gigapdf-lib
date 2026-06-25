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
//!   of greyscale (0) and truecolour (2) images;
//! * **APNG** animation chunks (`acTL`/`fcTL`/`fdAT`, the PNG animation
//!   extension): [`decode_png`] returns the fully-composited default image
//!   (the static fallback the spec defines), while [`decode_apng_frames`]
//!   returns the whole composited frame sequence honouring each frame's
//!   region, dispose op and blend op.
//!
//! Every chunk's stored CRC-32 is verified against `crc32(type ++ data)`; a
//! mismatch rejects the file.
//!
//! Returns `None` on any malformed or out-of-spec input — never panics.

use crate::filters::inflate::flate_decode;
use crate::raster::png::crc32;

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

/// One composited frame of an APNG animation: the full logical canvas as it
/// appears while this frame is shown, plus the authored display duration.
#[derive(Debug, Clone)]
pub struct ApngFrame {
    /// Canvas width (the IHDR width — same for every frame).
    pub width: u32,
    /// Canvas height (the IHDR height — same for every frame).
    pub height: u32,
    /// Composited RGBA pixels (`width * height * 4`).
    pub rgba: Vec<u8>,
    /// Display duration in milliseconds, from the `fcTL` `delay_num/delay_den`
    /// fraction of seconds (a `delay_den` of 0 means 1/100 s per the spec).
    pub delay_ms: u32,
}

/// A decoded APNG animation: the canvas size and every composited frame in
/// display order. A plain (non-animated) PNG is *not* one of these — use
/// [`decode_png`] for the static image.
#[derive(Debug, Clone)]
pub struct ApngAnimation {
    pub width: u32,
    pub height: u32,
    pub frames: Vec<ApngFrame>,
}

/// Decode a PNG byte slice into an RGBA8 image.
///
/// For an APNG, this returns the **default image** — the frames the IDAT carries
/// — which the spec mandates be a valid standalone still (whether or not it is
/// part of the animation). Use [`decode_apng_frames`] for the moving sequence.
///
/// Returns `None` if the input is not a valid PNG, uses an unsupported
/// colour-type/bit-depth combination, fails CRC verification, or is malformed
/// in any way.
pub fn decode_png(bytes: &[u8]) -> Option<DecodedImage> {
    let parsed = parse_chunks(bytes)?;
    // The IDAT default image always spans the full canvas at the origin.
    let rgba = decode_grid(&parsed, &parsed.idat_raw, parsed.width, parsed.height)?;
    Some(DecodedImage {
        width: parsed.width,
        height: parsed.height,
        rgba,
    })
}

/// Decode the **full APNG animation** (every `fcTL`/`fdAT` frame composited onto
/// the logical canvas in display order, honouring each frame's region, dispose
/// op and blend op). Returns `None` when `bytes` is not an APNG (no `acTL`), or
/// is malformed.
///
/// Per the APNG spec, the IDAT may or may not be the animation's first frame:
/// when an `fcTL` precedes the IDAT, the IDAT *is* frame 0; otherwise the IDAT
/// is a non-displayed default image and the animation starts at the first
/// `fdAT`-backed frame. Both cases are handled.
pub fn decode_apng_frames(bytes: &[u8]) -> Option<ApngAnimation> {
    let parsed = parse_chunks(bytes)?;
    if !parsed.is_apng || parsed.frames.is_empty() {
        return None;
    }

    let w = parsed.width as usize;
    let h = parsed.height as usize;
    // Persistent logical-screen canvas, composited frame after frame.
    let mut canvas = vec![0u8; w * h * 4];
    let mut out: Vec<ApngFrame> = Vec::with_capacity(parsed.frames.len());

    for fr in &parsed.frames {
        // Decode this frame's own sub-image grid (`fr.width × fr.height`).
        let sub = decode_grid(&parsed, &fr.data, fr.width, fr.height)?;

        // Snapshot for a "restore previous" dispose op (taken before painting).
        let prev = (fr.dispose == DISPOSE_PREVIOUS).then(|| canvas.clone());

        // Composite the sub-image into the canvas at (x_offset, y_offset).
        blit_frame(&mut canvas, parsed.width, parsed.height, &sub, fr);

        out.push(ApngFrame {
            width: parsed.width,
            height: parsed.height,
            rgba: canvas.clone(),
            delay_ms: frame_delay_ms(fr.delay_num, fr.delay_den),
        });

        // Apply the dispose op to prepare the canvas for the next frame.
        match fr.dispose {
            DISPOSE_BACKGROUND => clear_region(&mut canvas, parsed.width, fr),
            DISPOSE_PREVIOUS => {
                if let Some(p) = prev {
                    canvas = p;
                }
            }
            _ => {} // DISPOSE_NONE: leave the frame in place.
        }
    }

    Some(ApngAnimation {
        width: parsed.width,
        height: parsed.height,
        frames: out,
    })
}

// ─── Internals ───────────────────────────────────────────────────────────────

// APNG `fcTL` dispose ops (byte at offset 24). Op 0 (DISPOSE_NONE — leave the
// frame in place) is the implicit default and needs no named constant.
const DISPOSE_BACKGROUND: u8 = 1; // clear the frame's rect to transparent
const DISPOSE_PREVIOUS: u8 = 2; // revert the rect to the pre-frame canvas
                                // APNG `fcTL` blend op (byte at offset 25). Op 0 (BLEND_SOURCE — overwrite,
                                // alpha included) is the `else` default; only OVER needs a named constant.
const BLEND_OVER: u8 = 1; // source-over alpha composite

/// One APNG frame's control (`fcTL`) fields plus its concatenated image data
/// (the IDAT bytes for an IDAT-backed first frame, or the `fdAT` payloads with
/// their 4-byte sequence numbers already stripped).
struct ApngRawFrame {
    width: u32,
    height: u32,
    x_offset: u32,
    y_offset: u32,
    delay_num: u16,
    delay_den: u16,
    dispose: u8,
    blend: u8,
    /// zlib-compressed scanline data for this frame's own `width × height` grid.
    data: Vec<u8>,
}

/// Everything `decode_png`/`decode_apng_frames` need after one walk of the
/// chunk stream: the IHDR geometry, palette/transparency, the default-image
/// IDAT bytes, and (for an APNG) the per-frame control + data.
struct ParsedPng {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    interlace: u8,
    palette: Vec<[u8; 3]>,
    trns: Vec<u8>,
    trns_grey: Option<u16>,
    trns_rgb: Option<[u16; 3]>,
    idat_raw: Vec<u8>,
    is_apng: bool,
    frames: Vec<ApngRawFrame>,
}

/// Walk every chunk once: verify each CRC, validate the IHDR, and gather the
/// palette, transparency, default-image IDAT and APNG animation chunks.
fn parse_chunks(bytes: &[u8]) -> Option<ParsedPng> {
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

    // APNG state. `is_apng` is set by `acTL`; frames accrue from `fcTL`/`fdAT`.
    // An `fcTL` *before* the first IDAT means the IDAT is frame 0 — we route the
    // IDAT bytes into that frame's `data` once IEND is reached.
    let mut is_apng = false;
    let mut frames: Vec<ApngRawFrame> = Vec::new();
    let mut idat_is_frame0 = false;

    loop {
        // Each chunk: 4-byte length + 4-byte type + data + 4-byte CRC.
        let len = u32::from_be_bytes(*bytes.get(pos..pos + 4)?.first_chunk::<4>()?) as usize;
        pos += 4;
        let kind = bytes.get(pos..pos + 4)?;
        pos += 4;
        let data = bytes.get(pos..pos + len)?;
        pos += len;
        let crc = u32::from_be_bytes(*bytes.get(pos..pos + 4)?.first_chunk::<4>()?);
        pos += 4;

        // CRC-32 covers the chunk *type* followed by its *data* (not the length).
        let mut crc_input = Vec::with_capacity(4 + len);
        crc_input.extend_from_slice(kind);
        crc_input.extend_from_slice(data);
        if crc32(&crc_input) != crc {
            return None;
        }

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
            b"acTL" => {
                // Animation control: presence marks the file as an APNG.
                is_apng = true;
            }
            b"fcTL" => {
                // Frame control (26 bytes): seq(4) w(4) h(4) x(4) y(4)
                // delay_num(2) delay_den(2) dispose(1) blend(1).
                let f = parse_fctl(data)?;
                // If this fcTL appears before any IDAT, the upcoming IDAT is
                // this (first) frame's data — record that and push the frame now
                // so its `data` gets filled at IEND.
                if !ihdr_seen {
                    return None;
                }
                if idat_raw.is_empty() && !idat_is_frame0 && frames.is_empty() {
                    idat_is_frame0 = true;
                }
                frames.push(f);
            }
            b"fdAT" => {
                // Frame data: a 4-byte sequence number then raw IDAT-style bytes.
                let payload = data.get(4..)?;
                if let Some(last) = frames.last_mut() {
                    last.data.extend_from_slice(payload);
                } else {
                    return None; // fdAT with no preceding fcTL is malformed
                }
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

    // The IDAT must decode as a still (the spec requires it), so it can't be
    // empty for a valid file.
    if idat_raw.is_empty() {
        return None;
    }

    // If the first fcTL precedes the IDAT, frame 0 *is* the IDAT image — clone
    // the IDAT bytes into that frame's data so the animation walker can decode
    // it like any other frame.
    if is_apng && idat_is_frame0 {
        if let Some(first) = frames.first_mut() {
            if first.data.is_empty() {
                first.data = idat_raw.clone();
            }
        }
    }

    // ── 4. Transparency colour key (greyscale / truecolour) ─────────────
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

    Some(ParsedPng {
        width,
        height,
        bit_depth,
        color_type,
        interlace,
        palette,
        trns,
        trns_grey,
        trns_rgb,
        idat_raw,
        is_apng,
        frames,
    })
}

/// Parse a 26-byte `fcTL` chunk into an [`ApngRawFrame`] (with empty `data`).
fn parse_fctl(data: &[u8]) -> Option<ApngRawFrame> {
    if data.len() < 26 {
        return None;
    }
    let w = u32::from_be_bytes(*data.get(4..8)?.first_chunk::<4>()?);
    let h = u32::from_be_bytes(*data.get(8..12)?.first_chunk::<4>()?);
    let x = u32::from_be_bytes(*data.get(12..16)?.first_chunk::<4>()?);
    let y = u32::from_be_bytes(*data.get(16..20)?.first_chunk::<4>()?);
    let delay_num = u16::from_be_bytes([*data.get(20)?, *data.get(21)?]);
    let delay_den = u16::from_be_bytes([*data.get(22)?, *data.get(23)?]);
    let dispose = *data.get(24)?;
    let blend = *data.get(25)?;
    if w == 0 || h == 0 {
        return None;
    }
    Some(ApngRawFrame {
        width: w,
        height: h,
        x_offset: x,
        y_offset: y,
        delay_num,
        delay_den,
        dispose,
        blend,
        data: Vec::new(),
    })
}

/// Convert an APNG `delay_num/delay_den` fraction of seconds to milliseconds.
/// Per spec, `delay_den == 0` is treated as 100 (i.e. the unit is 1/100 s).
fn frame_delay_ms(num: u16, den: u16) -> u32 {
    let den = if den == 0 { 100 } else { den as u32 };
    (num as u32 * 1000) / den
}

/// Composite a decoded sub-image (`fr.width × fr.height` RGBA) onto `canvas`
/// (the full `cw × ch` logical screen) at `(fr.x_offset, fr.y_offset)`, using
/// the frame's blend op (source = overwrite, over = alpha composite). Pixels
/// that fall outside the canvas are clipped.
fn blit_frame(canvas: &mut [u8], cw: u32, ch: u32, sub: &[u8], fr: &ApngRawFrame) {
    let (cw, ch) = (cw as usize, ch as usize);
    let (fw, fh) = (fr.width as usize, fr.height as usize);
    let (ox, oy) = (fr.x_offset as usize, fr.y_offset as usize);
    for sy in 0..fh {
        let dy = oy + sy;
        if dy >= ch {
            break;
        }
        for sx in 0..fw {
            let dx = ox + sx;
            if dx >= cw {
                break;
            }
            let s = (sy * fw + sx) * 4;
            let d = (dy * cw + dx) * 4;
            let src = [sub[s], sub[s + 1], sub[s + 2], sub[s + 3]];
            if fr.blend == BLEND_OVER {
                composite_over(&mut canvas[d..d + 4], src);
            } else {
                // BLEND_SOURCE (0) and any unknown value: overwrite outright.
                canvas[d..d + 4].copy_from_slice(&src);
            }
        }
    }
}

/// Source-over alpha composite of `src` (straight-alpha RGBA) onto `dst`.
fn composite_over(dst: &mut [u8], src: [u8; 4]) {
    let sa = src[3] as u32;
    if sa == 255 {
        dst.copy_from_slice(&src);
        return;
    }
    if sa == 0 {
        return;
    }
    let da = dst[3] as u32;
    // out_a = sa + da*(1-sa); out_c = (sc*sa + dc*da*(1-sa)) / out_a   (0..255).
    let inv = 255 - sa;
    let out_a = sa + da * inv / 255;
    if out_a == 0 {
        for b in dst.iter_mut() {
            *b = 0;
        }
        return;
    }
    for c in 0..3 {
        let sc = src[c] as u32;
        let dc = dst[c] as u32;
        dst[c] = ((sc * sa + dc * da * inv / 255) / out_a) as u8;
    }
    dst[3] = out_a as u8;
}

/// Clear the frame's own rectangle in `canvas` back to fully transparent (the
/// "restore to background" dispose op).
fn clear_region(canvas: &mut [u8], cw: u32, fr: &ApngRawFrame) {
    let cw = cw as usize;
    let (fw, fh) = (fr.width as usize, fr.height as usize);
    let (ox, oy) = (fr.x_offset as usize, fr.y_offset as usize);
    let ch = canvas.len() / (cw.max(1) * 4);
    for sy in 0..fh {
        let dy = oy + sy;
        if dy >= ch {
            break;
        }
        for sx in 0..fw {
            let dx = ox + sx;
            if dx >= cw {
                break;
            }
            let d = (dy * cw + dx) * 4;
            canvas[d..d + 4].copy_from_slice(&[0, 0, 0, 0]);
        }
    }
}

/// Decompress and decode one image grid (`gw × gh`) from `compressed` zlib bytes
/// using the parsed colour/depth/interlace context. Used for both the IDAT
/// default image and each APNG frame's own sub-image.
fn decode_grid(p: &ParsedPng, compressed: &[u8], gw: u32, gh: u32) -> Option<Vec<u8>> {
    let pixel_count = (gw as usize).checked_mul(gh as usize)?;
    if pixel_count == 0 || pixel_count > 64 * 1024 * 1024 {
        return None;
    }
    let raw = flate_decode(compressed).ok()?;

    let ctx = ImageCtx {
        color_type: p.color_type,
        bit_depth: p.bit_depth,
        palette: &p.palette,
        trns: &p.trns,
        trns_grey: p.trns_grey,
        trns_rgb: p.trns_rgb,
    };

    let mut rgba = vec![0u8; pixel_count * 4];
    let mut offset = 0usize; // consumed bytes of `raw`

    if p.interlace == 0 {
        decode_pass(&raw, &mut offset, &ctx, gw, gh, &mut rgba, gw, |x, y| {
            (x, y)
        })?;
    } else {
        // Adam7: 7 passes, each a sparse sub-image mapped onto the full grid.
        for &(x0, y0, dx, dy) in &ADAM7 {
            let pw = pass_count(gw, x0, dx);
            let ph = pass_count(gh, y0, dy);
            if pw == 0 || ph == 0 {
                continue;
            }
            decode_pass(&raw, &mut offset, &ctx, pw, ph, &mut rgba, gw, |px, py| {
                (x0 + px * dx, y0 + py * dy)
            })?;
        }
    }
    Some(rgba)
}

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

    // ── #47: CRC-32 chunk verification ─────────────────────────────────────

    #[test]
    fn rejects_corrupt_chunk_crc() {
        // A valid PNG, then a single byte flipped inside the IDAT *data* (not the
        // CRC field), so the stored CRC no longer matches `crc32(type ++ data)`.
        let rgba = [10u8, 20, 30, 255, 40, 50, 60, 255];
        let mut png = encode_png(2, 1, &rgba);
        assert!(decode_png(&png).is_some(), "baseline must decode");

        // Locate the IDAT chunk and corrupt its first data byte.
        let idat = png.windows(4).position(|w| w == b"IDAT").unwrap();
        let data_start = idat + 4; // first byte of the IDAT data
        png[data_start] ^= 0xFF;
        assert!(
            decode_png(&png).is_none(),
            "a CRC mismatch must reject the file"
        );
    }

    #[test]
    fn accepts_correct_crc_roundtrip() {
        // The engine's own encoder writes correct CRCs → must verify and decode.
        let rgba = [1u8, 2, 3, 255, 4, 5, 6, 128, 7, 8, 9, 255, 10, 11, 12, 0];
        let png = encode_png(2, 2, &rgba);
        let img = decode_png(&png).expect("correct-CRC PNG must decode");
        assert_eq!(img.rgba, rgba);
    }

    // ── #48: APNG (acTL / fcTL / fdAT) ─────────────────────────────────────

    /// Build the `acTL` animation-control chunk (frame count + play count).
    fn actl(num_frames: u32, num_plays: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut d = Vec::new();
        d.extend_from_slice(&num_frames.to_be_bytes());
        d.extend_from_slice(&num_plays.to_be_bytes());
        chunk(&mut out, b"acTL", &d);
        out
    }

    /// Build an `fcTL` frame-control chunk.
    #[allow(clippy::too_many_arguments)]
    fn fctl(
        seq: u32,
        w: u32,
        h: u32,
        x: u32,
        y: u32,
        delay_num: u16,
        delay_den: u16,
        dispose: u8,
        blend: u8,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut d = Vec::new();
        d.extend_from_slice(&seq.to_be_bytes());
        d.extend_from_slice(&w.to_be_bytes());
        d.extend_from_slice(&h.to_be_bytes());
        d.extend_from_slice(&x.to_be_bytes());
        d.extend_from_slice(&y.to_be_bytes());
        d.extend_from_slice(&delay_num.to_be_bytes());
        d.extend_from_slice(&delay_den.to_be_bytes());
        d.push(dispose);
        d.push(blend);
        chunk(&mut out, b"fcTL", &d);
        out
    }

    /// Build an `fdAT` frame-data chunk: 4-byte seq + zlib-stored scanlines.
    fn fdat(seq: u32, idat_uncompressed: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut d = Vec::new();
        d.extend_from_slice(&seq.to_be_bytes());
        d.extend_from_slice(&zlib_store(idat_uncompressed));
        chunk(&mut out, b"fdAT", &d);
        out
    }

    /// One filter-0 scanline buffer for an 8-bit RGBA `w×h` solid colour.
    fn solid_rgba_scanlines(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        // Build a single row (filter byte 0 + `w` copies of `px`) then repeat it.
        let mut row = vec![0u8]; // filter None
        for _ in 0..w {
            row.extend_from_slice(&px);
        }
        row.repeat(h as usize)
    }

    /// Assemble an APNG: IHDR (8-bit RGBA) + acTL + (fcTL/IDAT for frame 0) +
    /// (fcTL/fdAT for each extra frame) + IEND.
    fn make_apng(canvas_w: u32, canvas_h: u32, frames: &[ApngTestFrame]) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&canvas_w.to_be_bytes());
        ihdr.extend_from_slice(&canvas_h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlace
        chunk(&mut out, b"IHDR", &ihdr);
        out.extend_from_slice(&actl(frames.len() as u32, 0));

        let mut seq = 0u32;
        for (i, f) in frames.iter().enumerate() {
            out.extend_from_slice(&fctl(
                seq, f.w, f.h, f.x, f.y, f.dnum, f.dden, f.dispose, f.blend,
            ));
            seq += 1;
            let scan = solid_rgba_scanlines(f.w, f.h, f.px);
            if i == 0 {
                // Frame 0 lives in IDAT (an fcTL precedes the IDAT).
                chunk(&mut out, b"IDAT", &zlib_store(&scan));
            } else {
                out.extend_from_slice(&fdat(seq, &scan));
                seq += 1;
            }
        }
        chunk(&mut out, b"IEND", &[]);
        out
    }

    struct ApngTestFrame {
        w: u32,
        h: u32,
        x: u32,
        y: u32,
        dnum: u16,
        dden: u16,
        dispose: u8,
        blend: u8,
        px: [u8; 4],
    }

    #[test]
    fn apng_default_image_is_idat_via_decode_png() {
        // The IDAT default image (frame 0) is a 2×2 red field. `decode_png` must
        // return exactly that still, ignoring the later animation frame.
        let frames = [
            ApngTestFrame {
                w: 2,
                h: 2,
                x: 0,
                y: 0,
                dnum: 1,
                dden: 10,
                dispose: 0,
                blend: 0,
                px: [255, 0, 0, 255],
            },
            ApngTestFrame {
                w: 2,
                h: 2,
                x: 0,
                y: 0,
                dnum: 1,
                dden: 10,
                dispose: 0,
                blend: 0,
                px: [0, 0, 255, 255],
            },
        ];
        let apng = make_apng(2, 2, &frames);
        let img = decode_png(&apng).expect("APNG default image must decode via decode_png");
        assert_eq!((img.width, img.height), (2, 2));
        // Every pixel red (the IDAT frame), NOT blue (the second frame).
        for px in img.rgba.chunks_exact(4) {
            assert_eq!(px, &[255, 0, 0, 255]);
        }
    }

    #[test]
    fn apng_two_full_frames_composited() {
        // Two full-canvas frames (red then blue), each non-blended (source).
        let frames = [
            ApngTestFrame {
                w: 2,
                h: 2,
                x: 0,
                y: 0,
                dnum: 1,
                dden: 20,
                dispose: 0,
                blend: 0,
                px: [255, 0, 0, 255],
            },
            ApngTestFrame {
                w: 2,
                h: 2,
                x: 0,
                y: 0,
                dnum: 3,
                dden: 20,
                dispose: 0,
                blend: 0,
                px: [0, 0, 255, 255],
            },
        ];
        let apng = make_apng(2, 2, &frames);
        let anim = decode_apng_frames(&apng).expect("APNG must decode to frames");
        assert_eq!((anim.width, anim.height), (2, 2));
        assert_eq!(anim.frames.len(), 2);
        // delay_ms = num*1000/den.
        assert_eq!(anim.frames[0].delay_ms, 50); // 1/20 s
        assert_eq!(anim.frames[1].delay_ms, 150); // 3/20 s
        for px in anim.frames[0].rgba.chunks_exact(4) {
            assert_eq!(px, &[255, 0, 0, 255], "frame 0 = red");
        }
        for px in anim.frames[1].rgba.chunks_exact(4) {
            assert_eq!(px, &[0, 0, 255, 255], "frame 1 = blue overwrites red");
        }
    }

    #[test]
    fn apng_subregion_frame_composites_over_base() {
        // 2×2 canvas. Frame 0: full red. Frame 1: a single opaque green pixel at
        // (1,0) with BLEND_OVER, DISPOSE_NONE → only that pixel changes.
        let frames = [
            ApngTestFrame {
                w: 2,
                h: 2,
                x: 0,
                y: 0,
                dnum: 1,
                dden: 10,
                dispose: 0,
                blend: 0,
                px: [255, 0, 0, 255],
            },
            ApngTestFrame {
                w: 1,
                h: 1,
                x: 1,
                y: 0,
                dnum: 1,
                dden: 10,
                dispose: 0,
                blend: 1,
                px: [0, 255, 0, 255],
            },
        ];
        let apng = make_apng(2, 2, &frames);
        let anim = decode_apng_frames(&apng).expect("APNG frames");
        let f1 = &anim.frames[1].rgba;
        assert_eq!(&f1[0..4], &[255, 0, 0, 255], "(0,0) stays red");
        assert_eq!(&f1[4..8], &[0, 255, 0, 255], "(1,0) painted green");
        assert_eq!(&f1[8..12], &[255, 0, 0, 255], "(0,1) stays red");
        assert_eq!(&f1[12..16], &[255, 0, 0, 255], "(1,1) stays red");
    }

    #[test]
    fn apng_dispose_background_clears_region() {
        // Frame 0: full red, DISPOSE_BACKGROUND (its rect cleared after). Frame 1:
        // a 1×1 blue pixel at (0,0). After frame 0 disposes to background, the
        // whole canvas is transparent, then frame 1 paints (0,0) blue → the rest
        // is transparent.
        let frames = [
            ApngTestFrame {
                w: 2,
                h: 2,
                x: 0,
                y: 0,
                dnum: 1,
                dden: 10,
                dispose: 1,
                blend: 0,
                px: [255, 0, 0, 255],
            },
            ApngTestFrame {
                w: 1,
                h: 1,
                x: 0,
                y: 0,
                dnum: 1,
                dden: 10,
                dispose: 0,
                blend: 0,
                px: [0, 0, 255, 255],
            },
        ];
        let apng = make_apng(2, 2, &frames);
        let anim = decode_apng_frames(&apng).expect("APNG frames");
        let f1 = &anim.frames[1].rgba;
        assert_eq!(&f1[0..4], &[0, 0, 255, 255], "(0,0) blue");
        assert_eq!(&f1[4..8], &[0, 0, 0, 0], "(1,0) cleared transparent");
        assert_eq!(&f1[8..12], &[0, 0, 0, 0], "(0,1) cleared transparent");
        assert_eq!(&f1[12..16], &[0, 0, 0, 0], "(1,1) cleared transparent");
    }

    #[test]
    fn decode_apng_frames_rejects_plain_png() {
        // A normal PNG has no acTL → not an animation.
        let png = encode_png(1, 1, &[1, 2, 3, 255]);
        assert!(
            decode_apng_frames(&png).is_none(),
            "a non-APNG must yield None"
        );
    }
}
