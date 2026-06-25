//! `/Predictor` post-processing for LZWDecode / FlateDecode (ISO 32000-1
//! §7.4.4.4, Table 10). Pure `std`, zero dependencies.
//!
//! A predictor transforms the filtered bytes so they compress better; decoding
//! reverses it. Two families exist:
//! - `2`: TIFF Predictor 2 (horizontal differencing per component).
//! - `10..=15`: PNG predictors, where each row is prefixed by a filter-type byte
//!   (None/Sub/Up/Average/Paeth), exactly as in the PNG format.

use crate::error::{EngineError, Result};

/// `/DecodeParms` values that drive a predictor pass. Defaults match the PDF
/// spec (`Predictor 1`, `Colors 1`, `BitsPerComponent 8`, `Columns 1`).
#[derive(Debug, Clone, Copy)]
pub struct PredictorParams {
    pub predictor: i64,
    pub colors: i64,
    pub bits_per_component: i64,
    pub columns: i64,
}

impl Default for PredictorParams {
    fn default() -> Self {
        Self {
            predictor: 1,
            colors: 1,
            bits_per_component: 8,
            columns: 1,
        }
    }
}

fn filter_err(msg: &str) -> EngineError {
    EngineError::Filter(msg.to_string())
}

/// Bytes per pixel (a "sample group"), rounded up — used as the per-component
/// stride for Sub/Average/Paeth. PDF predictors with sub-byte samples still step
/// by at least one byte.
fn bytes_per_pixel(colors: i64, bits: i64) -> usize {
    let bits_per_pixel = colors.max(1) * bits.max(1);
    ((bits_per_pixel + 7) / 8).max(1) as usize
}

/// Bytes per row for `columns` pixels of `colors`×`bits` samples (rounded up to
/// a byte boundary, as PNG/PDF rows are).
fn bytes_per_row(columns: i64, colors: i64, bits: i64) -> usize {
    let bits_per_row = columns.max(0) * colors.max(1) * bits.max(1);
    ((bits_per_row + 7) / 8) as usize
}

/// Reverse the predictor named by `params` over `data`. `Predictor` ≤ 1 is the
/// identity (data returned unchanged).
pub fn undo_predictor(data: &[u8], params: PredictorParams) -> Result<Vec<u8>> {
    match params.predictor {
        p if p <= 1 => Ok(data.to_vec()),
        2 => undo_tiff_predictor2(data, params),
        10..=15 => undo_png_predictor(data, params),
        other => Err(filter_err(&format!("unsupported /Predictor {other}"))),
    }
}

/// TIFF Predictor 2: each component is the horizontal difference from the
/// same-channel component one pixel to its left (TIFF 6.0 §14). Supported sample
/// depths: 8-bit, 16-bit (big-endian, per ISO 32000-1 image-data byte order), and
/// the sub-byte packed depths 1/2/4 bits. Other depths are rejected rather than
/// guessed.
fn undo_tiff_predictor2(data: &[u8], params: PredictorParams) -> Result<Vec<u8>> {
    let row_len = bytes_per_row(params.columns, params.colors, params.bits_per_component);
    if row_len == 0 {
        return Ok(data.to_vec());
    }
    let colors = params.colors.max(1) as usize;
    let columns = params.columns.max(0) as usize;
    let mut out = data.to_vec();
    match params.bits_per_component {
        8 => {
            for row in out.chunks_mut(row_len) {
                for i in colors..row.len() {
                    row[i] = row[i].wrapping_add(row[i - colors]);
                }
            }
        }
        16 => {
            // Each component is a big-endian u16; difference is per-channel
            // against the sample one pixel (colors*2 bytes) to the left.
            let stride = colors * 2;
            for row in out.chunks_mut(row_len) {
                for i in (stride..row.len()).step_by(2) {
                    if i + 1 >= row.len() {
                        break;
                    }
                    let left = u16::from_be_bytes([row[i - stride], row[i - stride + 1]]);
                    let cur = u16::from_be_bytes([row[i], row[i + 1]]);
                    let val = cur.wrapping_add(left);
                    let [hi, lo] = val.to_be_bytes();
                    row[i] = hi;
                    row[i + 1] = lo;
                }
            }
        }
        1 | 2 | 4 => {
            let bits = params.bits_per_component as usize;
            for row in out.chunks_mut(row_len) {
                undo_tiff_predictor2_subbyte(row, columns, colors, bits);
            }
        }
        other => {
            return Err(filter_err(&format!(
                "TIFF Predictor 2 with {other}-bit components is unsupported"
            )));
        }
    }
    Ok(out)
}

