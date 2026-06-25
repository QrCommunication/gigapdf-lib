//! JP2 box container parsing (ISO/IEC 15444-1 Annex I).
//!
//! A `JPXDecode` stream is either a raw JPEG 2000 codestream (starting with the
//! `SOC` marker `FF 4F`) or a JP2 file made of length-prefixed boxes
//! (`jP  `, `ftyp`, `jp2h{ihdr,colr,…}`, `jp2c`). This module returns the bytes
//! of the embedded codestream (the contents of the `jp2c` box) so the rest of
//! the decoder works on a codestream uniformly.

use crate::error::{EngineError, Result};

const SOC: [u8; 2] = [0xFF, 0x4F];
const JP2C: u32 = 0x6A70_3263; // "jp2c" — contiguous codestream box.
const JP2_SIGNATURE: u32 = 0x6A50_2020; // "jP  " — JP2 signature box.

/// Locate the JPEG 2000 codestream within a `JPXDecode` stream.
///
/// Accepts a raw codestream (returned as-is) or a JP2 box wrapper (the `jp2c`
/// box payload is returned). Scanning the box list is bounded by the data.
pub(super) fn find_codestream(data: &[u8]) -> Result<&[u8]> {
    if data.len() >= 2 && data[0..2] == SOC {
        return Ok(data);
    }
    // A JP2 file begins with the 12-byte signature box `jP  `.
    if looks_like_jp2(data) {
        if let Some(cs) = find_box(data, JP2C) {
            return Ok(cs);
        }
        return Err(EngineError::Filter(
            "jpx: JP2 file has no jp2c codestream box".into(),
        ));
    }
    // Last resort: scan for an SOC marker (some producers prepend stray bytes).
    if let Some(off) = data.windows(2).position(|w| w == SOC) {
        return Ok(&data[off..]);
    }
    Err(EngineError::Filter(
        "jpx: no JPEG 2000 codestream (SOC marker) found".into(),
    ))
}

/// A JP2 file opens with a 12-byte signature box: length `0x0000000C`, type
/// `jP  `, content `0x0D0A870A`.
fn looks_like_jp2(data: &[u8]) -> bool {
    if data.len() < 12 {
        return false;
    }
    let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let ty = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    len == 12 && ty == JP2_SIGNATURE
}

/// Walk the top-level box list and return the payload of the first box whose
/// type matches `want`. Box layout (Annex I.4): a 4-byte big-endian length
/// `LBox`, a 4-byte type `TBox`, then the payload. `LBox == 0` means "to end of
/// file"; `LBox == 1` means a 64-bit `XLBox` length follows the type.
fn find_box(data: &[u8], want: u32) -> Option<&[u8]> {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let lbox = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let tbox = u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
        let (header, total) = if lbox == 1 {
            // 64-bit extended length.
            if pos + 16 > data.len() {
                return None;
            }
            let xl = u64::from_be_bytes([
                data[pos + 8],
                data[pos + 9],
                data[pos + 10],
                data[pos + 11],
                data[pos + 12],
                data[pos + 13],
                data[pos + 14],
                data[pos + 15],
            ]);
            (16usize, xl as usize)
        } else if lbox == 0 {
            (8usize, data.len() - pos)
        } else {
            (8usize, lbox as usize)
        };
        if total < header {
            return None;
        }
        let body_start = pos + header;
        let body_end = pos.checked_add(total).unwrap_or(data.len()).min(data.len());
        if tbox == want {
            return Some(&data[body_start..body_end]);
        }
        if total == 0 {
            return None;
        }
        pos = match pos.checked_add(total) {
            Some(p) if p > pos => p,
            _ => return None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_codestream_passthrough() {
        let cs = [0xFFu8, 0x4F, 0xFF, 0x51, 0x00, 0x00];
        assert_eq!(find_codestream(&cs).unwrap(), &cs);
    }

    #[test]
    fn jp2_wrapper_extracts_jp2c() {
        // Minimal JP2: signature box, then a jp2c box wrapping a tiny codestream.
        let mut jp2 = Vec::new();
        // Signature box: len 12, "jP  ", 0x0D0A870A.
        jp2.extend_from_slice(&12u32.to_be_bytes());
        jp2.extend_from_slice(b"jP  ");
        jp2.extend_from_slice(&[0x0D, 0x0A, 0x87, 0x0A]);
        // jp2c box: len = 8 + payload.
        let payload = [0xFFu8, 0x4F, 0xFF, 0x51, 0xAB, 0xCD];
        jp2.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
        jp2.extend_from_slice(b"jp2c");
        jp2.extend_from_slice(&payload);
        assert_eq!(find_codestream(&jp2).unwrap(), &payload);
    }

    #[test]
    fn jp2_wrapper_zero_length_jp2c_runs_to_end() {
        let mut jp2 = Vec::new();
        jp2.extend_from_slice(&12u32.to_be_bytes());
        jp2.extend_from_slice(b"jP  ");
        jp2.extend_from_slice(&[0x0D, 0x0A, 0x87, 0x0A]);
        // jp2c with LBox 0 (to end of file).
        jp2.extend_from_slice(&0u32.to_be_bytes());
        jp2.extend_from_slice(b"jp2c");
        let payload = [0xFFu8, 0x4F, 0xFF, 0x51];
        jp2.extend_from_slice(&payload);
        assert_eq!(find_codestream(&jp2).unwrap(), &payload);
    }

    #[test]
    fn missing_codestream_errors() {
        assert!(find_codestream(&[0u8; 4]).is_err());
    }
}
