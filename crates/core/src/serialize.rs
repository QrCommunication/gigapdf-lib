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

/// A PDF file-version banner offered by the compact / linearized writers.
///
/// Object streams and cross-reference streams require PDF ≥ 1.5; both 1.7
/// (ISO 32000-1) and 2.0 (ISO 32000-2) support them, so these are the two
/// choices a caller may select. The classic writer keeps using
/// [`to_pdf_with_header`] for its own (e.g. PDF/A) header needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PdfVersion {
    /// PDF 1.7 (ISO 32000-1) — the default.
    #[default]
    V1_7,
    /// PDF 2.0 (ISO 32000-2).
    V2_0,
}

impl PdfVersion {
    /// The full file-header line for this version, including the mandatory
    /// binary-comment second line (ISO 32000 §7.5.2) that flags the file binary.
    pub fn header(self) -> &'static [u8] {
        match self {
            PdfVersion::V1_7 => b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n",
            PdfVersion::V2_0 => b"%PDF-2.0\n%\xE2\xE3\xCF\xD3\n",
        }
    }
}

/// Serialize the object map + trailer into a complete PDF byte stream.
pub fn to_pdf(objects: &BTreeMap<ObjectId, Object>, trailer: &Dictionary) -> Vec<u8> {
    to_pdf_with_header(objects, trailer, b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n")
}

/// Classic-xref serializer with a caller-chosen file-header line. Identical to
/// [`to_pdf`] in every other respect; only the leading `%PDF-1.n` banner differs.
/// Used by PDF/A export, where ISO 19005-1 (PDF/A-1) requires a 1.4 header while
/// later parts use 1.7. `header` must include the binary-comment second line.
pub fn to_pdf_with_header(
    objects: &BTreeMap<ObjectId, Object>,
    trailer: &Dictionary,
    header: &[u8],
) -> Vec<u8> {
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
    out.extend_from_slice(header);

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
    // The file identifier is required for PDF/A (ISO 19005-2 §6.1.3) and is a
    // pair of byte strings, not references — preserve it verbatim. (The compact
    // writer `to_pdf_compressed` keeps it the same way.)
    if let Some(id) = trailer.get(b"ID") {
        trailer_out.set(b"ID".to_vec(), remap_refs(id, &remap));
    }
    out.extend_from_slice(b"trailer\n");
    write_object(&mut out, &Object::Dictionary(trailer_out));
    out.extend_from_slice(format!("\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes());

    out
}

/// Append `width` big-endian bytes of `value` (the cross-reference-stream field
/// encoding, ISO 32000-1 §7.5.8.3).
fn push_field(out: &mut Vec<u8>, value: u64, width: usize) {
    let bytes = value.to_be_bytes();
    out.extend_from_slice(&bytes[8 - width..]);
}

/// Serialize with PDF 1.5+ **object streams** + a **cross-reference stream**
/// (ISO 32000-1 §7.5.7 / §7.5.8) for a more compact file, using the historical
/// `%PDF-1.5` header. See [`to_pdf_compressed_with_header`] for the full
/// description; this is the thin default wrapper (mirrors [`to_pdf`] /
/// [`to_pdf_with_header`]).
pub fn to_pdf_compressed(
    objects: &BTreeMap<ObjectId, Object>,
    trailer: &Dictionary,
    use_object_streams: bool,
) -> Vec<u8> {
    to_pdf_compressed_with_header(
        objects,
        trailer,
        use_object_streams,
        b"%PDF-1.5\n%\xE2\xE3\xCF\xD3\n",
    )
}

/// Compact serializer with a caller-chosen file-header line — identical to
/// [`to_pdf_compressed`] in every other respect. When `use_object_streams` is
/// set, every non-stream object is packed into a Flate-compressed `/Type
/// /ObjStm` (type-2 xref entries); otherwise only the cross-reference is written
/// as a stream (all objects stay type-1). Stream objects can never live in an
/// object stream and are always written directly. Linearization (Annex F / Fast
/// Web View) is **not** performed here.
///
/// `header` must be ≥ `%PDF-1.5` (object/xref streams require it) and include the
/// binary-comment second line; pass [`PdfVersion::header`] for a 1.7 / 2.0 banner.
pub fn to_pdf_compressed_with_header(
    objects: &BTreeMap<ObjectId, Object>,
    trailer: &Dictionary,
    use_object_streams: bool,
    header: &[u8],
) -> Vec<u8> {
    use crate::filters::deflate::flate_encode;

    // 1. Select + renumber 1..N (same selection as `to_pdf`; old ObjStm/XRef are
    //    dropped — fresh ones are written below).
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
    let n = ids.len() as u32;

    // 2. Partition: stream objects (type-1) vs compressible non-stream objects.
    let mut top_level: Vec<u32> = Vec::new();
    let mut compressible: Vec<u32> = Vec::new();
    for id in &ids {
        let num = remap[id];
        if !use_object_streams || objects[id].as_stream().is_some() {
            top_level.push(num);
        } else {
            compressible.push(num);
        }
    }

    // 3. Group compressible objects into object streams; assign their numbers
    //    (n+1…) and the cross-reference stream's number (last).
    const PER_STM: usize = 200;
    let stm_groups: Vec<&[u32]> = compressible.chunks(PER_STM).collect();
    let num_stms = stm_groups.len() as u32;
    let xref_num = n + num_stms + 1;

    /// Where a renumbered object lives, for its xref entry.
    enum Loc {
        Offset(usize),
        InStream { stm: u32, idx: u32 },
    }
    let mut loc: BTreeMap<u32, Loc> = BTreeMap::new();

    // 4. Header + the directly-written (stream) objects.
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(header);
    for &num in &top_level {
        let id = ids[(num - 1) as usize];
        let remapped = remap_refs(&objects[&id], &remap);
        loc.insert(num, Loc::Offset(out.len()));
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        write_object(&mut out, &remapped);
        out.extend_from_slice(b"\nendobj\n");
    }

    // 5. Object streams: a header of `N` `(objnum offset)` pairs, then the packed
    //    object bodies starting at `/First`, all Flate-compressed.
    for (group_index, group) in stm_groups.iter().enumerate() {
        let stm_num = n + 1 + group_index as u32;
        let mut header = String::new();
        let mut body: Vec<u8> = Vec::new();
        for (idx, &member) in group.iter().enumerate() {
            let id = ids[(member - 1) as usize];
            let remapped = remap_refs(&objects[&id], &remap);
            header.push_str(&format!("{member} {} ", body.len()));
            write_object(&mut body, &remapped);
            body.push(b'\n');
            loc.insert(
                member,
                Loc::InStream {
                    stm: stm_num,
                    idx: idx as u32,
                },
            );
        }
        let mut decoded = header.into_bytes();
        let first = decoded.len();
        decoded.extend_from_slice(&body);
        let compressed = flate_encode(&decoded);

        let mut dict = Dictionary::new();
        dict.set(b"Type".to_vec(), Object::Name(b"ObjStm".to_vec()));
        dict.set(b"N".to_vec(), Object::Integer(group.len() as i64));
        dict.set(b"First".to_vec(), Object::Integer(first as i64));
        dict.set(b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec()));
        dict.set(b"Length".to_vec(), Object::Integer(compressed.len() as i64));
        loc.insert(stm_num, Loc::Offset(out.len()));
        out.extend_from_slice(format!("{stm_num} 0 obj\n").as_bytes());
        write_dict(&mut out, &dict);
        out.extend_from_slice(b"\nstream\n");
        out.extend_from_slice(&compressed);
        out.extend_from_slice(b"\nendstream\nendobj\n");
    }

    // 6. The cross-reference stream. Fields are `[1 4 2]` bytes: type, then
    //    offset / object-stream number, then generation / index-in-stream.
    let xref_offset = out.len();
    loc.insert(xref_num, Loc::Offset(xref_offset));
    let size = xref_num + 1;
    let (w1, w2, w3) = (1usize, 4usize, 2usize);
    let mut xref_data: Vec<u8> = Vec::with_capacity(size as usize * (w1 + w2 + w3));
    // Object 0 — the head of the free list.
    push_field(&mut xref_data, 0, w1);
    push_field(&mut xref_data, 0, w2);
    push_field(&mut xref_data, 65535, w3);
    for num in 1..=xref_num {
        match loc.get(&num) {
            Some(Loc::Offset(off)) => {
                push_field(&mut xref_data, 1, w1);
                push_field(&mut xref_data, *off as u64, w2);
                push_field(&mut xref_data, 0, w3);
            }
            Some(Loc::InStream { stm, idx }) => {
                push_field(&mut xref_data, 2, w1);
                push_field(&mut xref_data, *stm as u64, w2);
                push_field(&mut xref_data, *idx as u64, w3);
            }
            None => {
                push_field(&mut xref_data, 0, w1);
                push_field(&mut xref_data, 0, w2);
                push_field(&mut xref_data, 0, w3);
            }
        }
    }
    let xref_compressed = flate_encode(&xref_data);

    let mut xdict = Dictionary::new();
    xdict.set(b"Type".to_vec(), Object::Name(b"XRef".to_vec()));
    xdict.set(b"Size".to_vec(), Object::Integer(size as i64));
    if let Some(root) = trailer.get(b"Root") {
        xdict.set(b"Root".to_vec(), remap_refs(root, &remap));
    }
    if let Some(info) = trailer.get(b"Info") {
        xdict.set(b"Info".to_vec(), remap_refs(info, &remap));
    }
    if let Some(id) = trailer.get(b"ID") {
        xdict.set(b"ID".to_vec(), id.clone());
    }
    xdict.set(
        b"W".to_vec(),
        Object::Array(vec![
            Object::Integer(w1 as i64),
            Object::Integer(w2 as i64),
            Object::Integer(w3 as i64),
        ]),
    );
    xdict.set(b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec()));
    xdict.set(
        b"Length".to_vec(),
        Object::Integer(xref_compressed.len() as i64),
    );
    out.extend_from_slice(format!("{xref_num} 0 obj\n").as_bytes());
    write_dict(&mut out, &xdict);
    out.extend_from_slice(b"\nstream\n");
    out.extend_from_slice(&xref_compressed);
    out.extend_from_slice(b"\nendstream\nendobj\n");
    out.extend_from_slice(format!("startxref\n{xref_offset}\n%%EOF\n").as_bytes());

    out
}

/// Append a PDF **incremental update** to an already-serialized `base` document
/// (ISO 32000-1 §7.5.6): keep `base` byte-for-byte intact and write, after it, a
/// fresh body of `new_objects` (each `(number, generation, object)`), a
/// cross-reference section listing only those objects, and a trailer whose
/// `/Prev` chains to the document's previous `startxref`.
///
/// The cross-reference form **matches the base**: if the base's most recent
/// cross-reference is a classic `xref` table, the update writes a classic table +
/// `trailer`; if it is a cross-reference **stream** (PDF ≥ 1.5, ISO 32000-1
/// §7.5.8), the update is itself written as a cross-reference stream (so the file
/// stays single-form and readers that only follow `/XRefStm`-free chains aren't
/// confused). In the stream form the xref stream is an indirect object and
/// consumes the next free number (`size`), so the new `/Size` is `size + 1`.
///
/// This is the mechanism PAdES-LTV needs: a `/DSS` (validation material) or a
/// document timestamp can be added **without disturbing the bytes an existing
/// signature's `/ByteRange` already covers** — the prior signature stays valid.
///
/// `prev_startxref` is the byte offset of the most recent xref in `base` (its
/// `startxref` value); `size` is the new `/Size` (one past the highest object
/// number in the whole file *before* any xref-stream object is added).
/// `root`/`info` carry the (updated) `/Root` and `/Info` references for the
/// trailer. The objects are written verbatim — callers pass fully-formed
/// `Object`s (references already point at final numbers), so no renumbering
/// happens here.
pub fn append_incremental_update(
    base: &[u8],
    new_objects: &[(u32, u16, Object)],
    prev_startxref: usize,
    size: u32,
    root: Object,
    info: Option<Object>,
) -> Vec<u8> {
    let mut out = base.to_vec();
    // A clean object boundary: most writers (and ours) end with `%%EOF\n`; a
    // newline before the first appended object keeps tokens from fusing.
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }

    // 1. Emit the new objects, recording offsets.
    let mut offsets: Vec<(u32, u16, usize)> = Vec::with_capacity(new_objects.len());
    for (number, generation, object) in new_objects {
        offsets.push((*number, *generation, out.len()));
        out.extend_from_slice(format!("{number} {generation} obj\n").as_bytes());
        write_object(&mut out, object);
        out.extend_from_slice(b"\nendobj\n");
    }

    // 2. Cross-reference section + trailer, matching the base's form.
    let mut sorted = offsets.clone();
    sorted.sort_by_key(|(n, _, _)| *n);

    if base_uses_xref_stream(base, prev_startxref) {
        // ── cross-reference STREAM update (ISO 32000-1 §7.5.8) ──
        // The xref stream is itself an indirect object: it takes the next free
        // number and bumps `/Size` by one.
        let xref_num = size;
        let xref_offset = out.len();
        let new_size = size + 1;

        // Entries: every new object (type 1) plus the xref stream itself (type 1),
        // sorted by object number.
        let mut entries: Vec<(u32, u64, u64)> = sorted
            .iter()
            .map(|(n, g, off)| (*n, *off as u64, *g as u64))
            .collect();
        entries.push((xref_num, xref_offset as u64, 0));
        entries.sort_by_key(|e| e.0);

        // Field widths: type = 1 byte; offset/object-number wide enough for the
        // largest offset; generation/index = 2 bytes (as the compact writer).
        let w1 = 1usize;
        let w2 = byte_width(xref_offset as u64).max(2);
        let w3 = 2usize;

        // `/Index` subsections (contiguous runs) + the packed binary field data.
        let mut index: Vec<Object> = Vec::new();
        let mut data: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < entries.len() {
            let start = entries[i].0;
            let mut j = i;
            while j + 1 < entries.len() && entries[j + 1].0 == entries[j].0 + 1 {
                j += 1;
            }
            index.push(Object::Integer(start as i64));
            index.push(Object::Integer((j - i + 1) as i64));
            for (_, off, gen) in &entries[i..=j] {
                push_field(&mut data, 1, w1); // type 1 = uncompressed object
                push_field(&mut data, *off, w2);
                push_field(&mut data, *gen, w3);
            }
            i = j + 1;
        }
        let compressed = crate::filters::deflate::flate_encode(&data);

        let mut xdict = Dictionary::new();
        xdict.set(b"Type".to_vec(), Object::Name(b"XRef".to_vec()));
        xdict.set(b"Size".to_vec(), Object::Integer(new_size as i64));
        xdict.set(b"Root".to_vec(), root);
        if let Some(info) = info {
            xdict.set(b"Info".to_vec(), info);
        }
        xdict.set(b"Prev".to_vec(), Object::Integer(prev_startxref as i64));
        xdict.set(b"Index".to_vec(), Object::Array(index));
        xdict.set(
            b"W".to_vec(),
            Object::Array(vec![
                Object::Integer(w1 as i64),
                Object::Integer(w2 as i64),
                Object::Integer(w3 as i64),
            ]),
        );
        xdict.set(b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec()));
        xdict.set(b"Length".to_vec(), Object::Integer(compressed.len() as i64));

        out.extend_from_slice(format!("{xref_num} 0 obj\n").as_bytes());
        write_dict(&mut out, &xdict);
        out.extend_from_slice(b"\nstream\n");
        out.extend_from_slice(&compressed);
        out.extend_from_slice(b"\nendstream\nendobj\n");
        out.extend_from_slice(format!("startxref\n{xref_offset}\n%%EOF\n").as_bytes());
        return out;
    }

    // ── classic xref table (one subsection per contiguous run of numbers; an
    //    incremental update need not start at object 0) + trailer ──
    let xref_offset = out.len();
    out.extend_from_slice(b"xref\n");
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i].0;
        let mut j = i;
        // Extend the run while object numbers stay consecutive.
        while j + 1 < sorted.len() && sorted[j + 1].0 == sorted[j].0 + 1 {
            j += 1;
        }
        let run_len = j - i + 1;
        out.extend_from_slice(format!("{start} {run_len}\n").as_bytes());
        for (_, generation, offset) in &sorted[i..=j] {
            out.extend_from_slice(format!("{offset:010} {generation:05} n \n").as_bytes());
        }
        i = j + 1;
    }

    // 3. Trailer chaining to the previous xref via /Prev.
    let mut trailer = Dictionary::new();
    trailer.set(b"Size".to_vec(), Object::Integer(size as i64));
    trailer.set(b"Root".to_vec(), root);
    if let Some(info) = info {
        trailer.set(b"Info".to_vec(), info);
    }
    trailer.set(b"Prev".to_vec(), Object::Integer(prev_startxref as i64));
    out.extend_from_slice(b"trailer\n");
    write_object(&mut out, &Object::Dictionary(trailer));
    out.extend_from_slice(format!("\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes());

    out
}

/// Whether the base's most recent cross-reference (at byte offset
/// `prev_startxref`) is a cross-reference **stream** (PDF ≥ 1.5) rather than a
/// classic `xref` table. A classic section begins with the literal `xref`
/// keyword; a cross-reference stream begins with an indirect-object header
/// (`<num> <gen> obj`). Out-of-range / unrecognized ⇒ treated as classic (safe
/// default — the historical behaviour).
fn base_uses_xref_stream(base: &[u8], prev_startxref: usize) -> bool {
    let Some(rest) = base.get(prev_startxref..) else {
        return false;
    };
    let mut i = 0;
    while i < rest.len() && is_whitespace(rest[i]) {
        i += 1;
    }
    !rest[i..].starts_with(b"xref")
}

/// Number of big-endian bytes needed to represent `value` (minimum 1).
fn byte_width(value: u64) -> usize {
    let mut width = 1usize;
    let mut v = value >> 8;
    while v > 0 {
        width += 1;
        v >>= 8;
    }
    width
}

// ─── linearization (Fast Web View, ISO 32000-1 Annex F) ─────────────────────

/// A big-endian, MSB-first bit packer for the hint-table records (Annex F.3).
///
/// Hint tables are sequences of variable-width unsigned integers packed without
/// byte alignment; the stream is byte-padded only at the very end. `pad()` flushes
/// the final partial byte (trailing zero bits), as the spec requires.
struct BitWriter {
    out: Vec<u8>,
    cur: u8,
    nbits: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            cur: 0,
            nbits: 0,
        }
    }

    /// Append the low `width` bits of `value`, most-significant bit first.
    fn put(&mut self, value: u64, width: u32) {
        let mut i = width;
        while i > 0 {
            i -= 1;
            let bit = ((value >> i) & 1) as u8;
            self.cur = (self.cur << 1) | bit;
            self.nbits += 1;
            if self.nbits == 8 {
                self.out.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
            }
        }
    }

    /// Flush a partial final byte (zero-padded), per Annex F.
    fn pad(&mut self) {
        if self.nbits > 0 {
            self.cur <<= 8 - self.nbits;
            self.out.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    fn into_bytes(mut self) -> Vec<u8> {
        self.pad();
        self.out
    }
}

/// Minimum number of bits needed to represent `value` (0 needs 0 bits, since the
/// hint header carries the "least" value and records carry only the difference;
/// an all-equal column then occupies zero bits per record, exactly like qpdf).
fn bit_width(value: u64) -> u32 {
    64 - value.leading_zeros()
}

/// Collect indirect references inside `object`. When `prune_kids` is set, the
/// `/Kids` array (a page-tree node's children) is skipped so the closure does not
/// walk into sibling pages.
fn collect_refs_pruned(object: &Object, out: &mut Vec<ObjectId>, prune_kids: bool) {
    match object {
        Object::Reference(id) => out.push(*id),
        Object::Array(items) => items
            .iter()
            .for_each(|o| collect_refs_pruned(o, out, false)),
        Object::Dictionary(dict) | Object::Stream(Stream { dict, .. }) => {
            for (key, value) in &dict.0 {
                if prune_kids && key.as_slice() == b"Kids" {
                    continue;
                }
                collect_refs_pruned(value, out, false);
            }
        }
        _ => {}
    }
}

/// Who references an object, for the linearization object partition. Mirrors
/// qpdf's `ObjUser`: a page (by 0-based index), a catalog ("root") key, the
/// catalog itself, or a trailer key. Only the distinctions the partition needs are
/// modelled.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ObjUser {
    /// Referenced from page `i`'s object subtree (0-based).
    Page(usize),
    /// Referenced from catalog key `/Name` (e.g. `/Pages`, `/Names`, `/AcroForm`).
    RootKey(Vec<u8>),
    /// The catalog object itself (the document root).
    Root,
    /// Referenced from a trailer key (e.g. `/Info`).
    TrailerKey(Vec<u8>),
}

