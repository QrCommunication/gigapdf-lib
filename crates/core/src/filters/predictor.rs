//! PDF stream predictors (ISO 32000-1 §7.4.4.4). Pure `std`, zero dependencies.
//!
//! `FlateDecode`/`LZWDecode` streams may carry a `/DecodeParms` dict with a
//! `/Predictor` that the encoder applied *before* compression to make the data
//! more compressible. After inflating, the engine must reverse it or the bytes
//! are wrong — scrambled xref entries, or mixed/illegible image pixels.
//!
//! Two families are defined:
//!   * `/Predictor 2` — TIFF Predictor 2 (horizontal differencing).
//!   * `/Predictor 10..=15` — PNG predictors, each row prefixed by a filter-type
//!     byte (None/Sub/Up/Average/Paeth).
//!
//! Shape comes from `/Columns` (default 1), `/Colors` (default 1) and
//! `/BitsPerComponent` (default 8).

use crate::error::{EngineError, Result};
use crate::object::{Dictionary, Object};

/// Parameters controlling a predictor, read from a `/DecodeParms` dict.
#[derive(Clone, Copy)]
struct PredictorParams {
    predictor: i64,
    colors: i64,
    bits_per_component: i64,
    columns: i64,
}

impl PredictorParams {
    /// Read predictor parameters from a `/DecodeParms` dictionary, applying the
    /// ISO 32000-1 defaults. Returns `None` when `/Predictor` is absent or `< 2`
    /// (1 means "no prediction").
    fn from_dict(dict: &Dictionary) -> Option<Self> {
        let predictor = dict.get(b"Predictor").and_then(Object::as_i64)?;
        if predictor < 2 {
            return None;
        }
        Some(Self {
            predictor,
            colors: dict.get(b"Colors").and_then(Object::as_i64).unwrap_or(1),
            bits_per_component: dict
                .get(b"BitsPerComponent")
                .and_then(Object::as_i64)
                .unwrap_or(8),
            columns: dict.get(b"Columns").and_then(Object::as_i64).unwrap_or(1),
        })
    }

    /// Bytes per pixel: `ceil(colors * bits_per_component / 8)`, at least 1.
    fn bytes_per_pixel(&self) -> usize {
        let bits = self.colors.max(1) * self.bits_per_component.max(1);
        (((bits + 7) / 8) as usize).max(1)
    }

    /// Bytes in one (unfiltered) row: `ceil(colors * bpc * columns / 8)`.
    fn row_bytes(&self) -> usize {
        let bits = self.colors.max(1) * self.bits_per_component.max(1) * self.columns.max(1);
        ((bits + 7) / 8) as usize
    }
}

fn filter_err(msg: &str) -> EngineError {
    EngineError::Filter(msg.to_string())
}

/// Reverse the predictor (if any) named by a `/DecodeParms` dictionary.
///
/// When the dict carries no `/Predictor` (or `/Predictor 1`), `data` is returned
/// unchanged. Otherwise the appropriate inverse (TIFF 2 or PNG 10..=15) runs.
pub fn apply_predictor(params_dict: &Dictionary, data: &[u8]) -> Result<Vec<u8>> {
    let Some(params) = PredictorParams::from_dict(params_dict) else {
        return Ok(data.to_vec());
    };
    let row_bytes = params.row_bytes();
    if row_bytes == 0 {
        return Ok(data.to_vec());
    }
    if params.predictor == 2 {
        return tiff_predictor_2(&params, row_bytes, data);
    }
    if (10..=15).contains(&params.predictor) {
        return png_predictor(&params, row_bytes, data);
    }
    Err(filter_err(&format!(
        "unsupported /Predictor {}",
        params.predictor
    )))
}

