//! RunLengthDecode (ISO 32000-1 §7.4.5). Pure `std`, zero dependencies.
//!
//! A length byte `n` drives each run:
//! - `0..=127`: the next `n + 1` bytes are copied literally.
//! - `129..=255`: the next single byte is repeated `257 - n` times.
//! - `128`: end-of-data.

use crate::error::{EngineError, Result};

fn filter_err(msg: &str) -> EngineError {
    EngineError::Filter(msg.to_string())
}

/// Decode a RunLength stream into raw bytes.
pub fn run_length_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut i = 0usize;

    while i < data.len() {
        let length = data[i];
        i += 1;
        match length {
            128 => break, // end-of-data
            0..=127 => {
                let run = length as usize + 1;
                let end = i
                    .checked_add(run)
                    .filter(|&e| e <= data.len())
                    .ok_or_else(|| filter_err("RunLength literal run exceeds input"))?;
                out.extend_from_slice(&data[i..end]);
                i = end;
            }
            _ => {
                // 129..=255: repeat the next byte (257 - length) times.
                let count = 257 - length as usize;
                let &byte = data
                    .get(i)
                    .ok_or_else(|| filter_err("RunLength repeat run missing its byte"))?;
                out.extend(std::iter::repeat_n(byte, count));
                i += 1;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_run() {
        // length 4 → copy next 5 bytes literally, then EOD.
        let encoded = [4u8, b'H', b'e', b'l', b'l', b'o', 128];
        assert_eq!(run_length_decode(&encoded).unwrap(), b"Hello");
    }

    #[test]
    fn repeat_run() {
        // 257 - 254 = 3 copies of 'A', then EOD.
        let encoded = [254u8, b'A', 128];
        assert_eq!(run_length_decode(&encoded).unwrap(), b"AAA");
    }

    #[test]
    fn mixed_literal_and_repeat() {
        // "ab" literal (len 1 → 2 bytes), then 'c' × (257-255)=2.
        let encoded = [1u8, b'a', b'b', 255, b'c', 128];
        assert_eq!(run_length_decode(&encoded).unwrap(), b"abcc");
    }

    #[test]
    fn missing_eod_decodes_available_data() {
        let encoded = [0u8, b'X'];
        assert_eq!(run_length_decode(&encoded).unwrap(), b"X");
    }

    #[test]
    fn truncated_literal_run_errors() {
        // Claims 4 bytes but only 2 follow.
        let encoded = [3u8, b'a', b'b'];
        assert!(run_length_decode(&encoded).is_err());
    }
}