/// The result of qpdf's object partition for a linearized file.
struct Partition {
    /// Part 4 — "open document" objects (catalog + viewer prefs / AcroForm / …),
    /// placed ahead of the hint stream.
    part4: Vec<ObjectId>,
    /// Part 6 — the first-page section: page-1 object, then its private objects,
    /// then the shared objects it uses. `nshared_first_page == part6.len()`.
    part6: Vec<ObjectId>,
    /// Part 7 — other pages' private objects, grouped per page. `page_groups[i]`
    /// (i ≥ 1) is that page's objects (page object first), laid out contiguously.
    page_groups: Vec<Vec<ObjectId>>,
    /// Part 8 — objects shared between pages 2..N (not used by the first page).
    part8: Vec<ObjectId>,
    /// Part 9 — the page tree, then everything else. The `outline_group` and
    /// `thread_group` runs are kept contiguous within it.
    part9: Vec<ObjectId>,
    /// Document-outline objects (the `/Outlines` dictionary first, then its
    /// items), as a single contiguous run inside `part9` — input to the outline
    /// hint table (ISO 32000-1 §F.3, `/O`). Empty when there is no outline.
    outline_group: Vec<ObjectId>,
    /// Article-thread objects (`/Threads` dictionaries + info dicts), as a single
    /// contiguous run inside `part9` — input to the thread-information hint table
    /// (`/A`). Empty when there are no threads.
    thread_group: Vec<ObjectId>,
    /// First page's page object (for `/O`).
    page1: ObjectId,
    /// Number of pages.
    npages: usize,
    /// For each object, the set of users that reference it (to compute "shared").
    users: std::collections::BTreeMap<ObjectId, std::collections::BTreeSet<ObjUser>>,
}

