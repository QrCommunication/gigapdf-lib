//! Stream filters (ISO 32000-1 §7.4). Pure `std`, zero dependencies.
//!
//! Decodes a stream's raw bytes by walking its `/Filter` chain. A stream may
//! chain several filters (e.g. `[/ASCII85Decode /FlateDecode]`); each is applied
//! in order, with its matching `/DecodeParms` entry. The classic five filters
//! are all implemented in-house: FlateDecode, LZWDecode, ASCII85Decode,
//! ASCIIHexDecode and RunLengthDecode. `/Predictor` post-processing (for
//! FlateDecode/LZWDecode) is reversed via the [`predictor`] pass. The two
//! bilevel scanned-document filters are implemented from scratch as well:
//! [`ccitt`] (CCITTFaxDecode, Group 3/4) and [`jbig2`] (JBIG2Decode); both yield
//! MSB-first packed 1-bpp rows that the image sample path consumes directly.

pub mod ascii85;
pub mod asciihex;
pub mod ccitt;
pub mod deflate;
pub mod inflate;
pub mod jbig2;
mod jbig2_huffman;
pub mod lzw;
pub mod predictor;
pub mod runlength;

use crate::error::{EngineError, Result};
use crate::object::{Dictionary, Object, Stream};
use predictor::PredictorParams;

/// Decode a stream's bytes by applying its `/Filter` chain in order.
///
/// A stream with no `/Filter` is returned verbatim. Each filter's
/// `/DecodeParms` (e.g. PNG/TIFF predictors, LZW `/EarlyChange`) is applied
/// alongside it.
pub fn decode_stream(stream: &Stream) -> Result<Vec<u8>> {
    let filters = filter_names(stream);
    if filters.is_empty() {
        return Ok(stream.raw.clone());
    }
    let parms = decode_parms(stream, filters.len());
    let mut data = stream.raw.clone();
    for (name, parm) in filters.iter().zip(parms.iter()) {
        data = apply_filter(name, &data, parm.as_ref())?;
    }
    Ok(data)
}

/// The ordered list of filter names on a stream. `/Filter` (or its abbreviation
/// `/F` in inline images) may be a single name or an array of names.
fn filter_names(stream: &Stream) -> Vec<Vec<u8>> {
    let entry = stream.dict.get(b"Filter").or_else(|| stream.dict.get(b"F"));
    match entry {
        Some(Object::Name(name)) => vec![name.clone()],
        Some(Object::Array(items)) => items
            .iter()
            .filter_map(|obj| obj.as_name().map(<[u8]>::to_vec))
            .collect(),
        _ => Vec::new(),
    }
}

/// The `/DecodeParms` (or `/DP`) entries, aligned one-to-one with the filters.
///
/// `/DecodeParms` may be a single dictionary (when there is one filter), an
/// array of dictionaries/`null`s (parallel to a filter array), or absent. The
/// returned vector always has `count` slots so it zips cleanly with the filters.
fn decode_parms(stream: &Stream, count: usize) -> Vec<Option<Dictionary>> {
    let entry = stream
        .dict
        .get(b"DecodeParms")
        .or_else(|| stream.dict.get(b"DP"));
    let mut parms: Vec<Option<Dictionary>> = match entry {
        Some(Object::Dictionary(dict)) => vec![Some(dict.clone())],
        Some(Object::Array(items)) => items.iter().map(|obj| obj.as_dict().cloned()).collect(),
        _ => Vec::new(),
    };
    parms.resize(count, None);
    parms
}

