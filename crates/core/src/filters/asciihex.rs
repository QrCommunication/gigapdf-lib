//! ASCIIHexDecode (ISO 32000-1 §7.4.2). Pure `std`, zero dependencies.
//!
//! Each byte is encoded as two hexadecimal digits (high nibble first).
//! Whitespace between digits is ignored, a `>` marks end-of-data, and an odd
//! final digit is treated as if followed by `0`.

use crate::error::{EngineError, Result};

fn filter_err(msg: &str) -> EngineError {
    EngineError::Filter(msg.to_string())
}

/// Map an ASCII hex digit to its 0–15 value, or `None` if not a hex digit.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// PDF whitespace (ISO 32000-1 §7.2.3): space, tab, CR, LF, FF, NUL.
fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | 0x0C | 0x00)
}

/// Decode an ASCIIHex stream into raw bytes.
pub fn ascii_hex_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 2);
    let mut high: Option<u8> = None;

    for &byte in data {
        if byte == b'>' {
            break; // end-of-data marker
        }
        if is_whitespace(byte) {
            continue;
        }
        let nibble = hex_value(byte)
            .ok_or_else(|| filter_err("invalid character in ASCIIHexDecode stream"))?;
        match high.take() {
            None => high = Some(nibble),
            Some(hi) => out.push((hi << 4) | nibble),
        }
    }

    // A dangling high nibble means an odd digit count: low nibble is 0.
    if let Some(hi) = high {
        out.push(hi << 4);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_hex_pairs_ignoring_whitespace() {
        // "48 65 6C 6C 6F" with mixed whitespace and an EOD marker → "Hello".
        let encoded = b"48 65\t6C\n6C6F>garbage-after-eod";
        assert_eq!(ascii_hex_decode(encoded).unwrap(), b"Hello");
    }

    #[test]
    fn odd_trailing_nibble_pads_low_with_zero() {
        // "4D2" → 0x4D, then a lone 0x2 high nibble → 0x20.
        assert_eq!(ascii_hex_decode(b"4D2>").unwrap(), &[0x4D, 0x20]);
    }

    #[test]
    fn lowercase_and_no_eod_marker() {
        assert_eq!(ascii_hex_decode(b"ff00ab").unwrap(), &[0xFF, 0x00, 0xAB]);
    }

    #[test]
    fn rejects_non_hex_character() {
        assert!(ascii_hex_decode(b"4G").is_err());
    }
}