/// All page leaves under `root`, in reading (depth-first) order.
fn collect_page_leaves(objects: &BTreeMap<ObjectId, Object>, root: ObjectId) -> Vec<ObjectId> {
    fn walk(
        objects: &BTreeMap<ObjectId, Object>,
        id: ObjectId,
        depth: usize,
        out: &mut Vec<ObjectId>,
    ) {
        if depth > 64 {
            return;
        }
        let Some(dict) = objects.get(&id).and_then(Object::as_dict) else {
            return;
        };
        let is_pages = dict.get(b"Type").and_then(Object::as_name) == Some(b"Pages".as_slice())
            || dict.contains(b"Kids");
        if is_pages {
            if let Some(kids) = dict.get(b"Kids").and_then(Object::as_array) {
                for kid in kids {
                    if let Some(k) = kid.as_reference() {
                        walk(objects, k, depth + 1, out);
                    }
                }
            }
        } else {
            out.push(id);
        }
    }
    let mut out = Vec::new();
    walk(objects, root, 0, &mut out);
    out
}

/// Walk the object graph from `root_obj` recording every reachable indirect object
/// as a user `ou`, exactly like qpdf's `updateObjectMapsInternal`: do not cross a
/// `/Page` boundary (a `/Page` reached non-top stops the walk), and skip a page
/// node's `/Parent` (no walking back up the tree). `top` is true only for the
/// starting object.
fn map_users(
    objects: &BTreeMap<ObjectId, Object>,
    start: &Object,
    ou: &ObjUser,
    users: &mut std::collections::BTreeMap<ObjectId, std::collections::BTreeSet<ObjUser>>,
    visited: &mut std::collections::BTreeSet<ObjectId>,
) {
    fn rec(
        objects: &BTreeMap<ObjectId, Object>,
        oh: &Object,
        ou: &ObjUser,
        users: &mut std::collections::BTreeMap<ObjectId, std::collections::BTreeSet<ObjUser>>,
        visited: &mut std::collections::BTreeSet<ObjectId>,
        top: bool,
        depth: usize,
    ) {
        if depth > 256 {
            return;
        }
        // Resolve an indirect reference to its target dict/array, recording usage.
        if let Object::Reference(id) = oh {
            let id = *id;
            let target = objects.get(&id);
            let is_page = target
                .map(|o| object_type(o) == Some(b"Page".as_slice()))
                .unwrap_or(false);
            if is_page && !top {
                return; // do not cross into another page
            }
            if !visited.insert(id) {
                return; // already walked under this user
            }
            users.entry(id).or_default().insert(ou.clone());
            if let Some(obj) = target {
                walk_container(objects, obj, ou, users, visited, is_page, depth + 1);
            }
            return;
        }
        // Direct (inline) array/dict — descend without recording (no object id).
        walk_container(objects, oh, ou, users, visited, false, depth + 1);
    }

    fn walk_container(
        objects: &BTreeMap<ObjectId, Object>,
        oh: &Object,
        ou: &ObjUser,
        users: &mut std::collections::BTreeMap<ObjectId, std::collections::BTreeSet<ObjUser>>,
        visited: &mut std::collections::BTreeSet<ObjectId>,
        is_page_node: bool,
        depth: usize,
    ) {
        match oh {
            Object::Array(items) => {
                for it in items {
                    rec(objects, it, ou, users, visited, false, depth);
                }
            }
            Object::Dictionary(dict) | Object::Stream(Stream { dict, .. }) => {
                for (key, value) in &dict.0 {
                    if is_page_node && key.as_slice() == b"Parent" {
                        continue; // don't traverse back up the page tree
                    }
                    if is_page_node && key.as_slice() == b"Thumb" {
                        continue; // thumbnails are a separate user (unsupported here)
                    }
                    rec(objects, value, ou, users, visited, false, depth);
                }
            }
            _ => {}
        }
    }

    // The starting object is `top`. If it is itself an indirect reference, follow
    // it as top; otherwise descend into it directly.
    rec(objects, start, ou, users, visited, true, 0);
}

/// Partition the document's objects following qpdf's linearization algorithm so a
/// strict reader's self-recomputation of the hint tables agrees byte-for-byte.
fn partition_by_page(
    objects: &BTreeMap<ObjectId, Object>,
    catalog_id: ObjectId,
    pages_root_id: ObjectId,
    trailer: &Dictionary,
) -> Option<Partition> {
    use std::collections::{BTreeMap as Map, BTreeSet};

    let page_leaves = collect_page_leaves(objects, pages_root_id);
    if page_leaves.is_empty() {
        return None;
    }
    let npages = page_leaves.len();

    // Build `object_to_obj_users`.
    let mut users: Map<ObjectId, BTreeSet<ObjUser>> = Map::new();

    // Catalog itself → Root.
    users.entry(catalog_id).or_default().insert(ObjUser::Root);

    // Each page subtree → Page(i).
    for (i, &leaf) in page_leaves.iter().enumerate() {
        let mut visited: BTreeSet<ObjectId> = BTreeSet::new();
        map_users(
            objects,
            &Object::Reference(leaf),
            &ObjUser::Page(i),
            &mut users,
            &mut visited,
        );
    }

    // Each catalog key → RootKey(key), walking from the key's value.
    if let Some(catalog) = objects.get(&catalog_id).and_then(Object::as_dict) {
        for (key, value) in &catalog.0 {
            if key.as_slice() == b"Type" {
                continue;
            }
            let mut visited: BTreeSet<ObjectId> = BTreeSet::new();
            map_users(
                objects,
                value,
                &ObjUser::RootKey(key.clone()),
                &mut users,
                &mut visited,
            );
        }
    }

    // Trailer keys (Info etc.) → TrailerKey(key).
    for (key, value) in &trailer.0 {
        if matches!(
            key.as_slice(),
            b"Root" | b"Size" | b"Prev" | b"ID" | b"Encrypt" | b"XRefStm"
        ) {
            continue;
        }
        let mut visited: BTreeSet<ObjectId> = BTreeSet::new();
        map_users(
            objects,
            value,
            &ObjUser::TrailerKey(key.clone()),
            &mut users,
            &mut visited,
        );
    }

    // Classify each object (qpdf's category switch). `/Threads` is intentionally
    // omitted here (qpdf keeps it as an open-document key): ISO 32000-1 recommends
    // thread information dictionaries live in part 9, and placing them there lets
    // them form the contiguous run the thread-information hint table (`/A`) needs.
    const OPEN_DOC_KEYS: &[&[u8]] = &[
        b"ViewerPreferences",
        b"PageMode",
        b"OpenAction",
        b"AcroForm",
    ];
    let mut lc_open_document: Vec<ObjectId> = Vec::new();
    let mut lc_first_page_private: BTreeSet<ObjectId> = BTreeSet::new();
    let mut lc_first_page_shared: BTreeSet<ObjectId> = BTreeSet::new();
    let mut lc_other_page_private: BTreeSet<ObjectId> = BTreeSet::new();
    let mut lc_other_page_shared: BTreeSet<ObjectId> = BTreeSet::new();
    let mut lc_other: BTreeSet<ObjectId> = BTreeSet::new();
    let mut lc_root: Option<ObjectId> = None;

    for (&og, ous) in &users {
        let mut in_open_document = false;
        let mut in_first_page = false;
        let mut other_pages = 0u32;
        let mut others = 0u32;
        let mut is_root = false;
        for ou in ous {
            match ou {
                ObjUser::TrailerKey(_) => others += 1,
                ObjUser::RootKey(k) => {
                    if OPEN_DOC_KEYS.contains(&k.as_slice()) {
                        in_open_document = true;
                    } else {
                        others += 1;
                    }
                }
                ObjUser::Page(0) => in_first_page = true,
                ObjUser::Page(_) => other_pages += 1,
                ObjUser::Root => is_root = true,
            }
        }
        if is_root {
            lc_root = Some(og);
        } else if in_open_document {
            lc_open_document.push(og);
        } else if in_first_page && others == 0 && other_pages == 0 {
            lc_first_page_private.insert(og);
        } else if in_first_page {
            lc_first_page_shared.insert(og);
        } else if other_pages == 1 && others == 0 {
            lc_other_page_private.insert(og);
        } else if other_pages > 1 {
            lc_other_page_shared.insert(og);
        } else {
            lc_other.insert(og);
        }
    }
    let catalog = lc_root?;

    // Part 4: catalog, then open-document objects.
    let mut part4: Vec<ObjectId> = vec![catalog];
    part4.extend(lc_open_document.iter().copied());

    // Part 6: first page object, then its private objects, then its shared objects.
    let page1 = page_leaves[0];
    lc_first_page_private.remove(&page1);
    let mut part6: Vec<ObjectId> = vec![page1];
    part6.extend(lc_first_page_private.iter().copied());
    part6.extend(lc_first_page_shared.iter().copied());

    // Part 7: other pages' private objects, grouped per page (page object first).
    let mut page_groups: Vec<Vec<ObjectId>> = Vec::with_capacity(npages);
    page_groups.push(part6.clone()); // index 0 is the first-page section
    let mut consumed_other_private = lc_other_page_private.clone();
    for (i, &leaf) in page_leaves.iter().enumerate().skip(1) {
        let mut group: Vec<ObjectId> = vec![leaf];
        consumed_other_private.remove(&leaf);
        // This page's private objects = objects in its closure that are in
        // lc_other_page_private. Recompute the closure ids for this page.
        let mut visited: BTreeSet<ObjectId> = BTreeSet::new();
        let mut page_user_objs: Map<ObjectId, BTreeSet<ObjUser>> = Map::new();
        map_users(
            objects,
            &Object::Reference(leaf),
            &ObjUser::Page(i),
            &mut page_user_objs,
            &mut visited,
        );
        for &og in page_user_objs.keys() {
            if og != leaf && consumed_other_private.remove(&og) {
                group.push(og);
            }
        }
        page_groups.push(group);
    }

    // Part 8: other pages' shared objects (order unimportant).
    let part8: Vec<ObjectId> = lc_other_page_shared.iter().copied().collect();

    // Part 9: the page tree first, then the remaining lc_other objects — but keep
    // the outline and thread object groups as contiguous runs so their generic
    // hint tables (ISO 32000-1 §F.3) are single byte spans (qpdf validates the
    // outline run's offset + length).
    let mut part9: Vec<ObjectId> = Vec::new();
    let mut remaining = lc_other.clone();
    // Page-tree nodes that landed in lc_other, in tree order.
    for id in page_tree_node_order(objects, pages_root_id) {
        if remaining.remove(&id) {
            part9.push(id);
        }
    }

    // Outline group: the `/Outlines` dictionary first (it must own the lowest
    // offset — the outline hint table's `first_object`), then its items still in
    // part 9 (objects reached from the `/Outlines` root key). Built only when the
    // `/Outlines` dictionary itself is a part-9 object.
    let outlines_root_id = objects
        .get(&catalog_id)
        .and_then(Object::as_dict)
        .and_then(|d| d.get(b"Outlines"))
        .and_then(Object::as_reference);
    let mut outline_group: Vec<ObjectId> = Vec::new();
    if let Some(out_id) = outlines_root_id {
        if remaining.remove(&out_id) {
            outline_group.push(out_id);
            let key = ObjUser::RootKey(b"Outlines".to_vec());
            let items: Vec<ObjectId> = remaining
                .iter()
                .copied()
                .filter(|id| users.get(id).is_some_and(|u| u.contains(&key)))
                .collect();
            for id in items {
                remaining.remove(&id);
                outline_group.push(id);
            }
        }
    }

    // Thread group: thread + info objects reached from the `/Threads` root key
    // that remain in part 9 (page-shared beads stay with their pages). qpdf
    // ignores the `/A` table, so this is a best-effort, self-consistent run.
    let mut thread_group: Vec<ObjectId> = Vec::new();
    {
        let key = ObjUser::RootKey(b"Threads".to_vec());
        let items: Vec<ObjectId> = remaining
            .iter()
            .copied()
            .filter(|id| users.get(id).is_some_and(|u| u.contains(&key)))
            .collect();
        for id in items {
            remaining.remove(&id);
            thread_group.push(id);
        }
    }

    part9.extend(remaining.iter().copied());
    part9.extend(outline_group.iter().copied());
    part9.extend(thread_group.iter().copied());

    Some(Partition {
        part4,
        part6,
        page_groups,
        part8,
        part9,
        outline_group,
        thread_group,
        page1,
        npages,
        users,
    })
}