/// TIFF Predictor 2: each sample was stored as the difference from the sample
/// one pixel to its left (per component); undo by running addition along each
/// row. Only the byte-aligned component widths (8/16) are reversed here; other
/// widths are returned verbatim (the data is rare and best left untouched rather
/// than mangled).
fn tiff_predictor_2(params: &PredictorParams, row_bytes: usize, data: &[u8]) -> Result<Vec<u8>> {
    let colors = params.colors.max(1) as usize;
    let mut out = data.to_vec();
    match params.bits_per_component {
        8 => {
            for row in out.chunks_mut(row_bytes) {
                for i in colors..row.len() {
                    row[i] = row[i].wrapping_add(row[i - colors]);
                }
            }
        }
        16 => {
            let stride = colors * 2;
            for row in out.chunks_mut(row_bytes) {
                let mut i = stride;
                while i + 1 < row.len() {
                    let left = u16::from(row[i - stride]) << 8 | u16::from(row[i - stride + 1]);
                    let cur = u16::from(row[i]) << 8 | u16::from(row[i + 1]);
                    let sum = cur.wrapping_add(left);
                    row[i] = (sum >> 8) as u8;
                    row[i + 1] = (sum & 0xff) as u8;
                    i += 2;
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// PNG predictors (10..=15): the data is a sequence of rows, each prefixed by a
/// one-byte filter type (0..=4). Reverse the named filter using the previous
/// (already-reconstructed) row, dropping the per-row filter bytes from the
/// output. Trailing bytes that don't form a full row are ignored (lenient, like
/// real readers, rather than failing the whole stream).
fn png_predictor(params: &PredictorParams, row_bytes: usize, data: &[u8]) -> Result<Vec<u8>> {
    let bpp = params.bytes_per_pixel();
    let stride = row_bytes + 1; // +1 filter-type byte per row
    let row_count = data.len() / stride;
    let mut out = vec![0u8; row_count * row_bytes];
    let mut prev = vec![0u8; row_bytes];

    for r in 0..row_count {
        let src = &data[r * stride..r * stride + stride];
        let filter = src[0];
        let cur_in = &src[1..];
        let cur_out = &mut out[r * row_bytes..r * row_bytes + row_bytes];

        for i in 0..row_bytes {
            let raw = cur_in[i];
            let a = if i >= bpp { cur_out[i - bpp] } else { 0 }; // left
            let b = prev[i]; // up
            let c = if i >= bpp { prev[i - bpp] } else { 0 }; // upper-left
            let value = match filter {
                0 => raw,                                                 // None
                1 => raw.wrapping_add(a),                                 // Sub
                2 => raw.wrapping_add(b),                                 // Up
                3 => raw.wrapping_add(((a as u16 + b as u16) / 2) as u8), // Average
                4 => raw.wrapping_add(paeth(a, b, c)),                    // Paeth
                _ => return Err(filter_err("invalid PNG predictor row filter")),
            };
            cur_out[i] = value;
        }
        prev.copy_from_slice(cur_out);
    }
    Ok(out)
}

/// The PNG Paeth predictor function (RFC 2083 / ISO 32000-1 §7.4.4.4): pick the
/// neighbour (left `a`, up `b`, upper-left `c`) closest to `a + b - c`.
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i32 + b as i32 - c as i32;
    let pa = (p - a as i32).abs();
    let pb = (p - b as i32).abs();
    let pc = (p - c as i32).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Object;

    fn params(entries: &[(&[u8], i64)]) -> Dictionary {
        let mut d = Dictionary::new();
        for &(k, v) in entries {
            d.set(k.to_vec(), Object::Integer(v));
        }
        d
    }

    #[test]
    fn no_predictor_returns_verbatim() {
        // Predictor absent → bytes unchanged.
        let d = Dictionary::new();
        assert_eq!(apply_predictor(&d, &[1, 2, 3]).unwrap(), vec![1, 2, 3]);
        // Predictor 1 ("no prediction") → also unchanged.
        let d1 = params(&[(b"Predictor", 1)]);
        assert_eq!(apply_predictor(&d1, &[9, 8]).unwrap(), vec![9, 8]);
    }

    #[test]
    fn png_up_filter_2x2_grayscale() {
        // 2×2, 1 colour, 8 bpc → row_bytes = 2, stride = 3 (filter byte + row).
        // Target image rows: [10, 20] then [13, 25].
        // Row 0 filter None(0): stored verbatim → [10, 20].
        // Row 1 filter Up(2): stored as (target - above) → [13-10, 25-20] = [3, 5].
        let d = params(&[
            (b"Predictor", 12),
            (b"Colors", 1),
            (b"BitsPerComponent", 8),
            (b"Columns", 2),
        ]);
        let encoded = [0, 10, 20, 2, 3, 5];
        assert_eq!(apply_predictor(&d, &encoded).unwrap(), vec![10, 20, 13, 25]);
    }

    #[test]
    fn png_sub_and_paeth_rows_rgb() {
        // 2 pixels wide, 3 colours, 8 bpc → bpp = 3, row_bytes = 6, stride = 7.
        // Row 0 Sub(1): left neighbour is the pixel one to the left (bpp back).
        //   stored [10,20,30, 1,2,3] → out [10,20,30, 11,22,33].
        // Row 1 Paeth(4) against row 0; with stored zeros the Paeth predictor of
        //   (a,b,c) reconstructs to the running prediction. Use raw 0s so each
        //   output equals paeth(left, up, upper-left).
        let d = params(&[
            (b"Predictor", 15),
            (b"Colors", 3),
            (b"BitsPerComponent", 8),
            (b"Columns", 2),
        ]);
        let mut encoded = vec![1u8, 10, 20, 30, 1, 2, 3];
        encoded.extend_from_slice(&[4u8, 0, 0, 0, 0, 0, 0]);
        let out = apply_predictor(&d, &encoded).unwrap();
        // Row 0 reconstructed.
        assert_eq!(&out[0..6], &[10, 20, 30, 11, 22, 33]);
        // Row 1: first pixel = paeth(0, above, 0) = above (b) since p=b; second
        // pixel = paeth(left, above, upper-left).
        let row0 = [10u8, 20, 30, 11, 22, 33];
        let mut expected = [0u8; 6];
        for i in 0..6 {
            let a = if i >= 3 { expected[i - 3] } else { 0 };
            let b = row0[i];
            let c = if i >= 3 { row0[i - 3] } else { 0 };
            expected[i] = paeth(a, b, c);
        }
        assert_eq!(&out[6..12], &expected);
    }

    #[test]
    fn tiff_predictor_2_row_8bpc() {
        // TIFF 2, 1 colour, 8 bpc, 4 columns → row_bytes = 4, no per-row byte.
        // Stored as left-differences of [5, 7, 6, 9] → [5, 2, -1(=255), 3].
        let d = params(&[
            (b"Predictor", 2),
            (b"Colors", 1),
            (b"BitsPerComponent", 8),
            (b"Columns", 4),
        ]);
        let encoded = [5u8, 2, 255, 3];
        assert_eq!(apply_predictor(&d, &encoded).unwrap(), vec![5, 7, 6, 9]);
    }

    #[test]
    fn tiff_predictor_2_rgb_two_rows() {
        // TIFF 2, 3 colours, 8 bpc, 2 columns → row_bytes = 6, differencing is
        // per-component across the row, and resets at each row boundary.
        // Row 0 target [10,20,30, 12,24,36] → stored [10,20,30, 2,4,6].
        // Row 1 target [ 1, 2, 3,  5, 7, 9] → stored [ 1, 2, 3, 4, 5, 6].
        let d = params(&[
            (b"Predictor", 2),
            (b"Colors", 3),
            (b"BitsPerComponent", 8),
            (b"Columns", 2),
        ]);
        let encoded = [10u8, 20, 30, 2, 4, 6, 1, 2, 3, 4, 5, 6];
        assert_eq!(
            apply_predictor(&d, &encoded).unwrap(),
            vec![10, 20, 30, 12, 24, 36, 1, 2, 3, 5, 7, 9]
        );
    }
}
