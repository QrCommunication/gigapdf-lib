//! Stream filters (ISO 32000-1 §7.4). Pure `std`, zero dependencies.
//!
//! Decodes a stream's raw bytes by walking its `/Filter` chain. Today the only
//! decoder we need for content streams is `FlateDecode`; others return an
//! `Unsupported` error so callers fail loudly instead of producing garbage.

pub mod deflate;
pub mod inflate;

use crate::error::{EngineError, Result};
use crate::object::{Object, Stream};

/// Decode a stream's bytes by applying its `/Filter` chain in order.
///
/// A stream with no `/Filter` is returned verbatim. `/DecodeParms` (e.g. PNG
/// predictors) are not yet applied — content streams don't use them; xref/image
/// streams that do are handled elsewhere.
pub fn decode_stream(stream: &Stream) -> Result<Vec<u8>> {
    let filters = filter_names(stream);
    if filters.is_empty() {
        return Ok(stream.raw.clone());
    }
    let mut data = stream.raw.clone();
    for name in filters {
        data = apply_filter(&name, &data)?;
    }
    Ok(data)
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
