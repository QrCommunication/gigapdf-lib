//! A minimal, correct PNG encoder — zero dependencies.
//!
//! Output is 8-bit RGBA (colour type 6). Each scanline is filtered with the PNG
//! filter (None, Sub, Up, Average or Paeth — PNG §9.2) that minimises the sum of
//! absolute signed residuals, the standard libpng heuristic. The filtered stream
//! is then compressed by the engine's own zlib encoder
//! ([`crate::filters::deflate::flate_encode`]: LZ77 + fixed Huffman), so a
//! rendered page is a small fraction of its raw `width * height * 4` bytes.

use crate::filters::deflate::flate_encode;

/// CRC-32 (IEEE, polynomial `0xEDB88320`) over `data`, as PNG chunks require.
///
/// Shared with the decoder ([`crate::raster::png_decode`]), which verifies each
/// chunk's stored CRC against `crc32(type ++ data)`.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Bytes per complete pixel for 8-bit RGBA — the `bpp` the PNG filters use to
/// find the "left" neighbour.
const BYTES_PER_PIXEL: usize = 4;

/// PNG filter type bytes (PNG §9.2), in the order they are tried.
const FILTER_NONE: u8 = 0;
const FILTER_SUB: u8 = 1;
const FILTER_UP: u8 = 2;
const FILTER_AVERAGE: u8 = 3;
const FILTER_PAETH: u8 = 4;
const FILTERS: [u8; 5] = [
    FILTER_NONE,
    FILTER_SUB,
    FILTER_UP,
    FILTER_AVERAGE,
    FILTER_PAETH,
];

/// The Paeth predictor (PNG §9.4): whichever of left / above / upper-left is
/// closest to `left + above - upper_left`, ties broken in that order.
fn paeth_predictor(left: u8, above: u8, upper_left: u8) -> u8 {
    let p = left as i16 + above as i16 - upper_left as i16;
    let pa = (p - left as i16).abs();
    let pb = (p - above as i16).abs();
    let pc = (p - upper_left as i16).abs();
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        above
    } else {
        upper_left
    }
}

/// Apply PNG filter `filter` to scanline `row` (whose predecessor is `prev`,
/// all-zero for the first row), writing the residuals into `out`.
fn filter_scanline(filter: u8, row: &[u8], prev: &[u8], out: &mut [u8]) {
    for i in 0..row.len() {
        let left = if i >= BYTES_PER_PIXEL {
            row[i - BYTES_PER_PIXEL]
        } else {
            0
        };
        let above = prev[i];
        let upper_left = if i >= BYTES_PER_PIXEL {
            prev[i - BYTES_PER_PIXEL]
        } else {
            0
        };
        let predicted = match filter {
            FILTER_SUB => left,
            FILTER_UP => above,
            FILTER_AVERAGE => ((left as u16 + above as u16) / 2) as u8,
            FILTER_PAETH => paeth_predictor(left, above, upper_left),
            _ => 0,
        };
        out[i] = row[i].wrapping_sub(predicted);
    }
}

/// Heuristic cost of a filtered scanline: the sum of its bytes read as signed
/// residuals. Small residuals (near 0 either way) compress best.
fn residual_cost(filtered: &[u8]) -> u64 {
    filtered
        .iter()
        .map(|&b| (b as i8).unsigned_abs() as u64)
        .sum()
}

