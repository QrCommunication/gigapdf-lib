//! PDF serializer: write the (edited) object map back to a valid PDF. Pure std.
//!
//! Strategy: we *read* modern PDFs (xref streams + object streams) but *write* a
//! clean, classic structure — objects renumbered `1..N` with a plain xref table
//! and trailer — which every reader accepts. Obsolete `/Type /ObjStm` and
//! `/Type /XRef` objects are dropped (their content is re-emitted directly), and
//! all indirect references are remapped to the new numbering, so the output has
//! no dangling references or free-list gaps.

use std::collections::BTreeMap;

use crate::object::{Dictionary, Object, ObjectId, Stream};

#[inline]
fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0C | 0x00)
}

#[inline]
fn is_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn object_type(object: &Object) -> Option<&[u8]> {
    object
        .as_dict()
        .and_then(|d| d.get(b"Type"))
        .and_then(Object::as_name)
}

/// Objects that exist only to package the old file structure and must not be
/// re-emitted (we write a classic xref + direct objects instead).
fn is_obsolete(object: &Object) -> bool {
    matches!(object_type(object), Some(t) if t == b"ObjStm".as_slice() || t == b"XRef".as_slice())
}

/// Serialize the object map + trailer into a complete PDF byte stream.
pub fn to_pdf(objects: &BTreeMap<ObjectId, Object>, trailer: &Dictionary) -> Vec<u8> {
    // 1. Select and order the objects to keep.
    let mut ids: Vec<ObjectId> = objects
        .iter()
        .filter(|(_, obj)| !is_obsolete(obj))
        .map(|(id, _)| *id)
        .collect();
    ids.sort_unstable();

    // 2. Renumber 1..N (old id → new number) so the output is gap-free.
    let mut remap: BTreeMap<ObjectId, u32> = BTreeMap::new();
    for (index, id) in ids.iter().enumerate() {
        remap.insert(*id, index as u32 + 1);
    }

    // 3. Emit header + objects, recording byte offsets for the xref.
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");

    let count = ids.len() as u32;
    let mut offsets = vec![0usize; count as usize + 1];

    for id in &ids {
        let number = remap[id];
        offsets[number as usize] = out.len();
        let remapped = remap_refs(&objects[id], &remap);
        out.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        write_object(&mut out, &remapped);
        out.extend_from_slice(b"\nendobj\n");
    }

    // 4. Classic xref table.
    let xref_offset = out.len();
    let size = count + 1;
    out.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for number in 1..=count {
        out.extend_from_slice(format!("{:010} 00000 n \n", offsets[number as usize]).as_bytes());
    }

    // 5. Trailer.
    let mut trailer_out = Dictionary::new();
    trailer_out.set(b"Size".to_vec(), Object::Integer(size as i64));
    if let Some(root) = trailer.get(b"Root") {
        trailer_out.set(b"Root".to_vec(), remap_refs(root, &remap));
    }
    if let Some(info) = trailer.get(b"Info") {
        trailer_out.set(b"Info".to_vec(), remap_refs(info, &remap));
    }
    out.extend_from_slice(b"trailer\n");
    write_object(&mut out, &Object::Dictionary(trailer_out));
    out.extend_from_slice(format!("\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes());

    out
}

/// Like [`to_pdf`] but encrypts every object's strings and stream bytes with the
/// Standard Security Handler, appending the `/Encrypt` dictionary (itself never
/// encrypted) and an `/Encrypt` + `/ID` trailer.
pub fn to_pdf_encrypted(
    objects: &BTreeMap<ObjectId, Object>,
    trailer: &Dictionary,
    security: &crate::security::Security,
    encrypt_dict: &Dictionary,
    id0: &[u8],
) -> Vec<u8> {
    let mut ids: Vec<ObjectId> = objects
        .iter()
        .filter(|(_, obj)| !is_obsolete(obj))
        .map(|(id, _)| *id)
        .collect();
    ids.sort_unstable();

    let mut remap: BTreeMap<ObjectId, u32> = BTreeMap::new();
    for (index, id) in ids.iter().enumerate() {
        remap.insert(*id, index as u32 + 1);
    }
    let count = ids.len() as u32;
    let encrypt_number = count + 1; // /Encrypt dict is appended as the last object

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");
    let mut offsets = vec![0usize; count as usize + 2];

    for id in &ids {
        let number = remap[id];
        offsets[number as usize] = out.len();
        let remapped = remap_refs(&objects[id], &remap);
        let encrypted = encrypt_object(&remapped, number, security);
        out.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        write_object(&mut out, &encrypted);
        out.extend_from_slice(b"\nendobj\n");
    }

    // The /Encrypt dictionary itself is written in the clear.
    offsets[encrypt_number as usize] = out.len();
    out.extend_from_slice(format!("{encrypt_number} 0 obj\n").as_bytes());
    write_object(&mut out, &Object::Dictionary(encrypt_dict.clone()));
    out.extend_from_slice(b"\nendobj\n");

    let size = count + 2;
    let xref_offset = out.len();
    out.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for number in 1..=encrypt_number {
        out.extend_from_slice(format!("{:010} 00000 n \n", offsets[number as usize]).as_bytes());
    }

    let mut trailer_out = Dictionary::new();
    trailer_out.set(b"Size".to_vec(), Object::Integer(size as i64));
    if let Some(root) = trailer.get(b"Root") {
        trailer_out.set(b"Root".to_vec(), remap_refs(root, &remap));
    }
    if let Some(info) = trailer.get(b"Info") {
        trailer_out.set(b"Info".to_vec(), remap_refs(info, &remap));
    }
    trailer_out.set(b"Encrypt".to_vec(), Object::Reference((encrypt_number, 0)));
    let id_obj = Object::String(id0.to_vec(), crate::object::StringKind::Literal);
    trailer_out.set(b"ID".to_vec(), Object::Array(vec![id_obj.clone(), id_obj]));
    out.extend_from_slice(b"trailer\n");
    write_object(&mut out, &Object::Dictionary(trailer_out));
    out.extend_from_slice(format!("\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes());
    out
}

fn encrypt_object(object: &Object, num: u32, sec: &crate::security::Security) -> Object {
    match object {
        Object::String(bytes, kind) => Object::String(sec.encrypt(num, 0, bytes), *kind),
        Object::Array(items) => {
            Object::Array(items.iter().map(|o| encrypt_object(o, num, sec)).collect())
        }
        Object::Dictionary(dict) => Object::Dictionary(encrypt_strings(dict, num, sec)),
        Object::Stream(stream) => Object::Stream(Stream {
            dict: encrypt_strings(&stream.dict, num, sec),
            raw: sec.encrypt(num, 0, &stream.raw),
        }),
        other => other.clone(),
    }
}

fn encrypt_strings(dict: &Dictionary, num: u32, sec: &crate::security::Security) -> Dictionary {
    let mut out = Dictionary::new();
    for (key, value) in &dict.0 {
        out.0.insert(key.clone(), encrypt_object(value, num, sec));
    }
    out
}

/// Rewrite every indirect reference inside an object to the new numbering.
/// Dangling references become `null`.
fn remap_refs(object: &Object, map: &BTreeMap<ObjectId, u32>) -> Object {
    match object {
        Object::Reference(id) => match map.get(id) {
            Some(number) => Object::Reference((*number, 0)),
            None => Object::Null,
        },
        Object::Array(items) => {
            Object::Array(items.iter().map(|o| remap_refs(o, map)).collect())
        }
        Object::Dictionary(dict) => Object::Dictionary(remap_dict(dict, map)),
        Object::Stream(stream) => Object::Stream(Stream {
            dict: remap_dict(&stream.dict, map),
            raw: stream.raw.clone(),
        }),
        other => other.clone(),
    }
}

fn remap_dict(dict: &Dictionary, map: &BTreeMap<ObjectId, u32>) -> Dictionary {
    let mut out = Dictionary::new();
    for (key, value) in &dict.0 {
        out.0.insert(key.clone(), remap_refs(value, map));
    }
    out
}

// ─── value writers ──────────────────────────────────────────────────────────

/// Encode a single object value into `out`. Used by the content-stream encoder.
pub(crate) fn encode_value(out: &mut Vec<u8>, object: &Object) {
    write_object(out, object);
}

fn write_object(out: &mut Vec<u8>, object: &Object) {
    match object {
        Object::Null => out.extend_from_slice(b"null"),
        Object::Boolean(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Object::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Object::Real(r) => out.extend_from_slice(format_real(*r).as_bytes()),
        Object::Name(name) => write_name(out, name),
        Object::String(bytes, crate::object::StringKind::Hex) => write_hex_string(out, bytes),
        Object::String(bytes, _) => write_literal_string(out, bytes),
        Object::Array(items) => write_array(out, items),
        Object::Dictionary(dict) => write_dict(out, dict),
        Object::Stream(stream) => write_stream(out, stream),
        Object::Reference((n, g)) => out.extend_from_slice(format!("{n} {g} R").as_bytes()),
    }
}

fn format_real(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return (value as i64).to_string();
    }
    let mut text = format!("{value:.6}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

fn write_name(out: &mut Vec<u8>, name: &[u8]) {
    out.push(b'/');
    for &b in name {
        if b == b'#' || is_whitespace(b) || is_delimiter(b) || !(0x21..=0x7E).contains(&b) {
            out.push(b'#');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0F));
        } else {
            out.push(b);
        }
    }
}

/// Always emit literal `(...)` form: valid for any bytes once `\`, `(`, `)` are
/// escaped. Avoids edge cases of the hex form.
fn write_hex_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'<');
    for &b in bytes {
        out.extend_from_slice(format!("{b:02X}").as_bytes());
    }
    out.push(b'>');
}

fn write_literal_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'(');
    for &b in bytes {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'(' => out.extend_from_slice(b"\\("),
            b')' => out.extend_from_slice(b"\\)"),
            _ => out.push(b),
        }
    }
    out.push(b')');
}

