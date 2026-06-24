//! Stream filters (ISO 32000-1 §7.4). Pure `std`, zero dependencies.
//!
//! Decodes a stream's raw bytes by walking its `/Filter` chain. Today the only
//! decoder we need for content streams is `FlateDecode`; others return an
//! `Unsupported` error so callers fail loudly instead of producing garbage.

pub mod deflate;
pub mod inflate;
pub mod predictor;

use crate::error::{EngineError, Result};
use crate::object::{Dictionary, Object, Stream};

/// Decode a stream's bytes by applying its `/Filter` chain in order.
///
/// A stream with no `/Filter` is returned verbatim. After each filter that
/// carries a `/DecodeParms` (or `/DP`) dict with a `/Predictor` ≥ 2, the TIFF/PNG
/// predictor is reversed (ISO 32000-1 §7.4.4.4) — required for PNG-predicted
/// images and `/Type /XRef` streams.
pub fn decode_stream(stream: &Stream) -> Result<Vec<u8>> {
    let filters = filter_names(stream);
    if filters.is_empty() {
        return Ok(stream.raw.clone());
    }
    let mut data = stream.raw.clone();
    for (i, name) in filters.iter().enumerate() {
        data = apply_filter(name, &data)?;
        if let Some(params) = decode_parms(stream, i) {
            data = predictor::apply_predictor(params, &data)?;
        }
    }
    Ok(data)
}

/// The `/DecodeParms` (or abbreviated `/DP`) dictionary for the filter at index
/// `i` in the `/Filter` chain. The value is either a single dict (used by the
/// single filter) or an array of dicts parallel to `/Filter`; entries may be
/// null when a filter takes no parameters.
fn decode_parms(stream: &Stream, i: usize) -> Option<&Dictionary> {
    let parms = stream
        .dict
        .get(b"DecodeParms")
        .or_else(|| stream.dict.get(b"DP"))?;
    match parms {
        Object::Dictionary(dict) => Some(dict),
        Object::Array(items) => items.get(i).and_then(Object::as_dict),
        _ => None,
    }
}

/// The ordered list of filter names on a stream (`/Filter` may be a single name
/// or an array of names).
fn filter_names(stream: &Stream) -> Vec<Vec<u8>> {
    match stream.dict.get(b"Filter") {
        Some(Object::Name(name)) => vec![name.clone()],
        Some(Object::Array(items)) => items
            .iter()
            .filter_map(|obj| obj.as_name().map(<[u8]>::to_vec))
            .collect(),
        _ => Vec::new(),
    }
}

fn apply_filter(name: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    match name {
        b"FlateDecode" | b"Fl" => inflate::flate_decode(data),
        other => Err(EngineError::Unsupported(format!(
            "stream filter /{}",
            String::from_utf8_lossy(other)
        ))),
    }
}
