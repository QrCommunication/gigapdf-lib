//! Prepare an input raster (PNG, JPEG, WebP, GIF or TIFF) for embedding as a PDF
//! `/Image` XObject.
//!
//! Strategies, all honouring the engine's zero-dependency rule:
//!
//! * **JPEG** is embedded *verbatim* with the `/DCTDecode` filter — PDF viewers
//!   decode baseline/progressive JPEG natively, so we only parse the SOF marker
//!   for the dimensions and component count. No pixel decoding happens. An
//!   optional companion soft mask may be attached (JPEG carries no alpha).
//! * **PNG** is decoded to RGBA (see [`crate::raster::png_decode`]) — every
//!   spec-conformant variant including **16-bit** depths (scaled to 8-bit) and
//!   **Adam7-interlaced** layouts — split into a `/DeviceRGB` colour stream plus
//!   an optional `/DeviceGray` soft mask for the alpha channel, and both are
//!   re-compressed with `/FlateDecode`.
//! * **WebP** ([`crate::raster::webp`]), **GIF** ([`crate::raster::gif`]) and
//!   **TIFF** (the strip reader below — uncompressed / LZW / Deflate) are decoded
//!   to RGBA and lowered through the same `/DeviceRGB` + `/SMask` path as PNG.

use crate::filters::deflate::flate_encode;
use crate::filters::inflate::flate_decode;
use crate::filters::lzw::lzw_decode;
use crate::raster::avif::decode_avif;
use crate::raster::gif::decode_gif;
use crate::raster::png_decode::decode_png;
use crate::raster::webp::decode_webp;

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
/// the bytes are an unsupported/malformed raster.
///
/// Dispatch: JPEG → verbatim `/DCTDecode`; PNG / WebP / GIF / TIFF / AVIF →
/// decoded to RGBA and lowered to a `/DeviceRGB` stream (+ `/DeviceGray`
/// `/SMask` when the source carries transparency).
pub fn prepare_image(data: &[u8]) -> Option<PreparedImage> {
    if is_jpeg(data) {
        prepare_jpeg(data, None)
    } else if is_png(data) {
        prepare_png(data)
    } else if is_webp(data) {
        let (w, h, rgba) = decode_webp(data)?;
        prepare_rgba(w, h, &rgba)
    } else if is_gif(data) {
        let (w, h, rgba) = decode_gif(data)?;
        prepare_rgba(w, h, &rgba)
    } else if is_tiff(data) {
        let (w, h, rgba) = decode_tiff(data)?;
        prepare_rgba(w, h, &rgba)
    } else if is_avif(data) {
        let (w, h, rgba) = decode_avif(data)?;
        prepare_rgba(w, h, &rgba)
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

fn is_webp(data: &[u8]) -> bool {
    data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP"
}

fn is_gif(data: &[u8]) -> bool {
    data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")
}

fn is_tiff(data: &[u8]) -> bool {
    // Little-endian (`II` 0x2A00) or big-endian (`MM` 0x002A) byte-order mark.
    data.starts_with(&[0x49, 0x49, 0x2A, 0x00]) || data.starts_with(&[0x4D, 0x4D, 0x00, 0x2A])
}

/// AVIF/HEIF-still detection: an ISO-BMFF `ftyp` box (at offset 4) carrying an
/// AVIF brand (major or compatible). Mirrors the brand check in
/// [`crate::convert::import`]'s `avif_dims` so the embedder and the importer
/// agree on what counts as AVIF.
fn is_avif(data: &[u8]) -> bool {
    const AVIF_BRANDS: [&[u8; 4]; 4] = [b"avif", b"avis", b"mif1", b"miaf"];
    if data.len() < 16 || &data[4..8] != b"ftyp" {
        return false;
    }
    // Box length bounds the brand list; clamp to the buffer.
    let box_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let end = box_len.clamp(16, data.len());
    // major_brand at offset 8, then minor_version (4), then compatible_brands.
    if AVIF_BRANDS.contains(&&[data[8], data[9], data[10], data[11]]) {
        return true;
    }
    let mut o = 16;
    while o + 4 <= end {
        if AVIF_BRANDS.contains(&&[data[o], data[o + 1], data[o + 2], data[o + 3]]) {
            return true;
        }
        o += 4;
    }
    false
}

/// Lower a decoded RGBA8 buffer (`width * height * 4` bytes, top-to-bottom) into
/// a `/DeviceRGB` `/FlateDecode` stream plus an optional 8-bit `/DeviceGray`
/// `/SMask` carrying the alpha channel (omitted when every pixel is opaque).
/// Shared by the PNG / WebP / GIF / TIFF paths.
fn prepare_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<PreparedImage> {
    let pixels = (width as usize).checked_mul(height as usize)?;
    if width == 0 || height == 0 || rgba.len() < pixels * 4 {
        return None;
    }

    let mut rgb = Vec::with_capacity(pixels * 3);
    let mut alpha = Vec::with_capacity(pixels);
    let mut opaque = true;
    for px in rgba.chunks_exact(4).take(pixels) {
        rgb.extend_from_slice(&px[..3]);
        alpha.push(px[3]);
        opaque &= px[3] == 0xFF;
    }

    Some(PreparedImage {
        width,
        height,
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

/// Decode a PNG to RGBA, then split into a `/DeviceRGB` stream and (when the
/// image is not fully opaque) a `/DeviceGray` soft mask. The decoder accepts
/// every spec-conformant PNG — **including 16-bit depths** (scaled to 8-bit per
/// channel) and **Adam7-interlaced** layouts — so those no longer fail to embed.
fn prepare_png(data: &[u8]) -> Option<PreparedImage> {
    let img = decode_png(data)?;
    prepare_rgba(img.width, img.height, &img.rgba)
}

/// Embed a JPEG verbatim under `/DCTDecode`, reading dimensions and component
/// count from the first Start-Of-Frame marker. Adobe APP14 presence on a
/// 4-component frame flags inverted CMYK.
///
/// JPEG itself carries no alpha; `smask_source`, when given, is any supported
/// raster (PNG/WebP/GIF/TIFF/JPEG) decoded to an **8-bit `/DeviceGray`** soft
/// mask and attached as `/SMask` (its luma is used; a true alpha channel, when
/// present, takes precedence). A mask that fails to decode is ignored rather
/// than failing the whole embed.
fn prepare_jpeg(data: &[u8], smask_source: Option<&[u8]>) -> Option<PreparedImage> {
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
        smask: smask_source
            .and_then(decode_smask_gray)
            .map(|g| flate_encode(&g)),
        cmyk_invert: color == ImageColor::Cmyk && frame.adobe,
    })
}

/// Embed a JPEG verbatim under `/DCTDecode` with a companion soft mask. The mask
/// is any supported raster, decoded to an 8-bit `/DeviceGray` `/SMask` (see
/// [`decode_smask_gray`]). Returns `None` only when the JPEG itself is
/// unparseable; an undecodable mask simply yields no `/SMask`.
pub fn prepare_jpeg_with_smask(jpeg: &[u8], smask: &[u8]) -> Option<PreparedImage> {
    prepare_jpeg(jpeg, Some(smask))
}

/// Decode an arbitrary raster (PNG / WebP / GIF / TIFF / JPEG**) to an 8-bit
/// grayscale buffer suitable for a `/DeviceGray` `/SMask`: the per-pixel alpha
/// when the source has transparency, otherwise the perceptual luma. Returns
/// `None` if the mask source can't be decoded.
///
/// (** A JPEG mask is decoded via the same RGBA path as colour JPEGs is not
/// available here — `prepare_image` keeps JPEG verbatim — so a JPEG mask source
/// is unsupported and yields `None`; PNG/WebP/GIF/TIFF masks are the useful
/// cases since they round-trip to RGBA.)
fn decode_smask_gray(source: &[u8]) -> Option<Vec<u8>> {
    let (w, h, rgba) = if is_png(source) {
        let img = decode_png(source)?;
        (img.width, img.height, img.rgba)
    } else if is_webp(source) {
        decode_webp(source)?
    } else if is_gif(source) {
        decode_gif(source)?
    } else if is_tiff(source) {
        decode_tiff(source)?
    } else if is_avif(source) {
        decode_avif(source)?
    } else {
        return None;
    };
    let pixels = (w as usize).checked_mul(h as usize)?;
    if rgba.len() < pixels * 4 {
        return None;
    }
    let mut gray = Vec::with_capacity(pixels);
    let mut any_translucent = false;
    for px in rgba.chunks_exact(4).take(pixels) {
        if px[3] != 0xFF {
            any_translucent = true;
        }
    }
    for px in rgba.chunks_exact(4).take(pixels) {
        // Prefer a real alpha channel; if the mask is fully opaque, fall back to
        // its luma (a grayscale mask image authored without an alpha channel).
        let v = if any_translucent {
            px[3]
        } else {
            // Rec. 601 luma, integer-rounded.
            ((px[0] as u32 * 299 + px[1] as u32 * 587 + px[2] as u32 * 114 + 500) / 1000) as u8
        };
        gray.push(v);
    }
    Some(gray)
}

// ─── TIFF strip reader ───────────────────────────────────────────────────────
//
// A pragmatic baseline-TIFF decoder (TIFF 6.0) covering the variants real
// documents embed: 8-bit samples, 1 (grayscale) / 3 (RGB) / 4 (RGBA) samples
// per pixel **or** a palette (PhotometricInterpretation 3), strip-organised,
// with Uncompressed (1) / LZW (5) / Deflate (8 or 32946) compression and the
// horizontal-differencing predictor (2). Tiled TIFFs, JPEG-in-TIFF, CMYK,
// floating-point and >8-bit samples are out of scope (→ `None`).

/// Decode a baseline TIFF into `(width, height, rgba)`. `None` for unsupported
/// or malformed input. Exposed `pub(crate)` so the image→PDF / watermark
/// transcode path ([`crate::convert::reverse::embeddable_image`]) can reuse the
/// same TIFF reader the embedder uses, keeping the two entry points in lockstep.
pub(crate) fn decode_tiff_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if !is_tiff(data) {
        return None;
    }
    decode_tiff(data)
}

/// Decode a baseline TIFF into `(width, height, rgba)`. `None` for unsupported
/// or malformed input.
fn decode_tiff(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if data.len() < 8 {
        return None;
    }
    let le = match &data[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let rd16 = |o: usize| -> Option<u16> {
        let b = data.get(o..o + 2)?;
        Some(if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let rd32 = |o: usize| -> Option<u32> {
        let b = data.get(o..o + 4)?;
        Some(if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    };

    // Magic 42 then the offset of the first IFD.
    if rd16(2)? != 42 {
        return None;
    }
    let ifd_off = rd32(4)? as usize;
    let entry_count = rd16(ifd_off)? as usize;

    // Collected tag values (only the ones we use).
    let mut width = 0u32;
    let mut height = 0u32;
    let mut bits_per_sample: Vec<u16> = Vec::new();
    let mut compression = 1u16;
    let mut photometric: u16 = u16::MAX;
    let mut strip_offsets: Vec<u32> = Vec::new();
    let mut strip_byte_counts: Vec<u32> = Vec::new();
    let mut samples_per_pixel = 1u16;
    let mut rows_per_strip = u32::MAX;
    let mut predictor = 1u16;
    let mut extra_samples: Vec<u16> = Vec::new();
    let mut colormap: Vec<u16> = Vec::new();

    // Size in bytes of each TIFF field type (index = type id).
    let type_size = |t: u16| -> usize {
        match t {
            1 | 2 | 6 | 7 => 1, // BYTE/ASCII/SBYTE/UNDEFINED
            3 | 8 => 2,         // SHORT/SSHORT
            4 | 9 | 11 => 4,    // LONG/SLONG/FLOAT
            5 | 10 | 12 => 8,   // RATIONAL/SRATIONAL/DOUBLE
            _ => 0,
        }
    };

    for i in 0..entry_count {
        let e = ifd_off + 2 + i * 12;
        let tag = rd16(e)?;
        let ftype = rd16(e + 2)?;
        let count = rd32(e + 4)? as usize;
        let ts = type_size(ftype);
        if ts == 0 {
            continue; // unknown field type → skip safely
        }
        let total = ts.checked_mul(count)?;
        // The value is inline (≤4 bytes) or at the offset stored in the entry.
        let val_off = if total <= 4 {
            e + 8
        } else {
            rd32(e + 8)? as usize
        };

        // Read `count` integer values of this short/long field.
        let read_ints = |how_many: usize| -> Option<Vec<u32>> {
            let mut v = Vec::with_capacity(how_many);
            for k in 0..how_many {
                let o = val_off + k * ts;
                let n = match ftype {
                    3 | 8 => rd16(o)? as u32,
                    4 | 9 => rd32(o)?,
                    1 | 6 | 7 => *data.get(o)? as u32,
                    _ => return None,
                };
                v.push(n);
            }
            Some(v)
        };

        match tag {
            256 => width = read_ints(1)?[0],
            257 => height = read_ints(1)?[0],
            258 => bits_per_sample = read_ints(count)?.iter().map(|&n| n as u16).collect(),
            259 => compression = read_ints(1)?[0] as u16,
            262 => photometric = read_ints(1)?[0] as u16,
            273 => strip_offsets = read_ints(count)?,
            277 => samples_per_pixel = read_ints(1)?[0] as u16,
            278 => rows_per_strip = read_ints(1)?[0],
            279 => strip_byte_counts = read_ints(count)?,
            317 => predictor = read_ints(1)?[0] as u16,
            320 => colormap = read_ints(count)?.iter().map(|&n| n as u16).collect(),
            338 => extra_samples = read_ints(count)?.iter().map(|&n| n as u16).collect(),
            _ => {}
        }
    }

    if width == 0 || height == 0 || strip_offsets.is_empty() {
        return None;
    }
    let pixel_count = (width as usize).checked_mul(height as usize)?;
    if pixel_count > 64 * 1024 * 1024 {
        return None;
    }
    // Only 8-bit samples are supported.
    if !bits_per_sample.is_empty() && bits_per_sample.iter().any(|&b| b != 8) {
        return None;
    }
    let spp = samples_per_pixel.max(1) as usize;
    if !matches!(spp, 1 | 3 | 4) {
        return None;
    }
    if rows_per_strip == 0 {
        return None;
    }
    if rows_per_strip == u32::MAX {
        rows_per_strip = height; // single strip
    }
    if strip_byte_counts.len() != strip_offsets.len() {
        return None;
    }

    // Decompress + de-predict every strip into one contiguous sample buffer.
    let is_palette = photometric == 3;
    let strip_samples = if is_palette { 1 } else { spp };
    let row_bytes = (width as usize).checked_mul(strip_samples)?;
    let mut samples: Vec<u8> = Vec::with_capacity(row_bytes.saturating_mul(height as usize));

    let mut row0 = 0u32;
    for (idx, (&off, &cnt)) in strip_offsets.iter().zip(&strip_byte_counts).enumerate() {
        let _ = idx;
        let raw = data.get(off as usize..off as usize + cnt as usize)?;
        let mut block = match compression {
            1 => raw.to_vec(),                    // none
            5 => lzw_decode(raw, true).ok()?,     // LZW (TIFF EarlyChange)
            8 | 32946 => flate_decode(raw).ok()?, // zlib/Deflate
            _ => return None,                     // unsupported codec
        };
        let strip_rows = (height - row0).min(rows_per_strip) as usize;
        let expect = row_bytes.checked_mul(strip_rows)?;
        if block.len() < expect {
            return None;
        }
        block.truncate(expect);
        if predictor == 2 {
            undo_horizontal_predictor(&mut block, width as usize, strip_samples, strip_rows);
        }
        samples.extend_from_slice(&block);
        row0 += strip_rows as u32;
        if row0 >= height {
            break;
        }
    }
    if samples.len() < row_bytes.saturating_mul(height as usize) {
        return None;
    }

    // Expand the samples to RGBA8.
    let mut rgba = vec![0u8; pixel_count * 4];
    // `white_is_zero` inverts grayscale (PhotometricInterpretation 0).
    let white_is_zero = photometric == 0;
    // Whether the 4th sample is associated/unassociated alpha (ExtraSamples 1/2).
    let has_alpha = spp == 4 && extra_samples.first().is_some_and(|&e| e == 1 || e == 2);

    for i in 0..pixel_count {
        let (r, g, b, a);
        if is_palette {
            // Palette: one index per pixel into the 3*2^bits ColorMap (16-bit,
            // ordered all-R then all-G then all-B; scale 16→8 by the high byte).
            let idx = *samples.get(i)? as usize;
            let entries = colormap.len() / 3;
            if entries == 0 || idx >= entries {
                return None;
            }
            r = (colormap[idx] >> 8) as u8;
            g = (colormap[entries + idx] >> 8) as u8;
            b = (colormap[2 * entries + idx] >> 8) as u8;
            a = 255;
        } else {
            let base = i * spp;
            match spp {
                1 => {
                    let mut y = *samples.get(base)?;
                    if white_is_zero {
                        y = 255 - y;
                    }
                    r = y;
                    g = y;
                    b = y;
                    a = 255;
                }
                3 => {
                    r = *samples.get(base)?;
                    g = *samples.get(base + 1)?;
                    b = *samples.get(base + 2)?;
                    a = 255;
                }
                _ => {
                    r = *samples.get(base)?;
                    g = *samples.get(base + 1)?;
                    b = *samples.get(base + 2)?;
                    a = if has_alpha {
                        *samples.get(base + 3)?
                    } else {
                        255
                    };
                }
            }
        }
        let o = i * 4;
        rgba[o] = r;
        rgba[o + 1] = g;
        rgba[o + 2] = b;
        rgba[o + 3] = a;
    }

    Some((width, height, rgba))
}

/// Undo TIFF horizontal differencing (Predictor 2) in place: each sample is the
/// running sum along its row, per component (`spp` interleaved samples/pixel).
fn undo_horizontal_predictor(buf: &mut [u8], width: usize, spp: usize, rows: usize) {
    let row_bytes = width * spp;
    for r in 0..rows {
        let start = r * row_bytes;
        for x in 1..width {
            for c in 0..spp {
                let cur = start + x * spp + c;
                let prev = start + (x - 1) * spp + c;
                if cur < buf.len() && prev < buf.len() {
                    buf[cur] = buf[cur].wrapping_add(buf[prev]);
                }
            }
        }
    }
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

    // ── PNG forge helpers (16-bit / interlaced are NOT producible by the 8-bit
    //    `encode_png`, so build spec-conformant bytes by hand) ──────────────

    fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_in = Vec::with_capacity(4 + data.len());
        crc_in.extend_from_slice(kind);
        crc_in.extend_from_slice(data);
        out.extend_from_slice(&crate::raster::png::crc32(&crc_in).to_be_bytes());
    }

    /// zlib stream of stored (uncompressed) DEFLATE blocks + Adler-32 trailer.
    fn zlib_store(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let mut i = 0;
        loop {
            let n = (data.len() - i).min(0xFFFF);
            let last = i + n >= data.len();
            out.push(u8::from(last));
            out.extend_from_slice(&(n as u16).to_le_bytes());
            out.extend_from_slice(&(!(n as u16)).to_le_bytes());
            out.extend_from_slice(&data[i..i + n]);
            i += n;
            if last {
                break;
            }
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn make_png(w: u32, h: u32, depth: u8, color_type: u8, interlace: u8, idat: &[u8]) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[depth, color_type, 0, 0, interlace]);
        png_chunk(&mut out, b"IHDR", &ihdr);
        png_chunk(&mut out, b"IDAT", &zlib_store(idat));
        png_chunk(&mut out, b"IEND", &[]);
        out
    }

    // ── #49: 16-bit + interlaced PNGs embed (no longer rejected) ───────────

    #[test]
    fn prepares_16bit_png_for_embedding() {
        // 1×1 16-bit RGB → /DeviceRGB, high byte kept (0x12,0xAB,0x00).
        let row = vec![0x00, 0x12, 0x34, 0xAB, 0xCD, 0x00, 0xFF];
        let png = make_png(1, 1, 16, 2, 0, &row);
        let prep = prepare_image(&png).expect("16-bit PNG must embed");
        assert_eq!((prep.width, prep.height), (1, 1));
        assert_eq!(prep.color, ImageColor::Rgb);
        assert_eq!(prep.filter, ImageFilter::Flate);
        // Decode the flate stream back to confirm the 16→8 reduction.
        let rgb = flate_decode(&prep.data).expect("flate");
        assert_eq!(&rgb, &[0x12, 0xAB, 0x00]);
        assert!(prep.smask.is_none(), "opaque RGB → no smask");
    }

    #[test]
    fn prepares_interlaced_png_for_embedding() {
        // 8×8 Adam7-interlaced 8-bit RGBA must embed (previously claimed rejected).
        let mut rgba = Vec::new();
        for y in 0u32..8 {
            for x in 0u32..8 {
                rgba.extend_from_slice(&[(x * 32) as u8, (y * 32) as u8, 0, 255]);
            }
        }
        // Forge the 7 Adam7 passes as filter-0 scanlines.
        const ADAM7: [(u32, u32, u32, u32); 7] = [
            (0, 0, 8, 8),
            (4, 0, 8, 8),
            (0, 4, 4, 8),
            (2, 0, 4, 4),
            (0, 2, 2, 4),
            (1, 0, 2, 2),
            (0, 1, 1, 2),
        ];
        let pc = |extent: u32, start: u32, step: u32| -> u32 {
            if extent <= start {
                0
            } else {
                (extent - start).div_ceil(step)
            }
        };
        let mut idat = Vec::new();
        for &(x0, y0, dx, dy) in &ADAM7 {
            let pw = pc(8, x0, dx);
            let ph = pc(8, y0, dy);
            for py in 0..ph {
                idat.push(0u8);
                for px in 0..pw {
                    let sx = x0 + px * dx;
                    let sy = y0 + py * dy;
                    let base = (sy as usize * 8 + sx as usize) * 4;
                    idat.extend_from_slice(&rgba[base..base + 4]);
                }
            }
        }
        let png = make_png(8, 8, 8, 6, 1, &idat);
        let prep = prepare_image(&png).expect("interlaced PNG must embed");
        assert_eq!((prep.width, prep.height), (8, 8));
        assert_eq!(prep.color, ImageColor::Rgb);
    }

    // ── #50: WebP / GIF / TIFF dispatch ────────────────────────────────────

    #[test]
    fn prepares_webp_via_dispatch() {
        // Round-trip a 2×2 RGBA through the engine's own lossless WebP encoder,
        // then prepare it for embedding.
        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 255, // row 0
            0, 0, 255, 255, 255, 255, 0, 255, // row 1
        ];
        let webp = crate::raster::webp::encode_webp(2, 2, &rgba);
        assert!(is_webp(&webp), "encoder must produce a WebP");
        let prep = prepare_image(&webp).expect("WebP must embed");
        assert_eq!((prep.width, prep.height), (2, 2));
        assert_eq!(prep.color, ImageColor::Rgb);
        assert_eq!(prep.filter, ImageFilter::Flate);
        let rgb = flate_decode(&prep.data).expect("flate");
        assert_eq!(&rgb[0..3], &[255, 0, 0], "first pixel red survives");
    }

    /// A minimal 2×2 GIF (red/green/blue/white GCT, indices 0,1,2,3).
    fn sample_gif() -> Vec<u8> {
        let mut g = Vec::new();
        g.extend_from_slice(b"GIF89a");
        g.extend_from_slice(&[2, 0, 2, 0]); // 2×2
        g.push(0x80 | 0x01); // GCT, size 2 → 4 colours
        g.extend_from_slice(&[0, 0]); // bg, aspect
        g.extend_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
        g.push(0x2C);
        g.extend_from_slice(&[0, 0, 0, 0, 2, 0, 2, 0, 0x00]);
        let min = 2u8;
        let codes: Vec<(u16, u8)> = vec![(4, 3), (0, 3), (1, 3), (2, 3), (3, 4), (5, 4)];
        let mut bits = Vec::new();
        let (mut acc, mut nb) = (0u32, 0u32);
        for (c, sz) in codes {
            acc |= (c as u32) << nb;
            nb += sz as u32;
            while nb >= 8 {
                bits.push((acc & 0xFF) as u8);
                acc >>= 8;
                nb -= 8;
            }
        }
        if nb > 0 {
            bits.push((acc & 0xFF) as u8);
        }
        g.push(min);
        g.push(bits.len() as u8);
        g.extend_from_slice(&bits);
        g.push(0x00);
        g.push(0x3B);
        g
    }

    #[test]
    fn prepares_gif_via_dispatch() {
        let gif = sample_gif();
        assert!(is_gif(&gif));
        let prep = prepare_image(&gif).expect("GIF must embed");
        assert_eq!((prep.width, prep.height), (2, 2));
        assert_eq!(prep.color, ImageColor::Rgb);
        let rgb = flate_decode(&prep.data).expect("flate");
        assert_eq!(&rgb[0..3], &[255, 0, 0], "GIF index 0 = red");
        assert_eq!(&rgb[3..6], &[0, 255, 0], "GIF index 1 = green");
    }

    // ── TIFF forge: little-endian, one IFD, single strip ───────────────────

    /// Build a minimal little-endian baseline TIFF.
    /// `tags` are `(tag, field_type, count, value_or_offset_bytes)` where, for
    /// inline values (≤4 bytes), the 4-byte little-endian value is given; for
    /// out-of-line arrays the strip data is appended and the offset substituted.
    fn make_tiff_le(entries: &[(u16, u16, u32, u32)], strip: &[u8], strip_off_tag: u16) -> Vec<u8> {
        // Header: II 42, IFD at offset 8.
        let mut out: Vec<u8> = vec![b'I', b'I', 0x2A, 0x00, 8, 0, 0, 0];
        let n = entries.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        // The strip data goes right after the IFD; compute its offset.
        let ifd_bytes = 2 + entries.len() * 12 + 4;
        let strip_off = (8 + ifd_bytes) as u32;
        for &(tag, ftype, count, val) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&ftype.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            // The StripOffsets tag's value is the computed strip offset.
            let v = if tag == strip_off_tag { strip_off } else { val };
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0, 0, 0]); // next-IFD offset = 0
        out.extend_from_slice(strip);
        out
    }

    #[test]
    fn prepares_uncompressed_grayscale_tiff() {
        // 2×2 8-bit grayscale, BlackIsZero, uncompressed.
        let strip = [10u8, 20, 30, 40];
        let entries = [
            (256u16, 3u16, 1u32, 2), // ImageWidth = 2
            (257, 3, 1, 2),          // ImageLength = 2
            (258, 3, 1, 8),          // BitsPerSample = 8
            (259, 3, 1, 1),          // Compression = none
            (262, 3, 1, 1),          // Photometric = BlackIsZero
            (273, 4, 1, 0),          // StripOffsets (filled in)
            (277, 3, 1, 1),          // SamplesPerPixel = 1
            (278, 3, 1, 2),          // RowsPerStrip = 2
            (279, 4, 1, 4),          // StripByteCounts = 4
        ];
        let tiff = make_tiff_le(&entries, &strip, 273);
        assert!(is_tiff(&tiff));
        let prep = prepare_image(&tiff).expect("grayscale TIFF must embed");
        assert_eq!((prep.width, prep.height), (2, 2));
        let rgb = flate_decode(&prep.data).expect("flate");
        // Grayscale 10 → (10,10,10), etc.
        assert_eq!(&rgb[0..3], &[10, 10, 10]);
        assert_eq!(&rgb[3..6], &[20, 20, 20]);
        assert_eq!(&rgb[9..12], &[40, 40, 40]);
        assert!(prep.smask.is_none(), "opaque TIFF → no smask");
    }

    #[test]
    fn prepares_rgb_tiff() {
        // 2×1 8-bit RGB, uncompressed. BitsPerSample is kept as a single inline
        // SHORT (8): a real count-3 array would go out-of-line, and the decoder
        // already treats an empty/8 BitsPerSample as 8-bit, taking the channel
        // count from SamplesPerPixel.
        let strip = [255u8, 0, 0, 0, 255, 0]; // red, green
        let entries = [
            (256u16, 3u16, 1u32, 2),
            (257, 3, 1, 1),
            (258, 3, 1, 8), // BitsPerSample = 8
            (259, 3, 1, 1),
            (262, 3, 1, 2), // RGB
            (273, 4, 1, 0),
            (277, 3, 1, 3),
            (278, 3, 1, 1),
            (279, 4, 1, 6),
        ];
        let tiff = make_tiff_le(&entries, &strip, 273);
        let prep = prepare_image(&tiff).expect("RGB TIFF must embed");
        assert_eq!((prep.width, prep.height), (2, 1));
        let rgb = flate_decode(&prep.data).expect("flate");
        assert_eq!(&rgb, &[255, 0, 0, 0, 255, 0]);
    }

    #[test]
    fn prepares_rgba_tiff_with_smask() {
        // 1×2 8-bit RGBA with ExtraSamples=2 (unassociated alpha) → smask emitted.
        let strip = [255u8, 0, 0, 128, 0, 255, 0, 255]; // red@a=128, green@a=255
        let entries = [
            (256u16, 3u16, 1u32, 1),
            (257, 3, 1, 2),
            (258, 3, 1, 8),
            (259, 3, 1, 1),
            (262, 3, 1, 2), // RGB
            (273, 4, 1, 0),
            (277, 3, 1, 4), // 4 samples/pixel
            (278, 3, 1, 2),
            (279, 4, 1, 8),
            (338, 3, 1, 2), // ExtraSamples = unassociated alpha
        ];
        let tiff = make_tiff_le(&entries, &strip, 273);
        let prep = prepare_image(&tiff).expect("RGBA TIFF must embed");
        assert!(prep.smask.is_some(), "alpha → smask");
        let alpha = flate_decode(prep.smask.as_ref().unwrap()).expect("flate");
        assert_eq!(alpha, vec![128, 255]);
    }

    #[test]
    fn prepares_deflate_tiff_with_predictor() {
        // 4×1 8-bit grayscale, Deflate (8), Predictor 2 (horizontal differencing).
        // Original samples: 10, 20, 25, 60. Differenced: 10, 10, 5, 35.
        let differenced = [10u8, 10, 5, 35];
        let strip = flate_encode(&differenced);
        let entries = [
            (256u16, 3u16, 1u32, 4),
            (257, 3, 1, 1),
            (258, 3, 1, 8),
            (259, 3, 1, 8), // Deflate
            (262, 3, 1, 1), // BlackIsZero
            (273, 4, 1, 0),
            (277, 3, 1, 1),
            (278, 3, 1, 1),
            (279, 4, 1, strip.len() as u32),
            (317, 3, 1, 2), // Predictor = horizontal differencing
        ];
        let tiff = make_tiff_le(&entries, &strip, 273);
        let prep = prepare_image(&tiff).expect("deflate+predictor TIFF must embed");
        assert_eq!((prep.width, prep.height), (4, 1));
        let rgb = flate_decode(&prep.data).expect("flate");
        // De-predicted grayscale → (10,20,25,60) each as (g,g,g).
        assert_eq!(rgb[0], 10);
        assert_eq!(rgb[3], 20);
        assert_eq!(rgb[6], 25);
        assert_eq!(rgb[9], 60);
    }

    #[test]
    fn prepares_palette_tiff() {
        // 2×1 palette (Photometric 3), indices 0,1 into a 2-entry ColorMap.
        // ColorMap is 16-bit, all-R then all-G then all-B (entries=2 → 6 shorts).
        // R = [0xFFFF, 0x0000], G = [0x0000, 0xFFFF], B = [0x0000, 0x0000]
        // → index 0 = red, index 1 = green.
        let mut cmap = Vec::new();
        for v in [0xFFFFu16, 0x0000, 0x0000, 0xFFFF, 0x0000, 0x0000] {
            cmap.extend_from_slice(&v.to_le_bytes());
        }
        // The strip (pixel indices) and the colormap both go out-of-line. Lay the
        // colormap right after the strip and point tag 320 at it.
        let strip = [0u8, 1];
        // Build header + IFD manually because two out-of-line arrays are needed.
        let entries_meta: [(u16, u16, u32); 10] = [
            (256, 3, 1),
            (257, 3, 1),
            (258, 3, 1),
            (259, 3, 1),
            (262, 3, 1),
            (273, 4, 1), // StripOffsets
            (277, 3, 1),
            (278, 3, 1),
            (279, 4, 1),
            (320, 3, 6), // ColorMap (6 shorts)
        ];
        let ifd_bytes = 2 + entries_meta.len() * 12 + 4;
        let strip_off = (8 + ifd_bytes) as u32;
        let cmap_off = strip_off + strip.len() as u32;
        let vals: [u32; 10] = [
            2,         // width
            1,         // length
            8,         // bits
            1,         // compression none
            3,         // palette
            strip_off, // strip offset
            1,         // samples/pixel
            1,         // rows/strip
            2,         // strip byte counts
            cmap_off,  // colormap offset
        ];
        let mut out: Vec<u8> = vec![b'I', b'I', 0x2A, 0x00, 8, 0, 0, 0];
        out.extend_from_slice(&(entries_meta.len() as u16).to_le_bytes());
        for (i, &(tag, ft, c)) in entries_meta.iter().enumerate() {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&ft.to_le_bytes());
            out.extend_from_slice(&c.to_le_bytes());
            out.extend_from_slice(&vals[i].to_le_bytes());
        }
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&strip);
        out.extend_from_slice(&cmap);

        let prep = prepare_image(&out).expect("palette TIFF must embed");
        assert_eq!((prep.width, prep.height), (2, 1));
        let rgb = flate_decode(&prep.data).expect("flate");
        assert_eq!(&rgb[0..3], &[255, 0, 0], "index 0 = red");
        assert_eq!(&rgb[3..6], &[0, 255, 0], "index 1 = green");
    }

    // ── #51: JPEG soft mask ────────────────────────────────────────────────

    fn baseline_jpeg_16x8() -> Vec<u8> {
        vec![
            0xFF, 0xD8, // SOI
            0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0,
            0, // APP0
            0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x08, 0x00, 0x10, 0x03, 0x01, 0x11, 0x00, 0x02,
            0x11, 0x01, 0x03, 0x11, 0x01, // SOF0 16x8x3
            0xFF, 0xD9, // EOI
        ]
    }

    #[test]
    fn jpeg_with_smask_attaches_gray_alpha() {
        // A 16×8 JPEG plus a translucent PNG soft mask of the same size: the mask
        // alpha becomes the /DeviceGray /SMask.
        let jpeg = baseline_jpeg_16x8();
        // Build a 16×8 RGBA PNG whose alpha is a known ramp (0,17,34,…) — the
        // colour is irrelevant, only alpha matters for the mask.
        let (w, h) = (16u32, 8u32);
        let mut rgba = Vec::new();
        for i in 0..(w * h) {
            let a = (i % 256) as u8;
            rgba.extend_from_slice(&[100, 100, 100, a]);
        }
        let mask_png = encode_png(w, h, &rgba);
        let prep = prepare_jpeg_with_smask(&jpeg, &mask_png).expect("jpeg+smask");
        assert_eq!((prep.width, prep.height), (16, 8));
        assert_eq!(prep.filter, ImageFilter::Dct, "JPEG stays /DCTDecode");
        assert_eq!(prep.data, jpeg, "JPEG bytes verbatim");
        let gray = flate_decode(prep.smask.as_ref().expect("smask present")).expect("flate");
        assert_eq!(gray.len(), (w * h) as usize, "one gray byte per pixel");
        // The mask's alpha ramp must be carried through verbatim.
        let expected: Vec<u8> = (0..(w * h)).map(|i| (i % 256) as u8).collect();
        assert_eq!(gray, expected);
    }

    #[test]
    fn jpeg_with_opaque_mask_uses_luma() {
        // A fully-opaque grayscale mask (no alpha) → its luma is used.
        let jpeg = baseline_jpeg_16x8();
        // 16×8 opaque image, all mid-gray (128) → luma 128.
        let (w, h) = (16u32, 8u32);
        let rgba: Vec<u8> = (0..(w * h)).flat_map(|_| [128u8, 128, 128, 255]).collect();
        let mask_png = encode_png(w, h, &rgba);
        let prep = prepare_jpeg_with_smask(&jpeg, &mask_png).expect("jpeg+smask");
        let gray = flate_decode(prep.smask.as_ref().expect("smask")).expect("flate");
        assert!(gray.iter().all(|&v| v == 128), "opaque gray luma = 128");
    }

    #[test]
    fn jpeg_without_smask_has_none() {
        // The single-arg entry point keeps the historical no-smask behaviour.
        let prep = prepare_image(&baseline_jpeg_16x8()).expect("jpeg");
        assert!(prep.smask.is_none());
    }

    #[test]
    fn jpeg_with_undecodable_mask_keeps_jpeg() {
        // A garbage mask must not fail the embed — just no /SMask.
        let jpeg = baseline_jpeg_16x8();
        let prep = prepare_jpeg_with_smask(&jpeg, b"not an image").expect("jpeg still embeds");
        assert!(prep.smask.is_none(), "undecodable mask → no smask");
        assert_eq!(prep.data, jpeg);
    }

    #[test]
    fn is_avif_detects_real_fixture_and_rejects_others() {
        let avif = include_bytes!("../raster/fixtures/av1test.avif");
        assert!(is_avif(avif), "the AVIF fixture is detected");
        // PNG / JPEG / GIF / WebP / TIFF / garbage are NOT AVIF.
        assert!(!is_avif(&encode_png(2, 2, &[0u8; 16])));
        assert!(!is_avif(&baseline_jpeg_16x8()));
        assert!(!is_avif(b"GIF89a............"));
        assert!(!is_avif(b"RIFF....WEBP........"));
        assert!(!is_avif(b"not an image at all"));
        assert!(!is_avif(b""));
    }

    #[test]
    fn prepare_image_embeds_avif_via_rgba_path() {
        // The 32×32 AVIF still fixture decodes to RGBA and lowers to a
        // /DeviceRGB Flate stream (the embedder now accepts AVIF, matching the
        // image→PDF / watermark transcode path).
        let avif = include_bytes!("../raster/fixtures/av1test.avif");
        let prep = prepare_image(avif).expect("AVIF must embed");
        assert_eq!((prep.width, prep.height), (32, 32));
        assert_eq!(prep.filter, ImageFilter::Flate);
        assert_eq!(prep.color, ImageColor::Rgb);
        let rgb = flate_decode(&prep.data).expect("flate");
        assert_eq!(rgb.len(), 32 * 32 * 3, "full RGB plane");
    }
}