fn write_array(out: &mut Vec<u8>, items: &[Object]) {
    out.push(b'[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        write_object(out, item);
    }
    out.push(b']');
}

fn write_dict(out: &mut Vec<u8>, dict: &Dictionary) {
    out.extend_from_slice(b"<<");
    for (key, value) in &dict.0 {
        out.push(b' ');
        write_name(out, key);
        out.push(b' ');
        write_object(out, value);
    }
    out.extend_from_slice(b" >>");
}

fn write_stream(out: &mut Vec<u8>, stream: &Stream) {
    // Keep the raw (still filter-encoded) bytes; only fix /Length to match.
    let mut dict = stream.dict.clone();
    dict.set(b"Length".to_vec(), Object::Integer(stream.raw.len() as i64));
    write_dict(out, &dict);
    out.extend_from_slice(b"\nstream\n");
    out.extend_from_slice(&stream.raw);
    out.extend_from_slice(b"\nendstream");
}

#[inline]
fn hex_digit(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0' + nibble,
        _ => b'A' + (nibble - 10),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_reals_without_scientific_notation() {
        assert_eq!(format_real(4.0), "4");
        assert_eq!(format_real(1.5), "1.5");
        assert_eq!(format_real(-0.002), "-0.002");
        assert_eq!(format_real(0.0), "0");
    }

    #[test]
    fn escapes_name_special_chars() {
        let mut out = Vec::new();
        write_name(&mut out, b"A B");
        assert_eq!(out, b"/A#20B");
    }

    #[test]
    fn escapes_literal_string() {
        let mut out = Vec::new();
        write_literal_string(&mut out, b"a(b)c\\");
        assert_eq!(out, b"(a\\(b\\)c\\\\)");
    }
}