/// Filter every scanline with the cheapest PNG filter, producing the
/// filter-byte-prefixed stream that IDAT compresses.
fn filter_image(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let stride = (width as usize) * BYTES_PER_PIXEL;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    let zero_row = vec![0u8; stride];
    let mut candidate = vec![0u8; stride];
    let mut best = vec![0u8; stride];
    for row_index in 0..height as usize {
        let start = row_index * stride;
        let row = &rgba[start..start + stride];
        let prev = if row_index == 0 {
            &zero_row[..]
        } else {
            &rgba[start - stride..start]
        };
        let mut best_filter = FILTER_NONE;
        let mut best_cost = u64::MAX;
        for &filter in &FILTERS {
            filter_scanline(filter, row, prev, &mut candidate);
            let cost = residual_cost(&candidate);
            if cost < best_cost {
                best_cost = cost;
                best_filter = filter;
                std::mem::swap(&mut best, &mut candidate);
            }
        }
        raw.push(best_filter);
        raw.extend_from_slice(&best);
    }
    raw
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Encode an 8-bit RGBA buffer (`width*height*4` bytes, row-major top-to-bottom)
/// as a PNG.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth 8, colour type 6 (RGBA)
    write_chunk(&mut out, b"IHDR", &ihdr);

    let raw = filter_image(width, height, rgba);
    write_chunk(&mut out, b"IDAT", &flate_encode(&raw));
    write_chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::inflate::flate_decode;
    use crate::raster::png_decode::decode_png;

    const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    /// The IDAT payload (zlib stream) of a single-IDAT PNG.
    fn idat_payload(png: &[u8]) -> &[u8] {
        let idat_pos = png.windows(4).position(|w| w == b"IDAT").unwrap();
        let len = u32::from_be_bytes([
            png[idat_pos - 4],
            png[idat_pos - 3],
            png[idat_pos - 2],
            png[idat_pos - 1],
        ]) as usize;
        &png[idat_pos + 4..idat_pos + 4 + len]
    }

    /// Deterministic xorshift noise — incompressible-ish, filter-hostile.
    fn noise(len: usize, mut seed: u32) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed >> 24) as u8
            })
            .collect()
    }

    fn gradient(width: u32, height: u32) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[
                    (x * 255 / width.max(1)) as u8,
                    (y * 255 / height.max(1)) as u8,
                    ((x + y) % 256) as u8,
                    255,
                ]);
            }
        }
        rgba
    }

    fn assert_round_trip(width: u32, height: u32, rgba: &[u8]) {
        let png = encode_png(width, height, rgba);
        assert_eq!(&png[0..8], &PNG_SIGNATURE);
        let decoded = decode_png(&png).expect("our own PNG must decode");
        assert_eq!((decoded.width, decoded.height), (width, height));
        assert_eq!(decoded.rgba, rgba, "{width}x{height} must round-trip");
    }

    #[test]
    fn crc_known_value() {
        // CRC-32("123456789") = 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn round_trips_pixel_exact_across_image_kinds() {
        // Flat colour.
        let flat: Vec<u8> = [12u8, 200, 99, 255].repeat(40 * 30);
        assert_round_trip(40, 30, &flat);
        // Smooth gradient (Sub / Up / Average / Paeth territory).
        assert_round_trip(64, 48, &gradient(64, 48));
        // Noise, including random alpha.
        assert_round_trip(37, 23, &noise(37 * 23 * 4, 0x9E37_79B9));
        // A single pixel.
        assert_round_trip(1, 1, &[1, 2, 3, 4]);
        // Odd, prime width and a single row / single column.
        assert_round_trip(13, 7, &noise(13 * 7 * 4, 7));
        assert_round_trip(101, 1, &gradient(101, 1));
        assert_round_trip(1, 57, &gradient(1, 57));
    }

    #[test]
    fn every_filter_type_round_trips_through_the_decoder() {
        // Force each filter on every row, assemble the PNG by hand, and let the
        // independent decoder un-filter it: pins `filter_scanline` for all five.
        let (width, height) = (9u32, 6u32);
        let rgba = noise((width * height * 4) as usize, 0xC0FF_EE11);
        let stride = width as usize * BYTES_PER_PIXEL;
        for &filter in &FILTERS {
            let mut raw = Vec::new();
            let zero = vec![0u8; stride];
            let mut out = vec![0u8; stride];
            for y in 0..height as usize {
                let row = &rgba[y * stride..(y + 1) * stride];
                let prev = if y == 0 {
                    &zero[..]
                } else {
                    &rgba[(y - 1) * stride..y * stride]
                };
                filter_scanline(filter, row, prev, &mut out);
                raw.push(filter);
                raw.extend_from_slice(&out);
            }
            let mut png = PNG_SIGNATURE.to_vec();
            let mut ihdr = Vec::new();
            ihdr.extend_from_slice(&width.to_be_bytes());
            ihdr.extend_from_slice(&height.to_be_bytes());
            ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
            write_chunk(&mut png, b"IHDR", &ihdr);
            write_chunk(&mut png, b"IDAT", &flate_encode(&raw));
            write_chunk(&mut png, b"IEND", &[]);
            let decoded = decode_png(&png).expect("forced-filter PNG decodes");
            assert_eq!(decoded.rgba, rgba, "filter {filter} must invert exactly");
        }
    }

    #[test]
    fn heuristic_picks_a_predictive_filter_for_smooth_content() {
        // A horizontal ramp: every row after the first is identical, so `Up`
        // (all-zero residuals) must beat `None` on rows 1.., and row 0 must not
        // keep `None` when `Sub` turns the ramp into a constant step.
        let rgba = gradient(32, 4);
        let raw = flate_decode(idat_payload(&encode_png(32, 4, &rgba))).unwrap();
        let stride = 32 * 4 + 1;
        let filters: Vec<u8> = (0..4).map(|row| raw[row * stride]).collect();
        assert!(filters.iter().all(|&f| f != FILTER_NONE), "{filters:?}");
    }

    #[test]
    fn idat_is_actually_compressed() {
        // A blank 8.5x11in page at 144 dpi (1224x1584 RGBA = 7.7 MB raw) must
        // shrink to well under 1 % — stored blocks used to make it ~100 %.
        let (width, height) = (1224u32, 1584u32);
        let raw_len = (width * height * 4) as usize;
        let png = encode_png(width, height, &vec![0xFF; raw_len]);
        assert!(
            png.len() * 100 < raw_len,
            "blank page PNG is {} bytes for {raw_len} raw bytes",
            png.len()
        );
        let decoded = decode_png(&png).unwrap();
        assert!(decoded.rgba.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn encode_empty_image_emits_valid_png() {
        // A zero-size image has an empty filtered stream: the zlib encoder must
        // still emit a well-formed (empty) DEFLATE stream.
        let png = encode_png(0, 0, &[]);
        // Still a spec-valid PNG: signature + IDAT chunk present.
        assert_eq!(
            &png[0..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
        assert!(png.windows(4).any(|w| w == b"IDAT"));
        assert!(png.windows(4).any(|w| w == b"IEND"));
    }
}
