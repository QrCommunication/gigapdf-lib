//! zlib / DEFLATE decompressor (RFC 1950 + RFC 1951). Pure `std`.
//!
//! PDF content streams are almost always `FlateDecode` (zlib), so the engine
//! implements DEFLATE directly.

use crate::error::{EngineError, Result};

const MAX_BITS: usize = 15;

fn filter_err(msg: &str) -> EngineError {
    EngineError::Filter(msg.to_string())
}

/// Reads bits LSB-first from a byte slice, with byte-aligned helpers for the
/// stored-block path.
struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    bit: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte: 0,
            bit: 0,
        }
    }

    fn bit(&mut self) -> Result<u32> {
        let byte = *self
            .data
            .get(self.byte)
            .ok_or_else(|| filter_err("unexpected end of deflate stream"))?;
        let value = (byte >> self.bit) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.byte += 1;
        }
        Ok(value as u32)
    }

    fn bits(&mut self, count: u32) -> Result<u32> {
        let mut value = 0u32;
        for i in 0..count {
            value |= self.bit()? << i;
        }
        Ok(value)
    }

    fn align_to_byte(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.byte += 1;
        }
    }

    fn read_byte(&mut self) -> Result<u8> {
        let byte = *self
            .data
            .get(self.byte)
            .ok_or_else(|| filter_err("unexpected end of stored block"))?;
        self.byte += 1;
        Ok(byte)
    }

    fn read_u16_le(&mut self) -> Result<u16> {
        let lo = self.read_byte()? as u16;
        let hi = self.read_byte()? as u16;
        Ok(lo | (hi << 8))
    }
}

/// A canonical Huffman decoder built from a list of code lengths.
struct Huffman {
    /// Number of codes of each length (index 0 unused).
    count: [u16; MAX_BITS + 1],
    /// Symbols ordered by (length, symbol).
    symbol: Vec<u16>,
}

impl Huffman {
    /// Build from per-symbol code lengths (0 = symbol absent).
    fn from_lengths(lengths: &[u16]) -> Result<Self> {
        let mut count = [0u16; MAX_BITS + 1];
        for &len in lengths {
            if len as usize > MAX_BITS {
                return Err(filter_err("code length exceeds 15"));
            }
            count[len as usize] += 1;
        }

        // Offsets of each length's first symbol within `symbol`.
        let mut offsets = [0u16; MAX_BITS + 1];
        for len in 1..MAX_BITS {
            offsets[len + 1] = offsets[len] + count[len];
        }

        let total: usize = (1..=MAX_BITS).map(|l| count[l] as usize).sum();
        let mut symbol = vec![0u16; total];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let slot = &mut offsets[len as usize];
                symbol[*slot as usize] = sym as u16;
                *slot += 1;
            }
        }

        Ok(Self { count, symbol })
    }

    /// Decode one symbol from the bit stream (RFC 1951 canonical decode).
    fn decode(&self, reader: &mut BitReader) -> Result<u16> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_BITS {
            code |= reader.bit()? as i32;
            let count = self.count[len] as i32;
            if code - count < first {
                return Ok(self.symbol[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(filter_err("invalid Huffman code"))
    }
}

// Length and distance base values + extra bits (RFC 1951 §3.2.5).
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit = [0u16; 288];
    for (i, slot) in lit.iter_mut().enumerate() {
        *slot = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let dist = [5u16; 30];
    (
        Huffman::from_lengths(&lit).expect("fixed lit table"),
        Huffman::from_lengths(&dist).expect("fixed dist table"),
    )
}

fn dynamic_tables(reader: &mut BitReader) -> Result<(Huffman, Huffman)> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let hlit = reader.bits(5)? as usize + 257;
    let hdist = reader.bits(5)? as usize + 1;
    let hclen = reader.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err(filter_err("invalid dynamic table sizes"));
    }

    let mut cl_lengths = [0u16; 19];
    for &slot in ORDER.iter().take(hclen) {
        cl_lengths[slot] = reader.bits(3)? as u16;
    }
    let cl_code = Huffman::from_lengths(&cl_lengths)?;

    let total = hlit + hdist;
    let mut lengths = vec![0u16; total];
    let mut index = 0;
    while index < total {
        let symbol = cl_code.decode(reader)?;
        match symbol {
            0..=15 => {
                lengths[index] = symbol;
                index += 1;
            }
            16 => {
                if index == 0 {
                    return Err(filter_err("repeat code 16 with no previous length"));
                }
                let repeat = 3 + reader.bits(2)? as usize;
                let value = lengths[index - 1];
                for _ in 0..repeat {
                    if index >= total {
                        return Err(filter_err("length repeat overflow"));
                    }
                    lengths[index] = value;
                    index += 1;
                }
            }
            17 => {
                let repeat = 3 + reader.bits(3)? as usize;
                index = fill_zero(&mut lengths, index, repeat, total)?;
            }
            18 => {
                let repeat = 11 + reader.bits(7)? as usize;
                index = fill_zero(&mut lengths, index, repeat, total)?;
            }
            _ => return Err(filter_err("invalid code-length symbol")),
        }
    }

    let lit = Huffman::from_lengths(&lengths[..hlit])?;
    let dist = Huffman::from_lengths(&lengths[hlit..])?;
    Ok((lit, dist))
}

fn fill_zero(lengths: &mut [u16], mut index: usize, repeat: usize, total: usize) -> Result<usize> {
    for _ in 0..repeat {
        if index >= total {
            return Err(filter_err("zero-run overflow"));
        }
        lengths[index] = 0;
        index += 1;
    }
    Ok(index)
}