/// Page-tree interior (`/Pages`) node ids under `root`, in tree (depth-first)
/// order, including `root`.
fn page_tree_node_order(objects: &BTreeMap<ObjectId, Object>, root: ObjectId) -> Vec<ObjectId> {
    fn walk(
        objects: &BTreeMap<ObjectId, Object>,
        id: ObjectId,
        depth: usize,
        out: &mut Vec<ObjectId>,
    ) {
        if depth > 64 {
            return;
        }
        let Some(dict) = objects.get(&id).and_then(Object::as_dict) else {
            return;
        };
        let is_pages = dict.get(b"Type").and_then(Object::as_name) == Some(b"Pages".as_slice())
            || dict.contains(b"Kids");
        if is_pages {
            out.push(id);
            if let Some(kids) = dict.get(b"Kids").and_then(Object::as_array) {
                for kid in kids {
                    if let Some(k) = kid.as_reference() {
                        walk(objects, k, depth + 1, out);
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(objects, root, 0, &mut out);
    out
}

/// Serialize as a **linearized** ("Fast Web View") PDF per ISO 32000-1 Annex F:
/// the first page (and only the objects needed to render it) plus a linearization
/// parameter dictionary and a primary hint stream are written at the **front** of
/// the file, so a viewer can display page 1 before the whole file has downloaded.
///
/// Layout (in order):
/// 1. `%PDF-1.x` header;
/// 2. the **linearization parameter dictionary** as the first body object
///    (`/Linearized 1 /L /H /O /E /N /T`);
/// 3. the **first-page cross-reference section** (classic table) + its trailer,
///    whose `/Prev` chains to the main xref;
/// 4. the document **catalog**, then the **primary hint stream**;
/// 5. the **first page's private** objects (page node, content, page-only
///    resources) — `/E` marks the end of this region;
/// 6. pages 2..N's private objects (each page contiguous), then the **shared**
///    objects (page-tree nodes, cross-page resources, document-level structure);
/// 7. the **main cross-reference table** + final trailer (whose `startxref` is the
///    first-page xref, per Annex F).
///
/// Object numbers are reassigned to follow the physical order so every hint table
/// references a contiguous range. Offsets (`/L`, `/H`, `/O`, `/E`, `/T`, the two
/// xref sections, the hint tables) are resolved by laying the file out
/// analytically with fixed-width (10-digit, zero-padded) numeric fields, so the
/// only feedback loop — the hint stream's own length — converges in a short
/// fixed-point iteration.
///
/// Returns `None` when the document cannot be linearized (no catalog / no page
/// tree / zero pages); callers fall back to a non-linearized writer.
///
/// Uses the default `%PDF-1.7` header; see [`to_linearized_with_header`] to pick
/// the version banner (e.g. via [`PdfVersion::header`]).
pub fn to_linearized(
    objects: &BTreeMap<ObjectId, Object>,
    trailer: &Dictionary,
) -> Option<Vec<u8>> {
    to_linearized_with_header(objects, trailer, b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n")
}

/// Linearized serializer with a caller-chosen file-header line — identical to
/// [`to_linearized`] otherwise. `header` must include the binary-comment second
/// line; pass [`PdfVersion::header`] for a 1.7 / 2.0 banner.
pub fn to_linearized_with_header(
    objects: &BTreeMap<ObjectId, Object>,
    trailer: &Dictionary,
    header: &[u8],
) -> Option<Vec<u8>> {
    use crate::filters::deflate::flate_encode;

    // ── 1. Resolve the catalog and page tree. ──
    let mut ids: Vec<ObjectId> = objects
        .iter()
        .filter(|(_, obj)| !is_obsolete(obj))
        .map(|(id, _)| *id)
        .collect();
    ids.sort_unstable();
    if ids.is_empty() {
        return None;
    }
    let catalog_id = trailer
        .get(b"Root")
        .and_then(Object::as_reference)
        .filter(|id| objects.contains_key(id))
        .or_else(|| {
            ids.iter()
                .find(|id| object_type(&objects[id]) == Some(b"Catalog".as_slice()))
                .copied()
        })?;
    let pages_root_id = objects
        .get(&catalog_id)
        .and_then(Object::as_dict)
        .and_then(|d| d.get(b"Pages"))
        .and_then(Object::as_reference)
        .filter(|id| objects.contains_key(id))?;

    // ── 2. Partition objects (qpdf-faithful parts 4/6/7/8/9). ──
    let part = partition_by_page(objects, catalog_id, pages_root_id, trailer)?;
    let n_pages = part.npages as u32;
    if n_pages == 0 {
        return None;
    }

    // ── 3. Physical order = numbering order. ──
    // Layout of body objects (after the lin-dict + first-page xref):
    //   part4 → hint → part6 → part7(page groups 1..N) → part8 → part9 → tail
    // We renumber 1..N in exactly this physical sequence so that, for any page, its
    // objects occupy a contiguous run of object numbers with the page object first
    // (qpdf validates page length as the byte span of that run).
    //
    // The linearization dictionary is physically first (number 1); the hint stream
    // sits physically between part4 and part6 and is numbered there.
    let mut ordered: Vec<ObjectId> = Vec::new();
    // (lin dict number is 1; it is synthetic with no original id.)
    let lin_num = 1u32;
    let mut next_num = 2u32;
    let mut remap: BTreeMap<ObjectId, u32> = BTreeMap::new();
    // part4
    for &id in &part.part4 {
        remap.insert(id, next_num);
        ordered.push(id);
        next_num += 1;
    }
    // hint (synthetic) goes here physically and numerically
    let hint_num = next_num;
    next_num += 1;
    // part6 (= page_groups[0]) then part7 (page_groups[1..])
    for group in &part.page_groups {
        for &id in group {
            // Skip ids already placed (an object could be both open-document and on
            // the page — keep the first placement).
            if let std::collections::btree_map::Entry::Vacant(e) = remap.entry(id) {
                e.insert(next_num);
                ordered.push(id);
                next_num += 1;
            }
        }
    }
    // part8
    for &id in &part.part8 {
        if let std::collections::btree_map::Entry::Vacant(e) = remap.entry(id) {
            e.insert(next_num);
            ordered.push(id);
            next_num += 1;
        }
    }
    // part9
    for &id in &part.part9 {
        if let std::collections::btree_map::Entry::Vacant(e) = remap.entry(id) {
            e.insert(next_num);
            ordered.push(id);
            next_num += 1;
        }
    }
    // tail: any object not placed anywhere (defensive — nothing dropped).
    for &id in &ids {
        if let std::collections::btree_map::Entry::Vacant(e) = remap.entry(id) {
            e.insert(next_num);
            ordered.push(id);
            next_num += 1;
        }
    }
    let n_total = next_num - 1; // highest assigned object number
    let page1_num = remap[&part.page1];

    // ── 4. Encode every object body (references remapped). ──
    let encode_body = |id: &ObjectId| -> Vec<u8> {
        let mut b = Vec::new();
        write_object(&mut b, &remap_refs(&objects[id], &remap));
        b
    };
    let obj_span = |num: u32, body: &[u8]| -> usize {
        format!("{num} 0 obj\n").len() + body.len() + b"\nendobj\n".len()
    };
    // body bytes keyed by number, in physical order.
    let bodies: Vec<(u32, Vec<u8>)> = ordered
        .iter()
        .map(|id| (remap[id], encode_body(id)))
        .collect();
    // part4 region length (physically between first-page xref and the hint).
    let part4_len: usize = part
        .part4
        .iter()
        .map(|id| {
            let num = remap[id];
            obj_span(num, &encode_body(id))
        })
        .sum();

    // Shared-object hint table inputs: part6 (all) followed by part8, in order.
    // `shared_idx_of[obj_num]` = its 0-based index in the shared table.
    let mut shared_obj_nums: Vec<u32> = Vec::new();
    for &id in &part.part6 {
        shared_obj_nums.push(remap[&id]);
    }
    let nshared_first_page = part.part6.len();
    for &id in &part.part8 {
        shared_obj_nums.push(remap[&id]);
    }
    let mut shared_idx_of: BTreeMap<u32, usize> = BTreeMap::new();
    for (idx, &num) in shared_obj_nums.iter().enumerate() {
        shared_idx_of.insert(num, idx);
    }

    // Per-page hint inputs: object count, and the shared-object indices each
    // page (after the first) references (objects used by >1 user that live in the
    // shared table). qpdf computes these from `object_to_obj_users`.
    let mut page_nobjects: Vec<u32> = Vec::with_capacity(n_pages as usize);
    page_nobjects.push(part.part6.len() as u32); // page 0 owns the whole first-page section
    let mut page_shared_idx: Vec<Vec<u32>> = Vec::with_capacity(n_pages as usize);
    page_shared_idx.push(Vec::new()); // page 0 references no shared objects (by spec)
    for (gi, group) in part.page_groups.iter().enumerate().skip(1) {
        page_nobjects.push(group.len() as u32);
        // shared objects referenced by this page = ids in its closure that are used
        // by >1 user and present in the shared table.
        let mut visited = std::collections::BTreeSet::new();
        let mut this_users: BTreeMap<ObjectId, std::collections::BTreeSet<ObjUser>> =
            BTreeMap::new();
        map_users(
            objects,
            &Object::Reference(part.page_groups[gi][0]),
            &ObjUser::Page(gi),
            &mut this_users,
            &mut visited,
        );
        let mut idxs: Vec<u32> = Vec::new();
        for &id in this_users.keys() {
            let shared = part.users.get(&id).map(|u| u.len() > 1).unwrap_or(false);
            if shared {
                if let Some(&idx) = shared_idx_of.get(&remap[&id]) {
                    idxs.push(idx as u32);
                }
            }
        }
        idxs.sort_unstable();
        idxs.dedup();
        page_shared_idx.push(idxs);
    }

    // ── 5. Geometry independent of the hint length. ──
    let id_obj = trailer_id_pair(trailer);
    let first_trailer_len = first_page_trailer_len(&id_obj);

    // First-page xref covers: lin_num, the part4 object numbers, hint_num, and the
    // part6 (first-page section) object numbers.
    let mut first_xref_nums: Vec<u32> = vec![lin_num, hint_num];
    for &id in &part.part4 {
        first_xref_nums.push(remap[&id]);
    }
    for &id in &part.part6 {
        first_xref_nums.push(remap[&id]);
    }
    first_xref_nums.sort_unstable();
    first_xref_nums.dedup();
    let runs = contiguous_runs(&first_xref_nums);
    let first_xref_overhead = {
        let mut len = b"xref\n".len();
        for (start, items) in &runs {
            len += format!("{start} {}\n", items.len()).len();
            len += 20 * items.len();
        }
        len
    };

    let lin_dict_len = {
        let dict = lin_dict_bytes(0, 0, 0, page1_num, 0, n_pages, 0);
        format!("{lin_num} 0 obj\n").len() + dict.len() + b"\nendobj\n".len()
    };
    let off_lin = header.len();
    let off_first_xref = off_lin + lin_dict_len;
    let off_part4 = off_first_xref + first_xref_overhead + first_trailer_len;
    let off_hint = off_part4 + part4_len;

    // Per-object spans by number (for offsets + page lengths).
    let span_of: BTreeMap<u32, usize> = bodies
        .iter()
        .map(|(num, b)| (*num, obj_span(*num, b)))
        .collect();

    // Byte offset of every object relative to the start of part 6 (= `off_part6`),
    // in the fixed physical-after-hint order. This is invariant to the hint-stream
    // length, so the generic (outline/thread) hint offsets resolve *before* the
    // hint fixed-point loop: a group's stored `first_object_offset` (absolute
    // offset minus the hint length) equals `off_hint + rel_off[first]`, exactly as
    // the first-page offset equals `off_hint`.
    let rel_off: BTreeMap<u32, usize> = {
        let mut cur = 0usize;
        let mut m: BTreeMap<u32, usize> = BTreeMap::new();
        let place = |num: u32, cur: &mut usize, m: &mut BTreeMap<u32, usize>| {
            if let std::collections::btree_map::Entry::Vacant(e) = m.entry(num) {
                e.insert(*cur);
                *cur += span_of[&num];
            }
        };
        for group in &part.page_groups {
            for &id in group {
                place(remap[&id], &mut cur, &mut m);
            }
        }
        for &id in part.part8.iter().chain(part.part9.iter()) {
            place(remap[&id], &mut cur, &mut m);
        }
        for &id in &ordered {
            place(remap[&id], &mut cur, &mut m);
        }
        m
    };

    // Generic hint tables for the contiguous outline / thread runs (part 9). The
    // first object owns the lowest offset in its run, so it is `first_object`.
    let generic_hint = |group: &[ObjectId]| -> Option<GenericHint> {
        let first_num = remap[group.first()?];
        let group_length: usize = group.iter().map(|id| span_of[&remap[id]]).sum();
        Some(GenericHint {
            first_object: first_num,
            first_object_offset: (off_hint + rel_off[&first_num]) as u32,
            nobjects: group.len() as u32,
            group_length: group_length as u32,
        })
    };
    let outline_hint = generic_hint(&part.outline_group);
    let thread_hint = generic_hint(&part.thread_group);

    // ── 6. Resolve the hint stream length (fixed-point on its own size). ──
    // Page lengths/content lengths derive from the part6/part7 layout, which starts
    // at `off_hint + hint_total_len`. Page length = byte span of the page's object
    // run; content length = the page's /Contents object span.
    let page_content_len: Vec<usize> = (0..n_pages as usize)
        .map(|pi| {
            let leaf = part.page_groups[pi][0];
            let content_ids: Vec<ObjectId> = objects
                .get(&leaf)
                .and_then(Object::as_dict)
                .and_then(|d| d.get(b"Contents"))
                .map(|c| {
                    let mut v = Vec::new();
                    collect_refs_pruned(c, &mut v, false);
                    v
                })
                .unwrap_or_default();
            content_ids
                .iter()
                .filter_map(|id| remap.get(id))
                .filter_map(|num| span_of.get(num))
                .sum()
        })
        .collect();
    let page_len: Vec<usize> = part
        .page_groups
        .iter()
        .map(|group| {
            group
                .iter()
                .filter_map(|id| remap.get(id))
                .filter_map(|num| span_of.get(num))
                .sum()
        })
        .collect();
    // Shared-object group lengths (part6 ++ part8), in shared-table order.
    let shared_group_len: Vec<usize> = shared_obj_nums
        .iter()
        .map(|num| span_of.get(num).copied().unwrap_or(1))
        .collect();

    let mut hint_total_len = 0usize;
    let mut hint_obj_bytes: Vec<u8> = Vec::new();
    for _ in 0..8 {
        // Offset of the first page's first object (start of part6).
        let off_part6 = off_hint + hint_total_len;
        let (payload, offs) = build_hint_payload(HintInput {
            first_page_obj_off: off_part6,
            hint_total_len,
            page_len: &page_len,
            page_content_len: &page_content_len,
            page_nobjects: &page_nobjects,
            page_shared_idx: &page_shared_idx,
            nshared_first_page,
            shared_group_len: &shared_group_len,
            first_shared_obj_num: shared_obj_nums.first().copied().unwrap_or(0),
            outline: outline_hint,
            thread: thread_hint,
        });
        let compressed = flate_encode(&payload);
        let mut hdict = Dictionary::new();
        // The hint data is Flate-compressed; the reader must inflate it before
        // parsing the bit-packed tables (without /Filter, qpdf reads the raw zlib
        // bytes as hint fields → bit-stream overflow).
        hdict.set(b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec()));
        hdict.set(b"Length".to_vec(), Object::Integer(compressed.len() as i64));
        hdict.set(b"S".to_vec(), Object::Integer(offs.shared as i64));
        if let Some(o) = offs.outline {
            hdict.set(b"O".to_vec(), Object::Integer(o as i64));
        }
        if let Some(a) = offs.thread {
            hdict.set(b"A".to_vec(), Object::Integer(a as i64));
        }
        let mut hbytes = Vec::new();
        hbytes.extend_from_slice(format!("{hint_num} 0 obj\n").as_bytes());
        write_dict(&mut hbytes, &hdict);
        hbytes.extend_from_slice(b"\nstream\n");
        hbytes.extend_from_slice(&compressed);
        hbytes.extend_from_slice(b"\nendstream\nendobj\n");
        let new_len = hbytes.len();
        hint_obj_bytes = hbytes;
        if new_len == hint_total_len {
            break;
        }
        hint_total_len = new_len;
    }

    // ── 7. Final absolute geometry + offsets. ──
    let off_part6 = off_hint + hint_total_len;
    let mut offsets: BTreeMap<u32, usize> = BTreeMap::new();
    offsets.insert(lin_num, off_lin);
    offsets.insert(hint_num, off_hint);
    // part4 offsets.
    {
        let mut cur = off_part4;
        for &id in &part.part4 {
            let num = remap[&id];
            offsets.insert(num, cur);
            cur += span_of[&num];
        }
    }
    // part6 .. tail offsets (everything after the hint), in physical order.
    // Physical order after hint = part6, part7 groups, part8, part9, tail — which
    // is exactly `ordered` minus the part4 prefix.
    let mut cur = off_part6;
    let mut end_first_page = off_part6;
    let place = |num: u32, cur: &mut usize, offsets: &mut BTreeMap<u32, usize>| {
        if let std::collections::btree_map::Entry::Vacant(e) = offsets.entry(num) {
            e.insert(*cur);
            *cur += span_of[&num];
        }
    };
    // Walk page groups (part6 = group 0, part7 = groups 1..) then part8/part9/tail.
    for (gi, group) in part.page_groups.iter().enumerate() {
        for &id in group {
            place(remap[&id], &mut cur, &mut offsets);
        }
        if gi == 0 {
            end_first_page = cur; // /E = end of the first-page section
        }
    }
    for &id in part.part8.iter().chain(part.part9.iter()) {
        place(remap[&id], &mut cur, &mut offsets);
    }
    // tail
    for &id in &ordered {
        place(remap[&id], &mut cur, &mut offsets);
    }
    let off_main_xref = cur;

    let main_size = n_total + 1; // objects 0..=n_total
    let main_subsection_header = format!("0 {main_size}\n");
    let t_value = off_main_xref + b"xref\n".len() + main_subsection_header.len() - 1;

    let root_ref = remap_refs(
        trailer
            .get(b"Root")
            .cloned()
            .as_ref()
            .unwrap_or(&Object::Null),
        &remap,
    );
    let info_ref = trailer
        .get(b"Info")
        .map(|o| remap_refs(o, &remap))
        .filter(|o| !matches!(o, Object::Null));

    let main_xref_bytes = build_main_xref_all(
        main_size,
        &offsets,
        &root_ref,
        info_ref.as_ref(),
        &id_obj,
        off_first_xref,
    );
    let total_len = off_main_xref + main_xref_bytes.len();

    let lin_dict = lin_dict_bytes(
        total_len,
        off_hint,
        hint_total_len,
        page1_num,
        end_first_page,
        n_pages,
        t_value,
    );
    let mut lin_obj_bytes = Vec::new();
    lin_obj_bytes.extend_from_slice(format!("{lin_num} 0 obj\n").as_bytes());
    lin_obj_bytes.extend_from_slice(&lin_dict);
    lin_obj_bytes.extend_from_slice(b"\nendobj\n");
    debug_assert_eq!(lin_obj_bytes.len(), lin_dict_len, "lin dict length drift");

    let first_xref_block = build_first_page_xref_all(
        &runs,
        &offsets,
        off_main_xref,
        main_size,
        &root_ref,
        info_ref.as_ref(),
        &id_obj,
    );
    debug_assert_eq!(
        first_xref_block.len(),
        first_xref_overhead + first_trailer_len,
        "first-page xref length drift"
    );

    // ── 8. Emit in physical order. ──
    let mut out: Vec<u8> = Vec::with_capacity(total_len);
    out.extend_from_slice(header);
    out.extend_from_slice(&lin_obj_bytes);
    out.extend_from_slice(&first_xref_block);
    // part4
    for &id in &part.part4 {
        let num = remap[&id];
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        out.extend_from_slice(&encode_body(&id));
        out.extend_from_slice(b"\nendobj\n");
    }
    // hint
    out.extend_from_slice(&hint_obj_bytes);
    // part6 .. tail, in physical order, each emitted exactly once.
    let mut emitted: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    emitted.extend(part.part4.iter().map(|id| remap[id]));
    let emit = |out: &mut Vec<u8>, emitted: &mut std::collections::BTreeSet<u32>, id: ObjectId| {
        let num = remap[&id];
        if emitted.insert(num) {
            out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
            out.extend_from_slice(&encode_body(&id));
            out.extend_from_slice(b"\nendobj\n");
        }
    };
    for group in &part.page_groups {
        for &id in group {
            emit(&mut out, &mut emitted, id);
        }
    }
    for &id in part.part8.iter().chain(part.part9.iter()) {
        emit(&mut out, &mut emitted, id);
    }
    for &id in &ordered {
        emit(&mut out, &mut emitted, id);
    }
    out.extend_from_slice(&main_xref_bytes);

    debug_assert_eq!(out.len(), total_len, "final /L drift");
    Some(out)
}

/// Contiguous runs of a sorted, de-duplicated number list, as `(start, members)`.
fn contiguous_runs(sorted: &[u32]) -> Vec<(u32, Vec<u32>)> {
    let mut runs: Vec<(u32, Vec<u32>)> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut members = vec![sorted[i]];
        let mut j = i;
        while j + 1 < sorted.len() && sorted[j + 1] == sorted[j] + 1 {
            members.push(sorted[j + 1]);
            j += 1;
        }
        runs.push((start, members));
        i = j + 1;
    }
    runs
}

/// The trailer `/ID` pair (two byte strings). If absent, a deterministic pair is
/// synthesized (linearized files want an `/ID`; the value is not security-bearing).
fn trailer_id_pair(trailer: &Dictionary) -> Object {
    if let Some(Object::Array(items)) = trailer.get(b"ID") {
        if items.len() == 2 {
            return Object::Array(items.clone());
        }
    }
    let zero = Object::String(vec![0u8; 16], crate::object::StringKind::Hex);
    Object::Array(vec![zero.clone(), zero])
}

/// Byte length of the first-page trailer, which we keep value-independent by
/// 10-padding every numeric field.
fn first_page_trailer_len(id_obj: &Object) -> usize {
    // We build it once with placeholder zeros to measure (lengths are fixed).
    build_first_page_trailer(0, 0, 0, &Object::Reference((1, 0)), None, id_obj).len()
}

/// The first-page trailer bytes: `trailer\n<< … >>\nstartxref\n0\n%%EOF\n`.
///
/// `/Size` and `/Prev` are written as **10-digit zero-padded** decimals (a valid
/// PDF integer literal, `0000001138` parses as `1138`) so the section's byte
/// length is invariant across the measure/emit passes — essential because `/Prev`
/// (the main-xref offset) is only known after the layout that depends on this very
/// length. `/Root`, optional `/Info` and `/ID` are written via the normal object
/// writer (their lengths are fixed by their content).
fn build_first_page_trailer(
    prev: usize,
    size: u32,
    _unused: u32,
    root_ref: &Object,
    info_ref: Option<&Object>,
    id_obj: &Object,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"trailer\n<<");
    out.extend_from_slice(format!(" /Size {size:010}").as_bytes());
    out.extend_from_slice(b" /Root ");
    write_object(&mut out, root_ref);
    if let Some(info) = info_ref {
        out.extend_from_slice(b" /Info ");
        write_object(&mut out, info);
    }
    out.extend_from_slice(format!(" /Prev {prev:010}").as_bytes());
    out.extend_from_slice(b" /ID ");
    write_object(&mut out, id_obj);
    out.extend_from_slice(b" >>");
    out.extend_from_slice(b"\nstartxref\n0\n%%EOF\n");
    out
}

/// Build the linearization parameter dictionary bytes (`<< … >>` only), with all
/// numeric fields written **10-digit zero-padded** so the object's byte length is
/// invariant across the measure/emit passes.
fn lin_dict_bytes(
    l: usize,
    h_off: usize,
    h_len: usize,
    o: u32,
    e: usize,
    n: u32,
    t: usize,
) -> Vec<u8> {
    // Keys ordered as qpdf writes them; values fixed-width.
    format!(
        "<< /Linearized 1 /L {:010} /H [ {:010} {:010} ] /O {:010} /E {:010} /N {:010} /T {:010} >>",
        l, h_off, h_len, o, e, n, t
    )
    .into_bytes()
}

/// Build the first-page cross-reference section + its trailer. Each object number
/// in `runs` is looked up in `offsets`; the trailer's `/Prev` chains to the main
/// xref. Per Annex F the final `startxref` (written in the main xref) points back
/// here, while this section's own `startxref` is `0`.
fn build_first_page_xref_all(
    runs: &[(u32, Vec<u32>)],
    offsets: &BTreeMap<u32, usize>,
    prev_main_xref: usize,
    main_size: u32,
    root_ref: &Object,
    info_ref: Option<&Object>,
    id_obj: &Object,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"xref\n");
    for (start, members) in runs {
        out.extend_from_slice(format!("{start} {}\n", members.len()).as_bytes());
        for &num in members {
            let off = offsets.get(&num).copied().unwrap_or(0);
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
    }
    out.extend_from_slice(&build_first_page_trailer(
        prev_main_xref,
        main_size,
        0,
        root_ref,
        info_ref,
        id_obj,
    ));
    out
}

/// Build the main cross-reference table (covers objects `0..=size-1`) + the final
/// trailer, whose `startxref` points back to the first-page xref (Annex F).
fn build_main_xref_all(
    size: u32,
    offsets: &BTreeMap<u32, usize>,
    root_ref: &Object,
    info_ref: Option<&Object>,
    id_obj: &Object,
    off_first_xref: usize,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"xref\n");
    out.extend_from_slice(format!("0 {size}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for num in 1..size {
        let off = offsets.get(&num).copied().unwrap_or(0);
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    // Final trailer (no /Prev; this is the document's last xref by chain order).
    let mut dict = Dictionary::new();
    dict.set(b"Size".to_vec(), Object::Integer(size as i64));
    dict.set(b"Root".to_vec(), root_ref.clone());
    if let Some(info) = info_ref {
        dict.set(b"Info".to_vec(), info.clone());
    }
    dict.set(b"ID".to_vec(), id_obj.clone());
    out.extend_from_slice(b"trailer\n");
    write_dict(&mut out, &dict);
    out.extend_from_slice(format!("\nstartxref\n{off_first_xref}\n%%EOF\n").as_bytes());
    out
}

/// A generic hint table (ISO 32000-1 §F.3): one contiguous group of objects
/// described by four 32-bit fields. Used for the document-outline (`/O`) and
/// thread-information (`/A`) hint tables.
#[derive(Clone, Copy)]
struct GenericHint {
    /// Object number of the first object in the group.
    first_object: u32,
    /// Byte offset of that object, minus the hint-stream length (the hint tables
    /// disregard the hint stream's own bytes, like every other stored offset).
    first_object_offset: u32,
    /// Number of objects in the group.
    nobjects: u32,
    /// Total byte length of the contiguous group.
    group_length: u32,
}

/// Byte offsets (within the decoded hint payload) of the hint tables, for the
/// hint-stream dictionary's `/S`, `/O` and `/A` entries.
struct HintOffsets {
    /// Shared-object hint table offset (`/S`, always present).
    shared: usize,
    /// Document-outline hint table offset (`/O`), if written.
    outline: Option<usize>,
    /// Thread-information hint table offset (`/A`), if written.
    thread: Option<usize>,
}

/// Inputs to [`build_hint_payload`], one entry per page / shared object.
struct HintInput<'a> {
    /// Absolute byte offset of the first page's first object (= start of part 6).
    first_page_obj_off: usize,
    /// The hint stream object's total byte length (subtracted from offsets per the
    /// qpdf/Acrobat convention — hint-table offsets disregard the hint stream).
    hint_total_len: usize,
    /// Page length (byte span of the page's object run) per page.
    page_len: &'a [usize],
    /// Page content-stream length per page.
    page_content_len: &'a [usize],
    /// Number of objects in each page's run.
    page_nobjects: &'a [u32],
    /// Shared-object indices each page references (empty for page 0).
    page_shared_idx: &'a [Vec<u32>],
    /// Number of shared entries that live in the first page (= `|part6|`).
    nshared_first_page: usize,
    /// Group (= object) byte length of each shared object, in shared-table order
    /// (`part6` then `part8`).
    shared_group_len: &'a [usize],
    /// Object number of the first shared object in `part8` region — only used (and
    /// only meaningful) when `nshared_total > nshared_first_page`.
    first_shared_obj_num: u32,
    /// Document-outline generic hint table (`/O`), if the document has one.
    outline: Option<GenericHint>,
    /// Thread-information generic hint table (`/A`), if the document has threads.
    thread: Option<GenericHint>,
}

/// Build the primary hint stream payload (decoded bytes): the **page-offset hint
/// table** (ISO 32000-1 §F.3.1), the **shared-object hint table** (§F.3.2), and —
/// when present — the **document-outline** and **thread-information** generic hint
/// tables (§F.3). Returns `(payload, offsets)` where `offsets` gives each table's
/// byte offset within the decoded payload (the hint stream's `/S`, `/O`, `/A`).
///
/// The byte/bit layout reproduces exactly what qpdf writes (and re-validates):
///
/// * Both tables are written **column-major**: for each field, every page's (or
///   every shared object's) value is written in sequence, then the bit cursor is
///   **byte-aligned** before the next field (`BitWriter::pad`). This is the layout
///   `write_vector_int`/`write_vector_vector` produce.
/// * Page-offset header (13 fixed fields), then columns: `delta_nobjects`,
///   `delta_page_length`, `nshared_objects`, `shared_identifiers` (all pages'
///   lists), `shared_numerators`, `delta_content_offset`, `delta_content_length`.
/// * Offsets in the header (`first_page_offset`, `first_shared_offset`) are written
///   **minus the hint-stream length** because the hint table disregards itself
///   (qpdf's `adjusted_offset` adds it back on read).
/// * Field widths: `nbits_delta_nobjects = nbits(max−min nobjects)`,
///   `nbits_delta_page_length = nbits(max−min length)`,
///   `nbits_nshared_objects = nbits(max nshared)`,
///   `nbits_shared_identifier = nbits(nshared_total)`,
///   `nbits_delta_content_length = nbits_delta_page_length`, content offset 0.
/// * Shared header (7 fields), then columns: `delta_group_length`,
///   `signature_present` (1 bit each, all 0), `nobjects_minus_one` (0 bits).
/// * Each generic hint table (outline, thread) is four big-endian 32-bit fields
///   (`first_object`, `first_object_offset`, `nobjects`, `group_length`), written
///   on byte boundaries.
fn build_hint_payload(input: HintInput) -> (Vec<u8>, HintOffsets) {
    let n_pages = input.page_len.len();
    let nshared_total = input.shared_group_len.len();

    // ── page-offset column statistics ──
    let min_nobjects = input.page_nobjects.iter().copied().min().unwrap_or(1);
    let max_nobjects = input.page_nobjects.iter().copied().max().unwrap_or(1);
    let min_length = input.page_len.iter().copied().min().unwrap_or(1);
    let max_length = input.page_len.iter().copied().max().unwrap_or(1);
    let max_shared = input
        .page_shared_idx
        .iter()
        .map(|v| v.len() as u32)
        .max()
        .unwrap_or(0);

    let nbits_delta_nobjects = bit_width((max_nobjects - min_nobjects) as u64);
    let nbits_delta_page_length = bit_width((max_length - min_length) as u64);
    let nbits_nshared_objects = bit_width(max_shared as u64);
    let nbits_shared_identifier = bit_width(nshared_total as u64);
    let nbits_shared_numerator = 0u32;
    let nbits_delta_content_offset = 0u32;
    let min_content_offset = 0u32;
    // Page-offset hint table content-stream fields (header items 8/9, ISO 32000-1
    // Table 132): the *real* least content-stream length and the bit width of
    // (greatest − least) content length — a column independent of page length, not
    // a copy of it. Acrobat/qpdf ignore these on read, but they now reflect the
    // actual content streams so a strict viewer can locate them.
    let min_content_length = input.page_content_len.iter().copied().min().unwrap_or(0);
    let max_content_length = input.page_content_len.iter().copied().max().unwrap_or(0);
    let nbits_delta_content_length = bit_width((max_content_length - min_content_length) as u64);
    let shared_denominator = 4u32;

    // Header field 2: first page's first-object offset, minus the hint length.
    let first_page_offset = input
        .first_page_obj_off
        .saturating_sub(input.hint_total_len) as u32;

    let mut header: Vec<u8> = Vec::new();
    let put_u32 = |h: &mut Vec<u8>, v: u32| h.extend_from_slice(&v.to_be_bytes());
    let put_u16 = |h: &mut Vec<u8>, v: u16| h.extend_from_slice(&v.to_be_bytes());
    put_u32(&mut header, min_nobjects); // 1
    put_u32(&mut header, first_page_offset); // 2
    put_u16(&mut header, nbits_delta_nobjects as u16); // 3
    put_u32(&mut header, min_length as u32); // 4
    put_u16(&mut header, nbits_delta_page_length as u16); // 5
    put_u32(&mut header, min_content_offset); // 6
    put_u16(&mut header, nbits_delta_content_offset as u16); // 7
    put_u32(&mut header, min_content_length as u32); // 8
    put_u16(&mut header, nbits_delta_content_length as u16); // 9
    put_u16(&mut header, nbits_nshared_objects as u16); // 10
    put_u16(&mut header, nbits_shared_identifier as u16); // 11
    put_u16(&mut header, nbits_shared_numerator as u16); // 12
    put_u16(&mut header, shared_denominator as u16); // 13

    // ── per-page records, column-major with byte alignment between columns ──
    let mut bw = BitWriter::new();
    // delta_nobjects
    for i in 0..n_pages {
        bw.put(
            (input.page_nobjects[i] - min_nobjects) as u64,
            nbits_delta_nobjects,
        );
    }
    bw.pad();
    // delta_page_length
    for i in 0..n_pages {
        bw.put(
            (input.page_len[i] - min_length) as u64,
            nbits_delta_page_length,
        );
    }
    bw.pad();
    // nshared_objects
    for i in 0..n_pages {
        bw.put(input.page_shared_idx[i].len() as u64, nbits_nshared_objects);
    }
    bw.pad();
    // shared_identifiers (all pages' lists, in order)
    for idxs in input.page_shared_idx {
        for &idx in idxs {
            bw.put(idx as u64, nbits_shared_identifier);
        }
    }
    bw.pad();
    // shared_numerators (0 bits → nothing, but still byte-aligned)
    bw.pad();
    // delta_content_offset (0 bits)
    bw.pad();
    // delta_content_length: the page's real content-stream length minus the least
    // content length (header item 8) — its own column, not the page-length delta.
    for i in 0..n_pages {
        let dcl = input.page_content_len[i].saturating_sub(min_content_length);
        bw.put(dcl as u64, nbits_delta_content_length);
    }
    bw.pad();
    let page_records = bw.into_bytes();

    let mut payload = Vec::new();
    payload.extend_from_slice(&header);
    payload.extend_from_slice(&page_records);
    let shared_off = payload.len();

    // ── shared-object hint table ──
    let min_group_length = input.shared_group_len.iter().copied().min().unwrap_or(1);
    let max_group_length = input.shared_group_len.iter().copied().max().unwrap_or(1);
    let nbits_delta_group_length = bit_width((max_group_length - min_group_length) as u64);
    let nbits_nobjects = 0u32; // one object per group
    let nshared_first_page = input.nshared_first_page as u32;
    // first_shared_obj / first_shared_offset only matter when part 8 is non-empty.
    let (first_shared_obj, first_shared_offset) = if nshared_total as u32 > nshared_first_page {
        // Offset of the first part-8 shared object, minus the hint length.
        let off = input
            .shared_group_len
            .iter()
            .take(input.nshared_first_page)
            .sum::<usize>()
            + input.first_page_obj_off;
        (
            input.first_shared_obj_num,
            off.saturating_sub(input.hint_total_len) as u32,
        )
    } else {
        (0u32, 0u32)
    };

    let mut sheader: Vec<u8> = Vec::new();
    put_u32(&mut sheader, first_shared_obj); // 1
    put_u32(&mut sheader, first_shared_offset); // 2
    put_u32(&mut sheader, nshared_first_page); // 3
    put_u32(&mut sheader, nshared_total as u32); // 4
    put_u16(&mut sheader, nbits_nobjects as u16); // 5
    put_u32(&mut sheader, min_group_length as u32); // 6
    put_u16(&mut sheader, nbits_delta_group_length as u16); // 7

    let mut sbw = BitWriter::new();
    // delta_group_length column
    for &len in input.shared_group_len {
        sbw.put((len - min_group_length) as u64, nbits_delta_group_length);
    }
    sbw.pad();
    // signature_present column (1 bit each, all 0)
    for _ in 0..nshared_total {
        sbw.put(0, 1);
    }
    sbw.pad();
    // nobjects_minus_one column (0 bits)
    sbw.pad();
    let shared_records = sbw.into_bytes();

    payload.extend_from_slice(&sheader);
    payload.extend_from_slice(&shared_records);

    // ── generic hint tables (outline `/O`, thread `/A`) ──
    // Each is four big-endian 32-bit fields on a byte boundary (the shared records
    // end byte-aligned). Their decoded-payload offsets feed the hint-stream dict.
    let put_generic = |payload: &mut Vec<u8>, g: &GenericHint| {
        payload.extend_from_slice(&g.first_object.to_be_bytes());
        payload.extend_from_slice(&g.first_object_offset.to_be_bytes());
        payload.extend_from_slice(&g.nobjects.to_be_bytes());
        payload.extend_from_slice(&g.group_length.to_be_bytes());
    };
    let outline_off = input.outline.as_ref().map(|g| {
        let off = payload.len();
        put_generic(&mut payload, g);
        off
    });
    let thread_off = input.thread.as_ref().map(|g| {
        let off = payload.len();
        put_generic(&mut payload, g);
        off
    });

    (
        payload,
        HintOffsets {
            shared: shared_off,
            outline: outline_off,
            thread: thread_off,
        },
    )
}

/// Read the byte offset in the most recent `startxref` of a serialized PDF — the
/// value an [`append_incremental_update`] must chain to via `/Prev`. Scans from
/// the end for the last `startxref` keyword. `None` if absent or unparsable.
pub fn last_startxref(pdf: &[u8]) -> Option<usize> {
    let keyword = b"startxref";
    let pos = pdf.windows(keyword.len()).rposition(|w| w == keyword)?;
    let rest = &pdf[pos + keyword.len()..];
    let digits: Vec<u8> = rest
        .iter()
        .copied()
        .skip_while(|b| b.is_ascii_whitespace())
        .take_while(u8::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

/// Read the trailer `/Size` of a serialized PDF — the next free object number is
/// this value (objects are numbered `1..Size-1`, with `0` the free-list head).
/// Scans the last `trailer` dictionary for `/Size`. `None` if absent.
pub fn last_size(pdf: &[u8]) -> Option<u32> {
    let keyword = b"/Size";
    let pos = pdf.windows(keyword.len()).rposition(|w| w == keyword)?;
    let rest = &pdf[pos + keyword.len()..];
    let digits: Vec<u8> = rest
        .iter()
        .copied()
        .skip_while(|b| b.is_ascii_whitespace())
        .take_while(u8::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    std::str::from_utf8(&digits).ok()?.parse().ok()
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
        Object::Array(items) => Object::Array(items.iter().map(|o| remap_refs(o, map)).collect()),
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

    // ── issue #77: linearization (dcl + outline/thread hints), version header,
    //    incremental xref-stream updates ──────────────────────────────────────

    use crate::object::{Stream, StringKind};
    use crate::Document;

    fn mkdict(pairs: Vec<(&str, Object)>) -> Dictionary {
        let mut d = Dictionary::new();
        for (k, v) in pairs {
            d.set(k.as_bytes().to_vec(), v);
        }
        d
    }
    fn tref(n: u32) -> Object {
        Object::Reference((n, 0))
    }
    fn tname(s: &[u8]) -> Object {
        Object::Name(s.to_vec())
    }
    fn tint(n: i64) -> Object {
        Object::Integer(n)
    }
    fn tstr(s: &[u8]) -> Object {
        Object::String(s.to_vec(), StringKind::Literal)
    }
    fn mediabox() -> Object {
        Object::Array(vec![tint(0), tint(0), tint(200), tint(200)])
    }
    fn content(body: &[u8]) -> Object {
        Object::Stream(Stream {
            dict: Dictionary::new(),
            raw: body.to_vec(),
        })
    }

    /// A small but valid 2-page document object map (+ trailer), with optional
    /// `/Outlines` (a 2-item outline) and `/Threads` (one article: thread + info +
    /// bead). Object numbers: 1 catalog, 2 pages, 3/5 page objects, 4/6 contents,
    /// 7 outlines + 8/9 items, 10 thread + 11 info + 12 bead.
    fn lin_objects(
        with_outline: bool,
        with_threads: bool,
    ) -> (BTreeMap<ObjectId, Object>, Dictionary) {
        let mut objects: BTreeMap<ObjectId, Object> = BTreeMap::new();
        let mut catalog = vec![("Type", tname(b"Catalog")), ("Pages", tref(2))];
        if with_outline {
            catalog.push(("Outlines", tref(7)));
        }
        if with_threads {
            catalog.push(("Threads", Object::Array(vec![tref(10)])));
        }
        objects.insert((1, 0), Object::Dictionary(mkdict(catalog)));
        objects.insert(
            (2, 0),
            Object::Dictionary(mkdict(vec![
                ("Type", tname(b"Pages")),
                ("Kids", Object::Array(vec![tref(3), tref(5)])),
                ("Count", tint(2)),
            ])),
        );
        objects.insert(
            (3, 0),
            Object::Dictionary(mkdict(vec![
                ("Type", tname(b"Page")),
                ("Parent", tref(2)),
                ("MediaBox", mediabox()),
                ("Contents", tref(4)),
                ("Resources", Object::Dictionary(Dictionary::new())),
            ])),
        );
        // Distinct, non-trivial content lengths so the content-length column is
        // visibly the real value (and clearly < the page length).
        objects.insert(
            (4, 0),
            content(b"0 0 200 200 re f 10 10 50 50 re f 80 80 40 40 re f 5 5 3 3 re f"),
        );
        objects.insert(
            (5, 0),
            Object::Dictionary(mkdict(vec![
                ("Type", tname(b"Page")),
                ("Parent", tref(2)),
                ("MediaBox", mediabox()),
                ("Contents", tref(6)),
                ("Resources", Object::Dictionary(Dictionary::new())),
            ])),
        );
        objects.insert((6, 0), content(b"20 20 30 30 re f"));
        if with_outline {
            objects.insert(
                (7, 0),
                Object::Dictionary(mkdict(vec![
                    ("Type", tname(b"Outlines")),
                    ("First", tref(8)),
                    ("Last", tref(9)),
                    ("Count", tint(2)),
                ])),
            );
            objects.insert(
                (8, 0),
                Object::Dictionary(mkdict(vec![
                    ("Title", tstr(b"Chapter 1")),
                    ("Parent", tref(7)),
                    ("Next", tref(9)),
                    ("Dest", Object::Array(vec![tref(3), tname(b"Fit")])),
                ])),
            );
            objects.insert(
                (9, 0),
                Object::Dictionary(mkdict(vec![
                    ("Title", tstr(b"Chapter 2")),
                    ("Parent", tref(7)),
                    ("Prev", tref(8)),
                    ("Dest", Object::Array(vec![tref(5), tname(b"Fit")])),
                ])),
            );
        }
        if with_threads {
            objects.insert(
                (10, 0),
                Object::Dictionary(mkdict(vec![
                    ("Type", tname(b"Thread")),
                    ("I", tref(11)),
                    ("F", tref(12)),
                ])),
            );
            objects.insert(
                (11, 0),
                Object::Dictionary(mkdict(vec![("Title", tstr(b"Article 1"))])),
            );
            objects.insert(
                (12, 0),
                Object::Dictionary(mkdict(vec![
                    ("Type", tname(b"Bead")),
                    ("T", tref(10)),
                    ("N", tref(12)),
                    ("V", tref(12)),
                    ("P", tref(3)),
                    (
                        "R",
                        Object::Array(vec![tint(0), tint(0), tint(100), tint(100)]),
                    ),
                ])),
            );
        }
        let trailer = mkdict(vec![("Root", tref(1)), ("Size", tint(13))]);
        (objects, trailer)
    }

    fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// Find `needle` and parse the (space-prefixed) ASCII integer right after it.
    fn int_after(hay: &[u8], needle: &[u8]) -> Option<u64> {
        let p = find_sub(hay, needle)? + needle.len();
        let digits: Vec<u8> = hay[p..]
            .iter()
            .copied()
            .skip_while(|b| *b == b' ')
            .take_while(u8::is_ascii_digit)
            .collect();
        std::str::from_utf8(&digits).ok()?.parse().ok()
    }

    fn be_u32(b: &[u8], off: usize) -> u32 {
        u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
    }

    /// Extract `(decoded_hint_payload, hint_object_dict_bytes)` from a linearized
    /// PDF by following the `/H` offset, reading `/Length`, and inflating the stream.
    fn extract_hint(pdf: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let hoff = int_after(pdf, b"/H [ ").expect("/H offset") as usize;
        let obj = &pdf[hoff..];
        let stream_at = find_sub(obj, b"stream\n").expect("hint stream keyword");
        let dict = obj[..stream_at].to_vec();
        let len = int_after(&dict, b"/Length ").expect("hint /Length") as usize;
        let start = stream_at + b"stream\n".len();
        let compressed = &obj[start..start + len];
        let decoded = crate::filters::inflate::flate_decode(compressed).expect("inflate hint");
        (decoded, dict)
    }

    fn qpdf_available() -> bool {
        std::process::Command::new("qpdf")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn qpdf_combined(pdf: &[u8], label: &str) -> Option<String> {
        if !qpdf_available() {
            eprintln!("qpdf not available; skipping --check for {label}");
            return None;
        }
        let path = std::env::temp_dir().join(format!("gp77_{label}_{}.pdf", std::process::id()));
        std::fs::write(&path, pdf).unwrap();
        let out = std::process::Command::new("qpdf")
            .arg("--check")
            .arg(&path)
            .output()
            .expect("run qpdf --check");
        let _ = std::fs::remove_file(&path);
        Some(format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }

    /// Assert qpdf reports the file linearized with no warnings/errors (skipped if
    /// qpdf is absent). qpdf re-derives the hint tables and compares, so this is the
    /// real validation of the page/shared/outline hint tables.
    fn qpdf_check_linearized(pdf: &[u8], label: &str) {
        let Some(combined) = qpdf_combined(pdf, label) else {
            return;
        };
        assert!(
            combined.contains("File is linearized"),
            "qpdf should report {label} linearized:\n{combined}"
        );
        assert!(
            !combined.to_lowercase().contains("warning") && !combined.contains("error encountered"),
            "qpdf --check warnings/errors for {label}:\n{combined}"
        );
    }

    /// Assert qpdf reports no warnings/errors (the file need not be linearized).
    fn qpdf_check_ok(pdf: &[u8], label: &str) {
        let Some(combined) = qpdf_combined(pdf, label) else {
            return;
        };
        assert!(
            !combined.to_lowercase().contains("warning") && !combined.contains("error encountered"),
            "qpdf --check warnings/errors for {label}:\n{combined}"
        );
    }

    // ── sub-item 1: the content-length hint column carries the real value ──
    #[test]
    fn linearized_hint_carries_real_content_length() {
        let (objects, trailer) = lin_objects(false, false);
        let lin = to_linearized(&objects, &trailer).expect("linearize");
        assert_eq!(Document::open(&lin).unwrap().page_count(), 2);

        let (decoded, _) = extract_hint(&lin);
        // Page-offset header: item 4 (min_page_length) @10, item 8 (min_content_length) @22.
        let min_page_length = be_u32(&decoded, 10);
        let min_content_length = be_u32(&decoded, 22);
        assert!(min_content_length > 0, "a real content length is recorded");
        assert!(
            min_content_length < min_page_length,
            "content length ({min_content_length}) is the real content-stream length, \
             strictly below the page length ({min_page_length}) — not a copy of it"
        );
        qpdf_check_linearized(&lin, "dcl");
    }

    // ── sub-item 2: outline hint table (qpdf-validated) ──
    #[test]
    fn linearized_outline_hint_table_emitted_and_valid() {
        let (objects, trailer) = lin_objects(true, false);
        let lin = to_linearized(&objects, &trailer).expect("linearize");
        let doc = Document::open(&lin).expect("reopen");
        assert_eq!(doc.page_count(), 2);
        assert_eq!(
            doc.outline_items().len(),
            2,
            "outline survives linearization"
        );

        let (decoded, dict) = extract_hint(&lin);
        let o = int_after(&dict, b"/O ").expect("hint dict has /O") as usize;
        // Generic table fields: first_object @o, offset @o+4, nobjects @o+8, group_length @o+12.
        assert_eq!(be_u32(&decoded, o + 8), 3, "/Outlines + 2 items");
        assert!(be_u32(&decoded, o + 12) > 0, "non-zero group length");
        // qpdf recomputes the outline group and compares to our /O table.
        qpdf_check_linearized(&lin, "outline");
    }

    // ── sub-item 2: thread-information hint table ──
    #[test]
    fn linearized_thread_hint_table_emitted() {
        let (objects, trailer) = lin_objects(false, true);
        let lin = to_linearized(&objects, &trailer).expect("linearize");
        assert_eq!(Document::open(&lin).unwrap().page_count(), 2);

        let (decoded, dict) = extract_hint(&lin);
        let a = int_after(&dict, b"/A ").expect("hint dict has /A") as usize;
        assert_eq!(be_u32(&decoded, a + 8), 3, "thread + info + bead");
        assert!(be_u32(&decoded, a + 12) > 0, "non-zero group length");
        // qpdf ignores /A — the file must stay warning-free.
        qpdf_check_linearized(&lin, "thread");
    }

    #[test]
    fn linearized_outline_and_thread_coexist() {
        let (objects, trailer) = lin_objects(true, true);
        let lin = to_linearized(&objects, &trailer).expect("linearize");
        assert_eq!(Document::open(&lin).unwrap().page_count(), 2);
        let (_, dict) = extract_hint(&lin);
        assert!(int_after(&dict, b"/O ").is_some(), "/O present");
        assert!(int_after(&dict, b"/A ").is_some(), "/A present");
        qpdf_check_linearized(&lin, "outline+thread");
    }

    #[test]
    fn linearized_without_outline_or_threads_omits_o_and_a() {
        let (objects, trailer) = lin_objects(false, false);
        let lin = to_linearized(&objects, &trailer).expect("linearize");
        let (_, dict) = extract_hint(&lin);
        assert!(int_after(&dict, b"/O ").is_none(), "no /O without outline");
        assert!(int_after(&dict, b"/A ").is_none(), "no /A without threads");
        assert!(int_after(&dict, b"/S ").is_some(), "/S always present");
    }

    // ── sub-item 3: incremental update matches the base's xref form ──

    /// Append (as an incremental update) a new page wired into a re-emitted page
    /// tree, so the added page is only reachable through the new xref. Returns the
    /// updated bytes and the base `/Size`.
    fn add_page_update(base: &[u8]) -> (Vec<u8>, u32) {
        let prev = last_startxref(base).unwrap();
        let prev_size = last_size(base).unwrap();
        let new_page = prev_size; // next free number
        let page = Object::Dictionary(mkdict(vec![
            ("Type", tname(b"Page")),
            ("Parent", tref(2)),
            ("MediaBox", mediabox()),
        ]));
        let pages = Object::Dictionary(mkdict(vec![
            ("Type", tname(b"Pages")),
            (
                "Kids",
                Object::Array(vec![tref(3), tref(5), tref(new_page)]),
            ),
            ("Count", tint(3)),
        ]));
        let new = vec![(new_page, 0u16, page), (2u32, 0u16, pages)];
        let size = new_page + 1; // one past the new object
        let bytes = append_incremental_update(base, &new, prev, size, tref(1), None);
        (bytes, prev_size)
    }

    #[test]
    fn incremental_xref_stream_update_round_trips() {
        let (objects, trailer) = lin_objects(false, false);
        let base = to_pdf_compressed(&objects, &trailer, false); // xref-stream base
        assert!(
            find_sub(&base, b"/Type /XRef").is_some(),
            "base uses an xref stream"
        );

        let (updated, prev_size) = add_page_update(&base);
        let appended = &updated[base.len()..];
        assert!(
            find_sub(appended, b"/Type /XRef").is_some(),
            "the incremental update is itself an xref stream"
        );
        assert!(
            find_sub(appended, b"\nxref\n").is_none(),
            "no classic xref table in the update"
        );
        let doc = Document::open(&updated).expect("reopen");
        assert_eq!(
            doc.page_count(),
            3,
            "added page is visible through the xref-stream update"
        );
        assert_eq!(
            last_size(&updated),
            Some(prev_size + 2),
            "the xref stream object consumed the next free number"
        );
        qpdf_check_ok(&updated, "incr_xrefstream");
    }

    #[test]
    fn incremental_classic_update_round_trips() {
        let (objects, trailer) = lin_objects(false, false);
        let base = to_pdf(&objects, &trailer); // classic xref base
        let (updated, prev_size) = add_page_update(&base);
        let appended = &updated[base.len()..];
        assert!(
            find_sub(appended, b"xref\n").is_some(),
            "classic xref table"
        );
        assert!(find_sub(appended, b"trailer").is_some(), "classic trailer");
        assert!(
            find_sub(appended, b"/Type /XRef").is_none(),
            "classic base keeps a classic update"
        );
        let doc = Document::open(&updated).expect("reopen");
        assert_eq!(doc.page_count(), 3);
        assert_eq!(
            last_size(&updated),
            Some(prev_size + 1),
            "classic /Size = one past the new object"
        );
        qpdf_check_ok(&updated, "incr_classic");
    }

    // ── sub-item 4: version-selectable header ──
    #[test]
    fn version_header_selects_banner() {
        assert_eq!(PdfVersion::default(), PdfVersion::V1_7);
        let (objects, trailer) = lin_objects(false, false);

        let v17 =
            to_pdf_compressed_with_header(&objects, &trailer, true, PdfVersion::V1_7.header());
        assert!(v17.starts_with(b"%PDF-1.7"));
        let v20 =
            to_pdf_compressed_with_header(&objects, &trailer, true, PdfVersion::V2_0.header());
        assert!(v20.starts_with(b"%PDF-2.0"));
        // The bare wrapper keeps the historical 1.5 banner.
        assert!(to_pdf_compressed(&objects, &trailer, true).starts_with(b"%PDF-1.5"));
        // The linearized writer honours the header too.
        let lin20 =
            to_linearized_with_header(&objects, &trailer, PdfVersion::V2_0.header()).unwrap();
        assert!(lin20.starts_with(b"%PDF-2.0"));

        for pdf in [&v17, &v20, &lin20] {
            assert_eq!(Document::open(pdf).unwrap().page_count(), 2, "round-trips");
        }
    }
}
