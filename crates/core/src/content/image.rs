//! Prepare an input raster (PNG or JPEG) for embedding as a PDF `/Image`
//! XObject.
//!
//! Two strategies, both honouring the engine's zero-dependency rule:
//!
//! * **JPEG** is embedded *verbatim* with the `/DCTDecode` filter — PDF viewers
//!   decode baseline/progressive JPEG natively, so we only parse the SOF marker
//!   for the dimensions and component count. No pixel decoding happens.
//! * **PNG** is decoded to RGBA (see [`crate::raster::png_decode`]), split into a
//!   `/DeviceRGB` colour stream plus an optional `/DeviceGray` soft mask for the
//!   alpha channel, and both are re-compressed with `/FlateDecode`.

use crate::filters::deflate::flate_encode;
use crate::raster::png_decode::decode_png;

/// Colour space of a prepared image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageColor {
    Gray,
    Rgb,
    Cmyk,
}

/// Stream filter to declare for a prepared image's main data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFilter {
    /// Raw JPEG bytes — `/DCTDecode`.
    Dct,
    /// Flate-compressed colour samples — `/FlateDecode`.
    Flate,
}

/// An image decoded/parsed into the pieces a PDF `/Image` XObject needs.
#[derive(Debug)]
pub struct PreparedImage {
    pub width: u32,
    pub height: u32,
    pub color: ImageColor,
    pub filter: ImageFilter,
    /// Main image stream bytes (raw JPEG, or flate-compressed samples).
    pub data: Vec<u8>,
    /// Optional 8-bit `/DeviceGray` soft mask (flate-compressed), same size.
    pub smask: Option<Vec<u8>>,
    /// Adobe CMYK JPEGs store inverted ink values; when true the caller must
    /// add `/Decode [1 0 1 0 1 0 1 0]` so the colours render correctly.
    pub cmyk_invert: bool,
}

/// Detect the format from the magic bytes and prepare the image, or `None` when
/// the bytes are neither a PNG nor a JPEG (or are malformed/unsupported).
pub fn prepare_image(data: &[u8]) -> Option<PreparedImage> {
    if is_jpeg(data) {
        prepare_jpeg(data)
    } else if is_png(data) {
        prepare_png(data)
    } else {
        None
    }
}

fn is_png(data: &[u8]) -> bool {
    data.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
}

