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
/// component one pixel to its left. Only the 8-bit case is implemented (the only
/// one seen in real PDFs); other bit depths are rejected rather than guessed.
fn undo_tiff_predictor2(data: &[u8], params: PredictorParams) -> Result<Vec<u8>> {
    if params.bits_per_component != 8 {
        return Err(filter_err(
            "TIFF Predictor 2 with non-8-bit components is unsupported",
        ));
    }
    let row_len = bytes_per_row(params.columns, params.colors, 8);
    if row_len == 0 {
        return Ok(data.to_vec());
    }
    let colors = params.colors.max(1) as usize;
    let mut out = data.to_vec();
    for row in out.chunks_mut(row_len) {
        for i in colors..row.len() {
            row[i] = row[i].wrapping_add(row[i - colors]);
        }
    }
    Ok(out)
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
    fn rejects_unknown_predictor() {
        assert!(undo_predictor(&[0u8], params(99, 1, 8, 1)).is_err());
    }

    #[test]
    fn paeth_matches_png_reference() {
        // Reference values from the PNG spec worked cases.
        assert_eq!(paeth(0, 0, 0), 0);
        assert_eq!(paeth(10, 20, 5), 20); // p=25; closest is above(20)
    }
}