fn inflate_block_codes(
    reader: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<()> {
    loop {
        let symbol = lit.decode(reader)?;
        match symbol {
            256 => return Ok(()), // end of block
            0..=255 => out.push(symbol as u8),
            257..=285 => {
                let s = (symbol - 257) as usize;
                let length = LENGTH_BASE[s] as usize + reader.bits(LENGTH_EXTRA[s])? as usize;
                let dsym = dist.decode(reader)? as usize;
                if dsym >= DIST_BASE.len() {
                    return Err(filter_err("invalid distance symbol"));
                }
                let distance = DIST_BASE[dsym] as usize + reader.bits(DIST_EXTRA[dsym])? as usize;
                if distance == 0 || distance > out.len() {
                    return Err(filter_err("distance points before output start"));
                }
                let start = out.len() - distance;
                for i in 0..length {
                    let byte = out[start + i];
                    out.push(byte);
                }
            }
            _ => return Err(filter_err("invalid length symbol")),
        }
    }
}

/// Raw DEFLATE (RFC 1951) → bytes.
pub fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut reader = BitReader::new(data);
    let mut out = Vec::new();
    loop {
        // A stream flushed with `Z_SYNC_FLUSH` ends with an empty stored block
        // (`00 00 ff ff`, BFINAL=0) and *no* final block — common in PDF content
        // streams (e.g. the `q`/`Q`/overlay pieces of signed PDFs). zlib's strict
        // mode errors ("unexpected end"), but PDF readers return what was decoded
        // so far. Match that leniency: once the input is exhausted at a block
        // boundary and we have produced output, stop instead of failing the whole
        // page. Mid-block truncation still errors (the read happens inside the
        // block, not here), so genuinely corrupt data is not masked.
        let final_block = match reader.bit() {
            Ok(b) => b,
            Err(_) if !out.is_empty() => break,
            Err(e) => return Err(e),
        };
        let block_type = reader.bits(2)?;
        match block_type {
            0 => {
                reader.align_to_byte();
                let len = reader.read_u16_le()? as usize;
                let _nlen = reader.read_u16_le()?;
                for _ in 0..len {
                    let byte = reader.read_byte()?;
                    out.push(byte);
                }
            }
            1 => {
                let (lit, dist) = fixed_tables();
                inflate_block_codes(&mut reader, &mut out, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut reader)?;
                inflate_block_codes(&mut reader, &mut out, &lit, &dist)?;
            }
            _ => return Err(filter_err("reserved deflate block type")),
        }
        if final_block == 1 {
            break;
        }
    }
    Ok(out)
}

/// zlib (RFC 1950) → bytes. Strips the 2-byte header (and optional preset
/// dictionary) when present, otherwise treats the input as raw DEFLATE.
pub fn flate_decode(data: &[u8]) -> Result<Vec<u8>> {
    let looks_like_zlib = data.len() >= 2
        && (data[0] & 0x0F) == 8
        && (((data[0] as u16) << 8) | data[1] as u16).is_multiple_of(31);

    if looks_like_zlib {
        let mut start = 2;
        if data[1] & 0x20 != 0 {
            start += 4; // FDICT: skip the 4-byte dictionary id
        }
        inflate(&data[start..])
    } else {
        inflate(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_block_roundtrip() {
        // zlib header (78 01) + stored block of "Hi!" + (adler omitted).
        // First byte 0x01 = BFINAL=1, BTYPE=00 (stored). LEN=3, NLEN=~3.
        let blob = [0x78, 0x01, 0x01, 0x03, 0x00, 0xFC, 0xFF, b'H', b'i', b'!'];
        assert_eq!(flate_decode(&blob).unwrap(), b"Hi!");
    }

    #[test]
    fn rejects_truncated_stream() {
        // Truncated mid-stored-block (no output yet) must still error — the
        // leniency only applies at a clean block boundary after some output.
        assert!(inflate(&[0x00]).is_err());
    }

    #[test]
    fn sync_flush_without_final_block_returns_decoded() {
        // `Z_SYNC_FLUSH` ends a stream with an empty stored block
        // (`00 00 ff ff`, BFINAL=0) and no final block — exactly the `q`/`Q`/
        // overlay content pieces of signed PDFs (real bytes from a Free/Adobe
        // FillSign document). zlib errors here; PDF readers return the decoded
        // bytes, and so must we — otherwise the whole page extracts nothing.
        let q = [0x78, 0x9c, 0x2a, 0xe4, 0x02, 0x00, 0x00, 0x00, 0xff, 0xff];
        assert_eq!(flate_decode(&q).unwrap(), b"q\n");

        let overlay = [
            0x78, 0x9c, 0xd2, 0x77, 0x74, 0x71, 0x72, 0x8d, 0x77, 0xcb, 0xcc, 0xc9, 0x09, 0xce,
            0x4c, 0xcf, 0x53, 0x70, 0xf2, 0x75, 0x56, 0xe0, 0x2a, 0x54, 0xe0, 0xd2, 0x77, 0xcb,
            0x35, 0x50, 0x70, 0xc9, 0x57, 0xe0, 0x0a, 0x54, 0xe0, 0x72, 0xf5, 0x75, 0x06, 0x00,
            0x00, 0x00, 0xff, 0xff,
        ];
        assert_eq!(
            flate_decode(&overlay).unwrap(),
            b"/ADBE_FillSign BMC \nq \n/Fm0 Do \nQ \nEMC"
        );
    }
}
