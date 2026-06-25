//! JPEG 2000 (`JPXDecode`) image decoding (ISO/IEC 15444-1, ISO 32000-1 §7.4.9).
//! Pure `std`, zero dependencies — a from-scratch J2K codestream decoder.
//!
//! Scope: the PDF `JPXDecode` profile. It accepts either a raw JPEG 2000
//! **codestream** (`FF 4F` SOC … `FF D9` EOC) or a **JP2 box** wrapper
//! (`jP  `/`ftyp`/`jp2h`/`jp2c`) and decodes it to interleaved image samples.
//!
//! Pipeline (decoder side, mirroring the encoder in reverse):
//! 1. [`container`] — locate the codestream inside an optional JP2 box wrapper.
//! 2. marker parsing — `SIZ`/`COD`/`COC`/`QCD`/`QCC`/`RGN`/`SOT`/`SOD`/`EOC`…
//! 3. tier-2 ([`packet`]) — read packets in progression order, parse their
//!    tag-tree headers, and gather each code-block's coded byte segments.
//! 4. tier-1 ([`t1`], EBCOT) — MQ-decode each code-block's bit-planes
//!    (significance-propagation / magnitude-refinement / cleanup passes) into
//!    quantised coefficients.
//! 5. dequantisation + inverse DWT ([`dwt`], 5/3 reversible or 9/7 irreversible)
//!    per resolution level, then the inverse multi-component transform
//!    (RCT/ICT), DC level-shift and clamp.
//!
//! Output: the decoded raster as packed, MSB-first, byte-row-aligned samples
//! with the components interleaved at the codestream's bit depth — exactly the
//! layout the image sample → RGB path consumes (so a `/ColorSpace` on the image
//! dict drives the final colour conversion, as ISO 32000-1 §7.4.9 permits).

use crate::error::{EngineError, Result};

#[path = "jpx_container.rs"]
mod container;
#[path = "jpx_dwt.rs"]
mod dwt;
#[path = "jpx_markers.rs"]
mod markers;
#[path = "jpx_packet.rs"]
mod packet;
#[path = "jpx_t1.rs"]
mod t1;

#[cfg(test)]
#[path = "jpx_e2e_tests.rs"]
mod e2e_tests;

pub use markers::{CodingStyle, Component, Quantization, Siz, Transform};

/// Decode a `JPXDecode` stream (raw codestream or JP2 box wrapper) into packed,
/// interleaved image samples at the codestream's native bit depth.
///
/// The returned bytes are row-major, each scanline padded up to a whole byte,
/// every `bpc`-bit sample stored MSB-first, and the `Csiz` components
/// interleaved per pixel — the same representation a `FlateDecode` image stream
/// yields, so the downstream `/ColorSpace` mapping applies unchanged.
pub fn jpx_decode(data: &[u8]) -> Result<Vec<u8>> {
    let image = decode_to_image(data)?;
    Ok(image.into_packed_samples())
}

/// A fully decoded JPEG 2000 image: one plane of `i32` samples per component,
/// already level-shifted/clamped to `0..=2^bitdepth-1`.
#[derive(Debug)]
pub(crate) struct Image {
    pub width: usize,
    pub height: usize,
    /// One plane per component, row-major, length `width*height`.
    pub planes: Vec<Vec<i32>>,
    /// Bit depth per component (`Ssiz & 0x7F` + 1).
    pub bit_depths: Vec<u32>,
}

impl Image {
    /// Pack the planes into interleaved, MSB-first, byte-row-aligned samples.
    ///
    /// Uses the first component's bit depth as the packing width (the PDF image
    /// dict's `/BitsPerComponent` describes the same value); all components are
    /// emitted at that width. The common cases (8-bit gray/RGB) pack to plain
    /// bytes.
    fn into_packed_samples(self) -> Vec<u8> {
        let n = self.planes.len().max(1);
        let bpc = self.bit_depths.first().copied().unwrap_or(8).clamp(1, 16);
        let row_bits = self.width * n * bpc as usize;
        let row_bytes = row_bits.div_ceil(8);
        let mut out = vec![0u8; row_bytes * self.height];
        for y in 0..self.height {
            let row_base_bit = (y * row_bytes) * 8;
            for x in 0..self.width {
                for (c, plane) in self.planes.iter().enumerate() {
                    let v = plane[y * self.width + x].clamp(0, (1i64 << bpc) as i32 - 1) as u32;
                    let bit = row_base_bit + (x * n + c) * bpc as usize;
                    write_bits(&mut out, bit, v, bpc);
                }
            }
        }
        out
    }
}

/// Write `width` low bits of `value` MSB-first at absolute bit offset `bit`.
fn write_bits(buf: &mut [u8], bit: usize, value: u32, width: u32) {
    for i in 0..width as usize {
        let src = (value >> (width as usize - 1 - i)) & 1;
        if src == 1 {
            let pos = bit + i;
            let byte = pos / 8;
            let shift = 7 - (pos % 8);
            if byte < buf.len() {
                buf[byte] |= 1 << shift;
            }
        }
    }
}

/// Decode the codestream to component planes (shared by [`jpx_decode`] and tests).
pub(crate) fn decode_to_image(data: &[u8]) -> Result<Image> {
    let codestream = container::find_codestream(data)?;
    let mut dec = markers::Codestream::parse(codestream)?;
    dec.decode()
}

/// A trivial big-endian byte reader used throughout the marker parser.
pub(crate) struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn u8(&mut self) -> Result<u8> {
        let b = self
            .data
            .get(self.pos)
            .copied()
            .ok_or_else(|| EngineError::Filter("jpx: unexpected end of codestream".into()))?;
        self.pos += 1;
        Ok(b)
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(((self.u8()? as u16) << 8) | self.u8()? as u16)
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(((self.u16()? as u32) << 16) | self.u16()? as u32)
    }

    /// Read `n` raw bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| EngineError::Filter("jpx: truncated marker segment".into()))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_bits_packs_msb_first() {
        // Two 4-bit samples 0b1010, 0b0101 -> one byte 0b1010_0101 = 0xA5.
        let mut buf = vec![0u8; 1];
        write_bits(&mut buf, 0, 0b1010, 4);
        write_bits(&mut buf, 4, 0b0101, 4);
        assert_eq!(buf, vec![0xA5]);
    }

    #[test]
    fn write_bits_8bit_is_identity() {
        let mut buf = vec![0u8; 3];
        write_bits(&mut buf, 0, 0x12, 8);
        write_bits(&mut buf, 8, 0x34, 8);
        write_bits(&mut buf, 16, 0x56, 8);
        assert_eq!(buf, vec![0x12, 0x34, 0x56]);
    }

    #[test]
    fn reader_reads_big_endian() {
        let mut r = Reader::new(&[0xFF, 0x4F, 0x00, 0x01, 0x02, 0x03]);
        assert_eq!(r.u16().unwrap(), 0xFF4F);
        assert_eq!(r.u32().unwrap(), 0x0001_0203);
        assert!(r.u8().is_err());
    }
}