fn apply_filter(name: &[u8], data: &[u8], parms: Option<&Dictionary>) -> Result<Vec<u8>> {
    match name {
        b"FlateDecode" | b"Fl" => {
            let decoded = inflate::flate_decode(data)?;
            apply_predictor(decoded, parms)
        }
        b"LZWDecode" | b"LZW" => {
            let early_change = early_change(parms);
            let decoded = lzw::lzw_decode(data, early_change)?;
            apply_predictor(decoded, parms)
        }
        b"ASCII85Decode" | b"A85" => ascii85::ascii_85_decode(data),
        b"ASCIIHexDecode" | b"AHx" => asciihex::ascii_hex_decode(data),
        b"RunLengthDecode" | b"RL" => runlength::run_length_decode(data),
        b"CCITTFaxDecode" | b"CCF" => {
            let p = parms.map(ccitt::CcittParams::from_dict).unwrap_or_default();
            ccitt::ccitt_decode(data, &p)
        }
        b"JBIG2Decode" => {
            // The JBIG2 page-stream segments decode here. A `/JBIG2Globals`
            // entry that is already an inline (resolved) stream object in the
            // parms is honoured; an unresolved indirect reference cannot be
            // followed at the filter layer (it has no document context), so only
            // the page-stream segments are decoded in that case.
            let globals = parms.and_then(jbig2_globals_bytes);
            jbig2::jbig2_decode(data, globals.as_deref(), parms)
        }
        other => Err(EngineError::Unsupported(format!(
            "stream filter /{}",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// Apply the `/Predictor` post-processing pass described by `parms`, if any.
fn apply_predictor(data: Vec<u8>, parms: Option<&Dictionary>) -> Result<Vec<u8>> {
    let Some(dict) = parms else {
        return Ok(data);
    };
    let params = predictor_params(dict);
    if params.predictor <= 1 {
        return Ok(data);
    }
    predictor::undo_predictor(&data, params)
}

/// Read `/Predictor`, `/Colors`, `/BitsPerComponent` and `/Columns` from a
/// `/DecodeParms` dictionary, falling back to the PDF defaults.
fn predictor_params(dict: &Dictionary) -> PredictorParams {
    let mut params = PredictorParams::default();
    if let Some(v) = dict.get(b"Predictor").and_then(Object::as_i64) {
        params.predictor = v;
    }
    if let Some(v) = dict.get(b"Colors").and_then(Object::as_i64) {
        params.colors = v;
    }
    if let Some(v) = dict.get(b"BitsPerComponent").and_then(Object::as_i64) {
        params.bits_per_component = v;
    }
    if let Some(v) = dict.get(b"Columns").and_then(Object::as_i64) {
        params.columns = v;
    }
    params
}

/// The decoded bytes of an inline `/JBIG2Globals` stream carried in a JBIG2
/// `/DecodeParms` dict, if present and already a (resolved) stream object.
///
/// `/JBIG2Globals` is normally an indirect reference, which cannot be resolved
/// here (the filter layer has no document); in that common case this returns
/// `None` and the page-stream segments decode on their own. When the globals are
/// embedded inline (a literal stream object), their own filter chain is applied
/// so the returned bytes are the raw JBIG2 globals segments.
fn jbig2_globals_bytes(parms: &Dictionary) -> Option<Vec<u8>> {
    match parms.get(b"JBIG2Globals") {
        Some(Object::Stream(stream)) => decode_stream(stream).ok(),
        _ => None,
    }
}

/// The LZW `/EarlyChange` flag (default 1 = true, Adobe behaviour).
fn early_change(parms: Option<&Dictionary>) -> bool {
    parms
        .and_then(|dict| dict.get(b"EarlyChange"))
        .and_then(Object::as_i64)
        .map(|v| v != 0)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::deflate::deflate;
    use crate::object::{Dictionary, Object, Stream};

    fn stream_with(filter: Object, raw: Vec<u8>) -> Stream {
        let mut dict = Dictionary::new();
        dict.set(b"Filter".to_vec(), filter);
        Stream::new(dict, raw)
    }

    #[test]
    fn no_filter_returns_raw() {
        let stream = Stream::new(Dictionary::new(), b"raw bytes".to_vec());
        assert_eq!(decode_stream(&stream).unwrap(), b"raw bytes");
    }

    #[test]
    fn single_ascii_hex_filter() {
        let stream = stream_with(
            Object::Name(b"ASCIIHexDecode".to_vec()),
            b"48656C6C6F>".to_vec(),
        );
        assert_eq!(decode_stream(&stream).unwrap(), b"Hello");
    }

    #[test]
    fn single_ascii85_filter() {
        let stream = stream_with(
            Object::Name(b"ASCII85Decode".to_vec()),
            b"87cURDZ~>".to_vec(),
        );
        assert_eq!(decode_stream(&stream).unwrap(), b"Hello");
    }

    #[test]
    fn single_run_length_filter() {
        let raw = vec![4u8, b'H', b'e', b'l', b'l', b'o', 128];
        let stream = stream_with(Object::Name(b"RunLengthDecode".to_vec()), raw);
        assert_eq!(decode_stream(&stream).unwrap(), b"Hello");
    }

    #[test]
    fn single_lzw_filter() {
        // Reference-encoder bytes for "Hello" (early-change).
        let raw = vec![0x80u8, 0x12, 0x0c, 0xa6, 0xc3, 0x61, 0xbe, 0x02];
        let stream = stream_with(Object::Name(b"LZWDecode".to_vec()), raw);
        assert_eq!(decode_stream(&stream).unwrap(), b"Hello");
    }

    #[test]
    fn chained_ascii85_then_flate() {
        // The canonical chained case: ASCII85-armoured DEFLATE. Encode "Chained
        // filters!" with deflate, then armour the compressed bytes as ASCII85.
        let payload = b"Chained filters!";
        let compressed = deflate(payload);
        let armoured = ascii85_encode(&compressed);

        let filter = Object::Array(vec![
            Object::Name(b"ASCII85Decode".to_vec()),
            Object::Name(b"FlateDecode".to_vec()),
        ]);
        let stream = stream_with(filter, armoured);
        assert_eq!(decode_stream(&stream).unwrap(), payload);
    }

    #[test]
    fn flate_with_png_up_predictor_via_decode_parms() {
        // DEFLATE-compress predictor-encoded bytes, then declare /Predictor 12.
        // Two rows of three 8-bit greys: row0 None [10,20,30], row1 Up [+1,+1,+1].
        let predicted = [0u8, 10, 20, 30, 2u8, 1, 1, 1];
        let compressed = deflate(&predicted);

        let mut dict = Dictionary::new();
        dict.set(b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec()));
        let mut parms = Dictionary::new();
        parms.set(b"Predictor".to_vec(), Object::Integer(12));
        parms.set(b"Columns".to_vec(), Object::Integer(3));
        parms.set(b"Colors".to_vec(), Object::Integer(1));
        parms.set(b"BitsPerComponent".to_vec(), Object::Integer(8));
        dict.set(b"DecodeParms".to_vec(), Object::Dictionary(parms));

        let stream = Stream::new(dict, compressed);
        assert_eq!(decode_stream(&stream).unwrap(), [10, 20, 30, 11, 21, 31]);
    }

    #[test]
    fn unsupported_filter_errors() {
        let stream = stream_with(Object::Name(b"JPXDecode".to_vec()), b"data".to_vec());
        assert!(decode_stream(&stream).is_err());
    }

    #[test]
    fn ccittfax_decode_through_dispatch() {
        // A 1-D (K=0) CCITT row of 8 columns: white 2, black 4, white 2, coded
        // with the modified-Huffman codes. Verify the `/CCITTFaxDecode` filter is
        // dispatched and produces the expected packed 1-bpp byte.
        // W2 = 0x07 (4 bits) = 0111; B4 = 0x03 (3 bits) = 011; W2 = 0111.
        // 0111 011 0111 = 0111_0110_111 -> bytes 0x76 0xE0.
        let coded = vec![0x76u8, 0xE0];
        let mut dict = Dictionary::new();
        dict.set(b"Filter".to_vec(), Object::Name(b"CCITTFaxDecode".to_vec()));
        let mut parms = Dictionary::new();
        parms.set(b"K".to_vec(), Object::Integer(0));
        parms.set(b"Columns".to_vec(), Object::Integer(8));
        parms.set(b"Rows".to_vec(), Object::Integer(1));
        parms.set(b"EndOfBlock".to_vec(), Object::Boolean(false));
        dict.set(b"DecodeParms".to_vec(), Object::Dictionary(parms));
        let stream = Stream::new(dict, coded);
        // WW BBBB WW with 0=black (BlackIs1 default false): 1 1 0 0 0 0 1 1 = 0xC3.
        assert_eq!(decode_stream(&stream).unwrap(), vec![0xC3]);
    }

    #[test]
    fn jbig2_decode_through_dispatch() {
        // A minimal JBIG2 stream (page-info + MMR generic region) routed through
        // the `/JBIG2Decode` filter dispatch. The bytes are the same fixture the
        // jbig2 module test builds, inlined here.
        // page info (8x2) + an MMR generic region painting WWW BB WWW per row.
        let mut s: Vec<u8> = Vec::new();
        // Segment 0: page info.
        s.extend_from_slice(&0u32.to_be_bytes());
        s.push(48);
        s.push(0x00);
        s.push(1);
        s.extend_from_slice(&19u32.to_be_bytes());
        s.extend_from_slice(&8u32.to_be_bytes());
        s.extend_from_slice(&2u32.to_be_bytes());
        s.extend_from_slice(&0u32.to_be_bytes());
        s.extend_from_slice(&0u32.to_be_bytes());
        s.push(0x00);
        s.extend_from_slice(&0u16.to_be_bytes());
        // Segment 1: immediate generic region (type 38), MMR. The MMR payload
        // 0x31 0xF8 decodes (G4) to WWW BB WWW for both rows (validated in the
        // jbig2 module's own round-trip test).
        let mmr = [0x31u8, 0xF8];
        let mut region = Vec::new();
        region.extend_from_slice(&8u32.to_be_bytes());
        region.extend_from_slice(&2u32.to_be_bytes());
        region.extend_from_slice(&0u32.to_be_bytes());
        region.extend_from_slice(&0u32.to_be_bytes());
        region.push(0x00); // OR
        region.push(0x01); // MMR generic flags
        region.extend_from_slice(&mmr);
        s.extend_from_slice(&1u32.to_be_bytes());
        s.push(38);
        s.push(0x00);
        s.push(1);
        s.extend_from_slice(&(region.len() as u32).to_be_bytes());
        s.extend_from_slice(&region);

        let mut dict = Dictionary::new();
        dict.set(b"Filter".to_vec(), Object::Name(b"JBIG2Decode".to_vec()));
        let stream = Stream::new(dict, s);
        // 8x2, 0=black: WWW BB WWW = 0xE7 per row.
        assert_eq!(decode_stream(&stream).unwrap(), vec![0xE7, 0xE7]);
    }

    #[test]
    fn parms_array_aligns_with_filter_array() {
        // First filter (ASCII85) takes no parms; the second (Flate) carries the
        // predictor parms. A leading `null` keeps the array aligned.
        let predicted = [0u8, 1, 2, 3];
        let compressed = deflate(&predicted);
        let armoured = ascii85_encode(&compressed);

        let mut dict = Dictionary::new();
        dict.set(
            b"Filter".to_vec(),
            Object::Array(vec![
                Object::Name(b"ASCII85Decode".to_vec()),
                Object::Name(b"FlateDecode".to_vec()),
            ]),
        );
        let mut flate_parms = Dictionary::new();
        flate_parms.set(b"Predictor".to_vec(), Object::Integer(1)); // identity
        dict.set(
            b"DecodeParms".to_vec(),
            Object::Array(vec![Object::Null, Object::Dictionary(flate_parms)]),
        );

        let stream = Stream::new(dict, armoured);
        assert_eq!(decode_stream(&stream).unwrap(), [0, 1, 2, 3]);
    }

    // Minimal ASCII85 encoder, used only to build test fixtures.
    fn ascii85_encode(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in data.chunks(4) {
            let mut group = [0u8; 4];
            group[..chunk.len()].copy_from_slice(chunk);
            let value = u32::from_be_bytes(group);
            let mut digits = [0u8; 5];
            let mut v = value;
            for slot in digits.iter_mut().rev() {
                *slot = (v % 85) as u8 + b'!';
                v /= 85;
            }
            out.extend_from_slice(&digits[..chunk.len() + 1]);
        }
        out.extend_from_slice(b"~>");
        out
    }
}
