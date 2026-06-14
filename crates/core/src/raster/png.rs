//! A minimal, correct PNG encoder — zero dependencies.
//!
//! Output is 8-bit RGBA (colour type 6). The IDAT image data is wrapped in a
//! zlib stream built entirely from **stored** (uncompressed) DEFLATE blocks, so
//! no compressor is needed — only framing + the Adler-32/CRC-32 checksums. The
//! result is a fully spec-valid PNG every viewer reads; compression can be added
//! later without changing the interface.

/// CRC-32 (IEEE, polynomial `0xEDB88320`) over `data`, as PNG chunks require.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Adler-32 over `data`, as the trailing zlib checksum requires.
fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

/// Wrap `data` in a zlib stream using stored (type-0) DEFLATE blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // zlib header: CM=8, no preset dict, default level
    let mut chunks = data.chunks(0xFFFF).peekable();
    if data.is_empty() {
        // A single empty final stored block.
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    }
    while let Some(chunk) = chunks.next() {
        let is_last = chunks.peek().is_none();
        out.push(if is_last { 0x01 } else { 0x00 }); // BFINAL + BTYPE=00
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Encode an 8-bit RGBA buffer (`width*height*4` bytes, row-major top-to-bottom)
/// as a PNG.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth 8, colour type 6 (RGBA)
    write_chunk(&mut out, b"IHDR", &ihdr);

    // Each scanline is prefixed with filter type 0 (None).
    let stride = (width as usize) * 4;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0);
        let start = row * stride;
        raw.extend_from_slice(&rgba[start..start + stride]);
    }
    write_chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    write_chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::inflate::inflate;

    #[test]
    fn crc_and_adler_known_values() {
        // CRC-32("123456789") = 0xCBF43926; Adler-32("Wikipedia") = 0x11E60398.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn png_round_trips_through_inflate() {
        // 2x2 image: red, green / blue, white.
        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 255, // row 0
            0, 0, 255, 255, 255, 255, 255, 255, // row 1
        ];
        let png = encode_png(2, 2, &rgba);
        assert_eq!(&png[0..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

        // Locate the IDAT chunk, strip the 2-byte zlib header + 4-byte adler,
        // and inflate the stored blocks back to the filtered scanlines.
        let idat_pos = png.windows(4).position(|w| w == b"IDAT").unwrap();
        let len = u32::from_be_bytes([
            png[idat_pos - 4],
            png[idat_pos - 3],
            png[idat_pos - 2],
            png[idat_pos - 1],
        ]) as usize;
        let zlib = &png[idat_pos + 4..idat_pos + 4 + len];
        let deflated = &zlib[2..zlib.len() - 4];
        let raw = inflate(deflated).unwrap();
        // Two rows, each: filter byte 0 + 8 RGBA bytes.
        assert_eq!(raw[0], 0);
        assert_eq!(&raw[1..9], &rgba[0..8]);
        assert_eq!(raw[9], 0);
        assert_eq!(&raw[10..18], &rgba[8..16]);
    }
}