fn is_jpeg(data: &[u8]) -> bool {
    data.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// Decode a PNG to RGBA, then split into a `/DeviceRGB` stream and (when the
/// image is not fully opaque) a `/DeviceGray` soft mask. Returns `None` for the
/// PNG variants the decoder rejects (16-bit, interlaced, …).
fn prepare_png(data: &[u8]) -> Option<PreparedImage> {
    let img = decode_png(data)?;
    let pixels = (img.width as usize).checked_mul(img.height as usize)?;
    if img.rgba.len() < pixels * 4 {
        return None;
    }

    let mut rgb = Vec::with_capacity(pixels * 3);
    let mut alpha = Vec::with_capacity(pixels);
    let mut opaque = true;
    for px in img.rgba.chunks_exact(4).take(pixels) {
        rgb.extend_from_slice(&px[..3]);
        alpha.push(px[3]);
        opaque &= px[3] == 0xFF;
    }

    Some(PreparedImage {
        width: img.width,
        height: img.height,
        color: ImageColor::Rgb,
        filter: ImageFilter::Flate,
        data: flate_encode(&rgb),
        smask: if opaque {
            None
        } else {
            Some(flate_encode(&alpha))
        },
        cmyk_invert: false,
    })
}

/// Embed a JPEG verbatim under `/DCTDecode`, reading dimensions and component
/// count from the first Start-Of-Frame marker. Adobe APP14 presence on a
/// 4-component frame flags inverted CMYK.
fn prepare_jpeg(data: &[u8]) -> Option<PreparedImage> {
    let frame = scan_jpeg(data)?;
    let color = match frame.components {
        1 => ImageColor::Gray,
        3 => ImageColor::Rgb,
        4 => ImageColor::Cmyk,
        _ => return None,
    };
    Some(PreparedImage {
        width: frame.width as u32,
        height: frame.height as u32,
        color,
        filter: ImageFilter::Dct,
        data: data.to_vec(),
        smask: None,
        cmyk_invert: color == ImageColor::Cmyk && frame.adobe,
    })
}

struct JpegFrame {
    width: u16,
    height: u16,
    components: u8,
    adobe: bool,
}

/// Walk JPEG marker segments to the first SOF, noting any Adobe APP14 marker.
/// Pure byte parsing with explicit bounds checks — never panics on bad input.
fn scan_jpeg(data: &[u8]) -> Option<JpegFrame> {
    let mut i = 2; // skip SOI (FFD8)
    let mut adobe = false;
    while i + 1 < data.len() {
        // Markers may be preceded by fill bytes (0xFF).
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        i += 2;
        match marker {
            0xFF => continue,               // fill byte
            0xD8 | 0xD9 | 0x01 => continue, // SOI/EOI/TEM: no payload
            0xD0..=0xD7 => continue,        // RSTn: no payload
            _ => {}
        }
        // Every other marker carries a 2-byte big-endian segment length that
        // includes the two length bytes themselves.
        let len = u16::from_be_bytes([*data.get(i)?, *data.get(i + 1)?]) as usize;
        if len < 2 {
            return None;
        }
        let payload = data.get(i + 2..i + len)?;
        match marker {
            // SOF0/1/2/3, 5/6/7, 9/10/11, 13/14/15 — every non-differential and
            // differential frame header shares the same prefix layout. DHP (C4),
            // DAC (CC) are excluded.
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                let height = u16::from_be_bytes([*payload.get(1)?, *payload.get(2)?]);
                let width = u16::from_be_bytes([*payload.get(3)?, *payload.get(4)?]);
                let components = *payload.get(5)?;
                if width == 0 || height == 0 || components == 0 {
                    return None;
                }
                return Some(JpegFrame {
                    width,
                    height,
                    components,
                    adobe,
                });
            }
            0xEE => {
                // APP14 "Adobe" marker.
                if payload.starts_with(b"Adobe") {
                    adobe = true;
                }
            }
            _ => {}
        }
        i += len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::png::encode_png;

    #[test]
    fn prepares_opaque_png_without_smask() {
        // 2x1 fully opaque red/green.
        let rgba = [255, 0, 0, 255, 0, 255, 0, 255];
        let png = encode_png(2, 1, &rgba);
        let prep = prepare_image(&png).expect("png prepared");
        assert_eq!((prep.width, prep.height), (2, 1));
        assert_eq!(prep.color, ImageColor::Rgb);
        assert_eq!(prep.filter, ImageFilter::Flate);
        assert!(prep.smask.is_none(), "fully opaque → no soft mask");
    }

    #[test]
    fn prepares_translucent_png_with_smask() {
        // 2x1 with one semi-transparent pixel.
        let rgba = [10, 20, 30, 128, 40, 50, 60, 255];
        let png = encode_png(2, 1, &rgba);
        let prep = prepare_image(&png).expect("png prepared");
        assert!(prep.smask.is_some(), "alpha present → soft mask");
    }

    #[test]
    fn parses_baseline_jpeg_dimensions() {
        // Minimal baseline JPEG header: SOI + SOF0 declaring 16x8, 3 components.
        let jpeg = [
            0xFF, 0xD8, // SOI
            0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0,
            0, // APP0
            0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x08, 0x00, 0x10, 0x03, 0x01, 0x11, 0x00, 0x02,
            0x11, 0x01, 0x03, 0x11, 0x01, // SOF0 16x8x3
            0xFF, 0xD9, // EOI
        ];
        let prep = prepare_image(&jpeg).expect("jpeg parsed");
        assert_eq!((prep.width, prep.height), (16, 8));
        assert_eq!(prep.color, ImageColor::Rgb);
        assert_eq!(prep.filter, ImageFilter::Dct);
        assert_eq!(prep.data, jpeg, "JPEG embedded verbatim");
    }

    #[test]
    fn rejects_non_image_bytes() {
        assert!(prepare_image(b"not an image at all").is_none());
        assert!(
            prepare_image(&[0xFF, 0xD8, 0xFF]).is_none(),
            "truncated jpeg"
        );
    }
}
