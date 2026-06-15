//! Pure-`std` PNG decoder — zero external dependencies.
//!
//! Decodes the subset produced by [`super::png::encode_png`] and any
//! conformant PNG: 8-bit colour types 0, 2, 3, 4 and 6, non-interlaced.
//! Every output pixel is expanded to RGBA8 (4 bytes, row-major,
//! top-to-bottom). Returns `None` on any unsupported feature or malformed
//! input — never panics.

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
/// Returns `None` if the input is not a valid 8-bit non-interlaced PNG,
/// uses an unsupported colour type, or is malformed in any way.
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
    let mut trns: Vec<u8> = Vec::new(); // alpha values for palette entries
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
    if !ihdr_seen {
        return None;
    }
    // Only bit depth 8 is supported.
    if bit_depth != 8 {
        return None;
    }
    // Interlace must be 0 (non-interlaced).
    if interlace != 0 {
        return None;
    }
    // Validate colour type.
    let valid_ct = matches!(color_type, 0 | 2 | 3 | 4 | 6);
    if !valid_ct {
        return None;
    }
    // Palette required for colour type 3.
    if color_type == 3 && palette.is_empty() {
        return None;
    }
    // Zero dimensions are invalid.
    if width == 0 || height == 0 {
        return None;
    }

    // ── 4. Decompress IDAT ──────────────────────────────────────────────
    let raw = flate_decode(&idat_raw).ok()?;

    // ── 5. Determine samples-per-pixel (for filter reconstruction) ──────
    // bpp here = bytes per filter unit (before RGBA expansion), minimum 1.
    let samples_per_pixel: usize = match color_type {
        0 => 1, // grayscale
        2 => 3, // RGB
        3 => 1, // palette index
        4 => 2, // grayscale + alpha
        6 => 4, // RGBA
        _ => return None,
    };
    // Each pixel is 8 bits per sample, so bytes = samples.
    let bpp = samples_per_pixel; // bytes per pixel in the raw stream

    let stride = (width as usize) * bpp; // raw bytes per row (without filter byte)
    let row_len = stride + 1; // filter byte + raw bytes

    if raw.len() < row_len * height as usize {
        return None;
    }

    // ── 6. Unfilter scanlines ────────────────────────────────────────────
    // We reconstruct in-place into a flat Vec<u8> of size height * stride.
    let mut unfiltered: Vec<u8> = vec![0u8; height as usize * stride];

    for row in 0..height as usize {
        let row_start = row * row_len;
        let filter_type = *raw.get(row_start)?;
        let src = raw.get(row_start + 1..row_start + 1 + stride)?;
        let dst_start = row * stride;

        match filter_type {
            // None — copy verbatim.
            0 => {
                unfiltered[dst_start..dst_start + stride].copy_from_slice(src);
            }
            // Sub — each byte += byte to the left (same row).
            1 => {
                for i in 0..stride {
                    let left = if i >= bpp {
                        unfiltered[dst_start + i - bpp]
                    } else {
                        0
                    };
                    unfiltered[dst_start + i] = src[i].wrapping_add(left);
                }
            }
            // Up — each byte += byte above (previous row).
            2 => {
                for i in 0..stride {
                    let above = if row > 0 {
                        unfiltered[dst_start - stride + i]
                    } else {
                        0
                    };
                    unfiltered[dst_start + i] = src[i].wrapping_add(above);
                }
            }
            // Average — each byte += floor((left + above) / 2).
            3 => {
                for i in 0..stride {
                    let left: u32 = if i >= bpp {
                        unfiltered[dst_start + i - bpp] as u32
                    } else {
                        0
                    };
                    let above: u32 = if row > 0 {
                        unfiltered[dst_start - stride + i] as u32
                    } else {
                        0
                    };
                    let avg = ((left + above) / 2) as u8;
                    unfiltered[dst_start + i] = src[i].wrapping_add(avg);
                }
            }
            // Paeth — each byte += paeth_predictor(left, above, upper-left).
            4 => {
                for i in 0..stride {
                    let left: i32 = if i >= bpp {
                        unfiltered[dst_start + i - bpp] as i32
                    } else {
                        0
                    };
                    let above: i32 = if row > 0 {
                        unfiltered[dst_start - stride + i] as i32
                    } else {
                        0
                    };
                    let upper_left: i32 = if row > 0 && i >= bpp {
                        unfiltered[dst_start - stride + i - bpp] as i32
                    } else {
                        0
                    };
                    let p = left + above - upper_left;
                    let pa = (p - left).abs();
                    let pb = (p - above).abs();
                    let pc = (p - upper_left).abs();
                    let predictor = if pa <= pb && pa <= pc {
                        left
                    } else if pb <= pc {
                        above
                    } else {
                        upper_left
                    };
                    unfiltered[dst_start + i] = src[i].wrapping_add(predictor as u8);
                }
            }
            _ => return None, // unknown filter type
        }
    }

    // ── 7. Expand to RGBA ────────────────────────────────────────────────
    let pixel_count = (width as usize) * (height as usize);
    let mut rgba = vec![0u8; pixel_count * 4];

    match color_type {
        // Grayscale → (Y, Y, Y, 255)
        0 => {
            for (i, chunk) in unfiltered.chunks_exact(1).enumerate() {
                let y = chunk[0];
                let base = i * 4;
                rgba[base] = y;
                rgba[base + 1] = y;
                rgba[base + 2] = y;
                rgba[base + 3] = 255;
            }
        }
        // RGB → (R, G, B, 255)
        2 => {
            for (i, chunk) in unfiltered.chunks_exact(3).enumerate() {
                let base = i * 4;
                rgba[base] = chunk[0];
                rgba[base + 1] = chunk[1];
                rgba[base + 2] = chunk[2];
                rgba[base + 3] = 255;
            }
        }
        // Palette index → RGBA from palette + optional tRNS alpha.
        3 => {
            for (i, &idx) in unfiltered.iter().enumerate() {
                let entry = *palette.get(idx as usize)?;
                let alpha = trns.get(idx as usize).copied().unwrap_or(255);
                let base = i * 4;
                rgba[base] = entry[0];
                rgba[base + 1] = entry[1];
                rgba[base + 2] = entry[2];
                rgba[base + 3] = alpha;
            }
        }
        // Grayscale + Alpha → (Y, Y, Y, A)
        4 => {
            for (i, chunk) in unfiltered.chunks_exact(2).enumerate() {
                let y = chunk[0];
                let a = chunk[1];
                let base = i * 4;
                rgba[base] = y;
                rgba[base + 1] = y;
                rgba[base + 2] = y;
                rgba[base + 3] = a;
            }
        }
        // RGBA passthrough.
        6 => {
            rgba.copy_from_slice(&unfiltered);
        }
        _ => return None,
    }

    Some(DecodedImage {
        width,
        height,
        rgba,
    })
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
}
