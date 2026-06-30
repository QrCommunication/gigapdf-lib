//! ASCII85Decode (ISO 32000-1 §7.4.3). Pure `std`, zero dependencies.
//!
//! Groups of five base-85 characters (`!`..=`u`) encode four bytes. The single
//! letter `z` is shorthand for four zero bytes. `~>` marks end-of-data, and a
//! final partial group of `n` characters (2–4) yields `n - 1` bytes.

use crate::error::{EngineError, Result};

fn filter_err(msg: &str) -> EngineError {
    EngineError::Filter(msg.to_string())
}

/// PDF whitespace (ISO 32000-1 §7.2.3): space, tab, CR, LF, FF, NUL.
fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | 0x0C | 0x00)
}

/// Flush a group of `count` base-85 digits (already accumulated into `value`)
/// to `count - 1` output bytes. Partial groups are padded with `u` (84) per the
/// spec, which the high-byte truncation then discards.
fn flush_group(value: u32, count: usize, out: &mut Vec<u8>) {
    let bytes = value.to_be_bytes();
    out.extend_from_slice(&bytes[..count - 1]);
}

/// Decode an ASCII85 stream into raw bytes.
pub fn ascii_85_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 4 / 5);
    let mut group = [0u8; 5];
    let mut count = 0usize;

    for &byte in data {
        if byte == b'~' {
            // `~>` end-of-data; tolerate a bare `~` at the very end too.
            break;
        }
        if is_whitespace(byte) {
            continue;
        }
        if byte == b'z' {
            if count != 0 {
                return Err(filter_err("'z' inside an ASCII85 group"));
            }
            out.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        if !(b'!'..=b'u').contains(&byte) {
            return Err(filter_err("invalid character in ASCII85Decode stream"));
        }
        group[count] = byte - b'!';
        count += 1;
        if count == 5 {
            let value = group.iter().fold(0u32, |acc, &digit| {
                acc.wrapping_mul(85).wrapping_add(digit as u32)
            });
            flush_group(value, 5, &mut out);
            count = 0;
        }
    }

    if count == 1 {
        return Err(filter_err("ASCII85 final group has a single character"));
    }
    if count > 0 {
        // Pad the partial group with the maximum digit (84), per the spec.
        let mut value = 0u32;
        for (slot, digit) in group.iter().enumerate() {
            let padded = if slot < count { *digit } else { 84 };
            value = value.wrapping_mul(85).wrapping_add(padded as u32);
        }
        flush_group(value, count, &mut out);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_full_group_plus_partial() {
        // "Hello" (5 bytes) = full group "Hell" (`87cUR`) + partial "o" (`DZ`).
        assert_eq!(ascii_85_decode(b"87cURDZ~>").unwrap(), b"Hello");
    }

    #[test]
    fn z_shortcut_expands_to_four_zeros() {
        // A lone 'z' = four NUL bytes.
        assert_eq!(ascii_85_decode(b"z~>").unwrap(), &[0, 0, 0, 0]);
    }

    #[test]
    fn exact_four_byte_group() {
        // "Man" (3 bytes) encodes to the partial group "9jqo".
        assert_eq!(ascii_85_decode(b"9jqo~>").unwrap(), b"Man");
    }

    #[test]
    fn ignores_whitespace_between_digits() {
        assert_eq!(ascii_85_decode(b"87\ncU\tRDZ~>").unwrap(), b"Hello");
    }

    #[test]
    fn tolerates_missing_eod_marker() {
        // No `~>`: decode everything accumulated so far.
        assert_eq!(ascii_85_decode(b"87cURDZ").unwrap(), b"Hello");
    }

    #[test]
    fn rejects_out_of_range_character() {
        // 'v' is one past the valid range (`!`..=`u`).
        assert!(ascii_85_decode(b"vvvvv~>").is_err());
    }

    #[test]
    fn single_trailing_char_is_error() {
        // `87cUR` is the full group "Hell"; a single extra character leaves a
        // partial group of one, which is malformed per the spec.
        assert!(ascii_85_decode(b"87cUR!~>").is_err());
    }

    #[test]
    fn z_inside_a_group_is_error() {
        // 'z' is only valid at a group boundary; one mid-group (after a digit
        // has started a group) is rejected.
        assert!(ascii_85_decode(b"8z~>").is_err());
    }
}