/// TIFF Predictor 2 over sub-byte (1/2/4-bit) samples in one packed row.
/// Samples are stored MSB-first, `colors` interleaved channels per pixel, rows
/// padded to a byte boundary. Each sample is re-summed (modulo `1 << bits`) with
/// the same-channel sample one pixel to its left.
fn undo_tiff_predictor2_subbyte(row: &mut [u8], columns: usize, colors: usize, bits: usize) {
    let mask = (1u16 << bits) - 1;
    let total = columns.saturating_mul(colors);

    let get = |row: &[u8], sample: usize| -> u16 {
        let bit_pos = sample * bits;
        let byte = bit_pos / 8;
        let shift = 8 - bits - (bit_pos % 8);
        ((row[byte] as u16) >> shift) & mask
    };
    let set = |row: &mut [u8], sample: usize, value: u16| {
        let bit_pos = sample * bits;
        let byte = bit_pos / 8;
        let shift = 8 - bits - (bit_pos % 8);
        let clear = !((mask as u8) << shift);
        row[byte] = (row[byte] & clear) | (((value & mask) as u8) << shift);
    };

    // Walk samples left→right; for each, add the same channel one pixel back.
    for sample in colors..total {
        let cur = get(row, sample);
        let left = get(row, sample - colors);
        set(row, sample, cur.wrapping_add(left) & mask);
    }
}

/// PNG predictors: every row starts with a filter-type byte, then `row_len`
/// data bytes. Each filter is reversed against the already-reconstructed
/// previous row (zeros for the first row).
fn undo_png_predictor(data: &[u8], params: PredictorParams) -> Result<Vec<u8>> {
    let row_len = bytes_per_row(params.columns, params.colors, params.bits_per_component);
    if row_len == 0 {
        return Err(filter_err("PNG predictor with zero-length rows"));
    }
    let bpp = bytes_per_pixel(params.colors, params.bits_per_component);
    let stride = row_len + 1; // +1 for the per-row filter-type byte

    let mut out = Vec::with_capacity(data.len());
    let mut previous = vec![0u8; row_len];

    for raw_row in data.chunks(stride) {
        if raw_row.len() < 1 + row_len {
            // A trailing partial row (truncated stream): stop cleanly.
            break;
        }
        let filter_type = raw_row[0];
        let mut current = raw_row[1..1 + row_len].to_vec();

        match filter_type {
            0 => {} // None
            1 => {
                // Sub: add the byte `bpp` to the left.
                for i in bpp..row_len {
                    current[i] = current[i].wrapping_add(current[i - bpp]);
                }
            }
            2 => {
                // Up: add the byte above.
                for i in 0..row_len {
                    current[i] = current[i].wrapping_add(previous[i]);
                }
            }
            3 => {
                // Average: add floor((left + above) / 2).
                for i in 0..row_len {
                    let left = if i >= bpp { current[i - bpp] as u16 } else { 0 };
                    let above = previous[i] as u16;
                    current[i] = current[i].wrapping_add(((left + above) / 2) as u8);
                }
            }
            4 => {
                // Paeth.
                for i in 0..row_len {
                    let left = if i >= bpp { current[i - bpp] } else { 0 };
                    let above = previous[i];
                    let upper_left = if i >= bpp { previous[i - bpp] } else { 0 };
                    current[i] = current[i].wrapping_add(paeth(left, above, upper_left));
                }
            }
            other => {
                return Err(filter_err(&format!("invalid PNG filter type {other}")));
            }
        }

        out.extend_from_slice(&current);
        previous = current;
    }
    Ok(out)
}

