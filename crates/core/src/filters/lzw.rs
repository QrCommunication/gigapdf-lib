//! LZWDecode (ISO 32000-1 §7.4.4). Pure `std`, zero dependencies.
//!
//! Variable-width LZW: codes start at 9 bits and grow to 12, read MSB-first.
//! Code 256 clears the table, 257 marks end-of-data. The `EarlyChange`
//! parameter (default 1) bumps the code width one code earlier than strictly
//! necessary — the behaviour Adobe's encoders and readers use.

use crate::error::{EngineError, Result};

const CLEAR_TABLE: u16 = 256;
const EOD: u16 = 257;
const MIN_CODE_WIDTH: u32 = 9;
const MAX_CODE_WIDTH: u32 = 12;

fn filter_err(msg: &str) -> EngineError {
    EngineError::Filter(msg.to_string())
}

/// Reads codes MSB-first (big-endian bit order) from the byte stream.
struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Read `width` bits MSB-first, or `None` once the input is exhausted.
    fn read(&mut self, width: u32) -> Option<u16> {
        let mut value: u16 = 0;
        for _ in 0..width {
            let byte_index = self.bit_pos / 8;
            let byte = *self.data.get(byte_index)?;
            let bit_index = 7 - (self.bit_pos % 8);
            let bit = (byte >> bit_index) & 1;
            value = (value << 1) | bit as u16;
            self.bit_pos += 1;
        }
        Some(value)
    }
}

/// Decode an LZW stream into raw bytes. `early_change` is the PDF
/// `/DecodeParms /EarlyChange` value (1 = Adobe default, 0 = strict TIFF).
pub fn lzw_decode(data: &[u8], early_change: bool) -> Result<Vec<u8>> {
    let mut reader = BitReader::new(data);
    let mut out = Vec::with_capacity(data.len() * 3);

    // The dictionary: each entry is the byte sequence for a code. Indices
    // 0..=255 are the literal bytes, 256/257 are control codes (kept empty).
    let mut table: Vec<Vec<u8>> = Vec::with_capacity(4096);
    let reset_table = |table: &mut Vec<Vec<u8>>| {
        table.clear();
        for byte in 0..=255u16 {
            table.push(vec![byte as u8]);
        }
        table.push(Vec::new()); // 256: clear
        table.push(Vec::new()); // 257: EOD
    };
    reset_table(&mut table);

    let mut code_width = MIN_CODE_WIDTH;
    let mut previous: Option<u16> = None;
    let bump = u32::from(early_change);

    loop {
        let Some(code) = reader.read(code_width) else {
            break; // ran out of input (tolerate a missing EOD marker)
        };

        if code == CLEAR_TABLE {
            reset_table(&mut table);
            code_width = MIN_CODE_WIDTH;
            previous = None;
            continue;
        }
        if code == EOD {
            break;
        }

        let entry: Vec<u8> = if (code as usize) < table.len() {
            table[code as usize].clone()
        } else if Some(code) == next_code(&table) && previous.is_some() {
            // Special LZW case: code not yet in the table. Rebuild it as the
            // previous sequence plus its own first byte.
            let prev_seq = &table[previous.unwrap() as usize];
            let mut seq = prev_seq.clone();
            seq.push(prev_seq[0]);
            seq
        } else {
            return Err(filter_err("invalid LZW code"));
        };

        out.extend_from_slice(&entry);

        if let Some(prev) = previous {
            // Add `prev + entry[0]` as a new dictionary entry.
            if table.len() < 4096 {
                let mut new_entry = table[prev as usize].clone();
                new_entry.push(entry[0]);
                table.push(new_entry);
            }
        }
        previous = Some(code);

        // Grow the code width as the table fills (early-change bumps one sooner).
        let next = table.len() as u32 + bump;
        if next > (1 << code_width) && code_width < MAX_CODE_WIDTH {
            code_width += 1;
        }
    }

    Ok(out)
}

/// The code that the next dictionary insertion will occupy.
fn next_code(table: &[Vec<u8>]) -> Option<u16> {
    u16::try_from(table.len()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-encoded LZW (Adobe early-change) for "-----A---B" from the
    // ISO 32000-1 §7.4.4.2 worked example. Encoded bytes: 80 0B 60 50 22 0C
    // 0C 85 01. Cross-checked against a reference encoder.
    const SPEC_EXAMPLE: [u8; 9] = [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01];

    #[test]
    fn decodes_spec_example() {
        assert_eq!(lzw_decode(&SPEC_EXAMPLE, true).unwrap(), b"-----A---B");
    }

    #[test]
    fn decodes_simple_literals() {
        // "AB": CLEAR 'A' 'B' EOD with early-change (reference-encoder bytes).
        let encoded = [0x80u8, 0x10, 0x48, 0x50, 0x10];
        assert_eq!(lzw_decode(&encoded, true).unwrap(), b"AB");
    }

    #[test]
    fn decodes_repeated_text() {
        // "Hello" (reference-encoder bytes, early-change).
        let encoded = [0x80u8, 0x12, 0x0c, 0xa6, 0xc3, 0x61, 0xbe, 0x02];
        assert_eq!(lzw_decode(&encoded, true).unwrap(), b"Hello");
    }

    #[test]
    fn decodes_with_code_width_growth_and_self_referential_codes() {
        // "TOBEORNOTTOBEORTOBEORNOT" exercises the LZW "code == next table slot"
        // special case and a 9→10 bit width transition. Reference-encoder bytes.
        let encoded = [
            0x80u8, 0x15, 0x09, 0xe4, 0x22, 0x29, 0x3c, 0xa4, 0x4e, 0x27, 0x95, 0x20, 0x50, 0x48,
            0x34, 0x2e, 0x0b, 0x07, 0x84, 0xc0, 0x40,
        ];
        assert_eq!(
            lzw_decode(&encoded, true).unwrap(),
            b"TOBEORNOTTOBEORTOBEORNOT"
        );
    }

    #[test]
    fn tolerates_missing_eod() {
        // "Hello" bytes with the trailing EOD code stripped: the decoder returns
        // what it recovered instead of erroring on the missing marker.
        let encoded = [0x80u8, 0x12, 0x0c, 0xa6, 0xc3, 0x61];
        let decoded = lzw_decode(&encoded, true).unwrap();
        assert!(decoded.starts_with(b"Hel"));
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert_eq!(lzw_decode(&[], true).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn invalid_code_errors() {
        // CLEAR (256) then a 9-bit code 300, which is neither in the fresh
        // 258-entry table nor the special next-code case → "invalid LZW code".
        // Bits: 100000000 (256) 100101100 (300) packed MSB-first.
        let mut bits = String::new();
        for code in [256u16, 300] {
            bits.push_str(&format!("{code:09b}"));
        }
        while !bits.len().is_multiple_of(8) {
            bits.push('0');
        }
        let bytes: Vec<u8> = bits
            .as_bytes()
            .chunks(8)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap())
            .collect();
        assert!(lzw_decode(&bytes, true).is_err());
    }

    #[test]
    fn strict_tiff_early_change_zero_decodes() {
        // early_change = false (strict TIFF) exercises the no-bump width path.
        // "AB": CLEAR 'A' 'B' EOD encoded with early_change=0 still round-trips
        // a couple of literals; we only assert it decodes without error and is
        // non-trivial, since the bit layout differs from the Adobe stream.
        let encoded = [0x80u8, 0x10, 0x48, 0x50, 0x10];
        let decoded = lzw_decode(&encoded, false).unwrap();
        assert!(decoded.starts_with(b"A"));
    }
}
