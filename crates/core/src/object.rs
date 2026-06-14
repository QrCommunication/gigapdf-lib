//! The PDF object model (ISO 32000-1 §7.3). Pure `std`, zero dependencies.
//!
//! A PDF file is, at its core, a graph of these eight object types referenced
//! by indirect ids. Everything the engine does — parsing, editing, writing —
//! operates on this model.

use std::collections::BTreeMap;
use std::fmt;

/// Indirect object identifier: `(object number, generation number)`.
pub type ObjectId = (u32, u16);

/// How a PDF string was written in the source bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringKind {
    /// `( ... )` literal string.
    Literal,
    /// `< ... >` hexadecimal string.
    Hex,
}

/// A PDF object (ISO 32000-1 §7.3).
///
/// `Name` and `String` hold already-decoded bytes (name `#xx` escapes and
/// string escapes are resolved by the parser). `Real` keeps `f64`; equality is
/// structural (`PartialEq`) which is enough for our edit/compare needs.
#[derive(Debug, Clone, PartialEq)]
pub enum Object {
    /// `null`.
    Null,
    /// `true` / `false`.
    Boolean(bool),
    /// Integer number.
    Integer(i64),
    /// Real number.
    Real(f64),
    /// Name without the leading `/`, with `#xx` escapes resolved.
    Name(Vec<u8>),
    /// String bytes (already unescaped) and how it was written.
    String(Vec<u8>, StringKind),
    /// `[ ... ]`.
    Array(Vec<Object>),
    /// `<< ... >>`.
    Dictionary(Dictionary),
    /// A stream object: dictionary + raw (still-encoded) bytes.
    Stream(Stream),
    /// Indirect reference `n g R`.
    Reference(ObjectId),
}

impl Object {
    /// Borrow the name bytes if this is a [`Object::Name`].
    pub fn as_name(&self) -> Option<&[u8]> {
        match self {
            Object::Name(n) => Some(n),
            _ => None,
        }
    }

    /// Borrow the dictionary if this is a [`Object::Dictionary`] (or the dict of
    /// a [`Object::Stream`]).
    pub fn as_dict(&self) -> Option<&Dictionary> {
        match self {
            Object::Dictionary(d) => Some(d),
            Object::Stream(s) => Some(&s.dict),
            _ => None,
        }
    }

    /// Borrow the array if this is an [`Object::Array`].
    pub fn as_array(&self) -> Option<&[Object]> {
        match self {
            Object::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Get the referenced id if this is an [`Object::Reference`].
    pub fn as_reference(&self) -> Option<ObjectId> {
        match self {
            Object::Reference(id) => Some(*id),
            _ => None,
        }
    }

    /// Integer value, coercing a [`Object::Real`] by truncation.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Object::Integer(i) => Some(*i),
            Object::Real(r) => Some(*r as i64),
            _ => None,
        }
    }

    /// Real value, coercing a [`Object::Integer`].
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Object::Real(r) => Some(*r),
            Object::Integer(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// Borrow the stream if this is an [`Object::Stream`].
    pub fn as_stream(&self) -> Option<&Stream> {
        match self {
            Object::Stream(s) => Some(s),
            _ => None,
        }
    }
}

/// A PDF dictionary: an ordered map from name (bytes, no leading `/`) to object.
///
/// Backed by a `BTreeMap` so iteration and serialization are deterministic.
#[derive(Clone, Default, PartialEq)]
pub struct Dictionary(pub BTreeMap<Vec<u8>, Object>);

impl Dictionary {
    /// An empty dictionary.
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Look up a key (name bytes without the leading `/`).
    pub fn get(&self, key: &[u8]) -> Option<&Object> {
        self.0.get(key)
    }

    /// Insert or replace a key.
    pub fn set(&mut self, key: impl Into<Vec<u8>>, value: Object) {
        self.0.insert(key.into(), value);
    }

    /// Whether the key is present.
    pub fn contains(&self, key: &[u8]) -> bool {
        self.0.contains_key(key)
    }

    /// Remove a key, returning its value if present.
    pub fn remove(&mut self, key: &[u8]) -> Option<Object> {
        self.0.remove(key)
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the dictionary is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Dictionary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for (key, value) in &self.0 {
            map.entry(&String::from_utf8_lossy(key), value);
        }
        map.finish()
    }
}

/// A PDF stream object: a dictionary plus its raw (still filter-encoded) bytes.
///
/// `raw` is exactly what sat between `stream`/`endstream` in the file. Decoding
/// (FlateDecode etc.) happens on demand in the `filters` module.
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    /// The stream dictionary (includes `/Length`, `/Filter`, …).
    pub dict: Dictionary,
    /// Raw, still-encoded stream bytes.
    pub raw: Vec<u8>,
}

impl Stream {
    /// Construct a stream from a dictionary and raw bytes.
    pub fn new(dict: Dictionary, raw: Vec<u8>) -> Self {
        Self { dict, raw }
    }
}