/// PNG Paeth predictor (RFC 2083 / PNG spec): pick the neighbour closest to
/// `left + above - upper_left`.
fn paeth(left: u8, above: u8, upper_left: u8) -> u8 {
    let p = left as i32 + above as i32 - upper_left as i32;
    let pa = (p - left as i32).abs();
    let pb = (p - above as i32).abs();
    let pc = (p - upper_left as i32).abs();
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        above
    } else {
        upper_left
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(predictor: i64, colors: i64, bits: i64, columns: i64) -> PredictorParams {
        PredictorParams {
            predictor,
            colors,
            bits_per_component: bits,
            columns,
        }
    }

    #[test]
    fn predictor_one_is_identity() {
        let data = [1u8, 2, 3, 4];
        assert_eq!(undo_predictor(&data, params(1, 1, 8, 4)).unwrap(), data);
    }

    #[test]
    fn tiff_predictor2_horizontal_diff() {
        // 1 row, 4 columns, 1 colour, 8-bit. Original [10,20,30,40] differenced
        // → [10,10,10,10]; undoing the diff restores the original.
        let encoded = [10u8, 10, 10, 10];
        assert_eq!(
            undo_predictor(&encoded, params(2, 1, 8, 4)).unwrap(),
            [10, 20, 30, 40]
        );
    }

    #[test]
    fn tiff_predictor2_multi_component() {
        // 2 columns, 3 colours (RGB), 8-bit. Original row = [R0 G0 B0 R1 G1 B1]
        // = [10,20,30,40,50,60]. Differenced per component → [10,20,30,30,30,30].
        let encoded = [10u8, 20, 30, 30, 30, 30];
        assert_eq!(
            undo_predictor(&encoded, params(2, 3, 8, 2)).unwrap(),
            [10, 20, 30, 40, 50, 60]
        );
    }

    #[test]
    fn png_up_predictor() {
        // 2 rows, 3 columns, 1 colour, 8-bit. Row 0 filter None [10,20,30];
        // row 1 filter Up (2) with deltas [1,1,1] → reconstructs [11,21,31].
        let encoded = [
            0u8, 10, 20, 30, // row 0: None
            2u8, 1, 1, 1, // row 1: Up
        ];
        assert_eq!(
            undo_predictor(&encoded, params(12, 1, 8, 3)).unwrap(),
            [10, 20, 30, 11, 21, 31]
        );
    }

    #[test]
    fn png_sub_predictor() {
        // 1 row, 4 columns, 1 colour, 8-bit. Filter Sub (1): bytes are deltas
        // from the byte one pixel (1 byte) to the left.
        // Original [5,7,9,11] → Sub deltas [5,2,2,2].
        let encoded = [1u8, 5, 2, 2, 2];
        assert_eq!(
            undo_predictor(&encoded, params(11, 1, 8, 4)).unwrap(),
            [5, 7, 9, 11]
        );
    }

    #[test]
    fn tiff_predictor2_16bit_big_endian() {
        // 1 row, 3 columns, 1 colour, 16-bit BE. Original samples
        // [0x0100, 0x0150, 0x0140] differenced per pixel → [0x0100, 0x0050, -0x0010].
        // Encoded (BE): 0x0100, 0x0050, 0xFFF0; undoing restores the originals.
        let encoded = [0x01, 0x00, 0x00, 0x50, 0xFF, 0xF0];
        assert_eq!(
            undo_predictor(&encoded, params(2, 1, 16, 3)).unwrap(),
            [0x01, 0x00, 0x01, 0x50, 0x01, 0x40]
        );
    }

    #[test]
    fn tiff_predictor2_16bit_multi_component() {
        // 2 columns, 2 colours, 16-bit BE. Original
        // [c0=0x1000 c1=0x2000][c0=0x1010 c1=0x1FF0] → per-channel diff
        // [0x1000 0x2000][0x0010 0xFFF0]; undoing restores it.
        let encoded = [0x10, 0x00, 0x20, 0x00, 0x00, 0x10, 0xFF, 0xF0];
        assert_eq!(
            undo_predictor(&encoded, params(2, 2, 16, 2)).unwrap(),
            [0x10, 0x00, 0x20, 0x00, 0x10, 0x10, 0x1F, 0xF0]
        );
    }

    #[test]
    fn tiff_predictor2_4bit_subbyte() {
        // 1 row, 4 columns, 1 colour, 4-bit. Original nibbles [1,3,6,10]
        // differenced → [1,2,3,4]; packed = 0x12, 0x34. Undoing restores
        // [1,3,6,10] = packed 0x13, 0x6A.
        let encoded = [0x12u8, 0x34];
        assert_eq!(
            undo_predictor(&encoded, params(2, 1, 4, 4)).unwrap(),
            [0x13, 0x6A]
        );
    }

    #[test]
    fn tiff_predictor2_1bit_subbyte() {
        // 1 row, 8 columns, 1 colour, 1-bit. Original bits 1,0,0,1,1,1,0,0.
        // Predictor-2 over GF(2) is XOR with the left pixel: deltas
        // 1,1,0,1,0,0,1,0 = 0b11010010 = 0xD2. Undoing restores 0b10011100 = 0x9C.
        let encoded = [0xD2u8];
        assert_eq!(
            undo_predictor(&encoded, params(2, 1, 1, 8)).unwrap(),
            [0x9C]
        );
    }

    #[test]
    fn tiff_predictor2_2bit_subbyte() {
        // 1 row, 4 columns, 1 colour, 2-bit. Original 2-bit samples [1,3,2,0]
        // differenced mod 4 → [1,2,3,2]; packed = 0b01_10_11_10 = 0x6E. Undoing
        // restores [1,3,2,0] = 0b01_11_10_00 = 0x78.
        let encoded = [0x6Eu8];
        assert_eq!(
            undo_predictor(&encoded, params(2, 1, 2, 4)).unwrap(),
            [0x78]
        );
    }

    #[test]
    fn rejects_unknown_predictor() {
        assert!(undo_predictor(&[0u8], params(99, 1, 8, 1)).is_err());
    }

    #[test]
    fn tiff_predictor2_rejects_unsupported_depth() {
        // 12-bit components are still rejected rather than silently mis-decoded.
        assert!(undo_predictor(&[0u8, 0, 0], params(2, 1, 12, 2)).is_err());
    }

    #[test]
    fn paeth_matches_png_reference() {
        // Reference values from the PNG spec worked cases.
        assert_eq!(paeth(0, 0, 0), 0);
        assert_eq!(paeth(10, 20, 5), 20); // p=25; closest is above(20)
    }
}
