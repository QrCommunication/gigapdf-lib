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

    /// Borrow the (already-unescaped) string bytes if this is a
    /// [`Object::String`].
    pub fn as_string(&self) -> Option<&[u8]> {
        match self {
            Object::String(s, _) => Some(s),
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

    /// Boolean value if this is an [`Object::Boolean`].
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Object::Boolean(b) => Some(*b),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_accessors_match_variant_and_reject_others() {
        assert_eq!(Object::Name(b"Foo".to_vec()).as_name(), Some(&b"Foo"[..]));
        assert_eq!(Object::Null.as_name(), None);

        let s = Object::String(b"hi".to_vec(), StringKind::Literal);
        assert_eq!(s.as_string(), Some(&b"hi"[..]));
        assert_eq!(Object::Null.as_string(), None);

        let arr = Object::Array(vec![Object::Integer(1)]);
        assert_eq!(arr.as_array().map(|a| a.len()), Some(1));
        assert_eq!(Object::Null.as_array(), None);

        assert_eq!(Object::Reference((5, 0)).as_reference(), Some((5, 0)));
        assert_eq!(Object::Null.as_reference(), None);

        assert_eq!(Object::Boolean(true).as_bool(), Some(true));
        assert_eq!(Object::Null.as_bool(), None);
    }

    #[test]
    fn numeric_coercions() {
        // as_i64: integer direct, real truncates, other → None.
        assert_eq!(Object::Integer(7).as_i64(), Some(7));
        assert_eq!(Object::Real(3.9).as_i64(), Some(3));
        assert_eq!(Object::Null.as_i64(), None);
        // as_f64: real direct, integer widens, other → None.
        assert_eq!(Object::Real(2.5).as_f64(), Some(2.5));
        assert_eq!(Object::Integer(4).as_f64(), Some(4.0));
        assert_eq!(Object::Null.as_f64(), None);
    }

    #[test]
    fn as_dict_covers_dictionary_and_stream() {
        let mut d = Dictionary::new();
        d.set("Type", Object::Name(b"Page".to_vec()));
        let dict_obj = Object::Dictionary(d.clone());
        assert!(dict_obj.as_dict().is_some());
        // A stream's dict is reachable through as_dict.
        let stream = Object::Stream(Stream::new(d, b"raw".to_vec()));
        assert_eq!(
            stream
                .as_dict()
                .and_then(|x| x.get(b"Type"))
                .and_then(|o| o.as_name()),
            Some(&b"Page"[..])
        );
        assert!(stream.as_stream().is_some());
        assert_eq!(
            stream.as_stream().map(|s| s.raw.as_slice()),
            Some(&b"raw"[..])
        );
        assert!(Object::Null.as_dict().is_none());
        assert!(Object::Null.as_stream().is_none());
    }

    #[test]
    fn dictionary_crud_and_len() {
        let mut d = Dictionary::new();
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
        d.set("A", Object::Integer(1));
        d.set("B", Object::Boolean(false));
        assert_eq!(d.len(), 2);
        assert!(!d.is_empty());
        assert!(d.contains(b"A"));
        assert!(!d.contains(b"Z"));
        assert_eq!(d.get(b"A").and_then(|o| o.as_i64()), Some(1));
        // remove returns the value, then the key is gone.
        assert_eq!(d.remove(b"A"), Some(Object::Integer(1)));
        assert!(!d.contains(b"A"));
        assert_eq!(d.remove(b"A"), None);
    }

    #[test]
    fn dictionary_debug_renders_keys_as_strings() {
        let mut d = Dictionary::new();
        d.set("Type", Object::Name(b"Catalog".to_vec()));
        let dbg = format!("{d:?}");
        assert!(dbg.contains("Type"), "key rendered as UTF-8 string: {dbg}");
    }
}
