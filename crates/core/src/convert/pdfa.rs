//! PDF/A metadata (XMP packet) for the archival export.
//!
//! [`Document::to_pdfa`](crate::Document::to_pdfa) adds the structural pieces of
//! PDF/A conformance — this XMP identification packet plus an sRGB
//! OutputIntent (see [`super::srgb_icc`]). The exact conformance level is chosen
//! by [`PdfaLevel`]. Full conformance additionally requires every font embedded;
//! that's documented on `to_pdfa`.

use std::collections::BTreeMap;

use crate::object::{Dictionary, Object, ObjectId, Stream};

/// PDF/A conformance level selectable on [`Document::to_pdfa`](crate::Document::to_pdfa).
///
/// All three conformance flavours are modelled: **level B** ("basic", visual
/// reproduction), **level U** ("Unicode": B + every glyph `/ToUnicode`-mapped)
/// and **level A** ("accessible": a *Tagged PDF* — B/U + a logical structure
/// tree with marked content, ISO 19005-1 §6.8 / 19005-2 §6.8). Level A is built
/// by [`super::tagged`] from the structure the engine reconstructs.
///
/// | Variant | ISO standard | PDF base | XMP `part`/`conformance` | veraPDF flavour | Tagged |
/// |---------|--------------|----------|--------------------------|-----------------|--------|
/// | [`Pdfa1b`](Self::Pdfa1b) | 19005-1 | 1.4 | `1` / `B` | `1b` | no |
/// | [`Pdfa1a`](Self::Pdfa1a) | 19005-1 | 1.4 | `1` / `A` | `1a` | yes |
/// | [`Pdfa2b`](Self::Pdfa2b) | 19005-2 | 1.7 | `2` / `B` | `2b` | no |
/// | [`Pdfa2u`](Self::Pdfa2u) | 19005-2 | 1.7 | `2` / `U` | `2u` | no |
/// | [`Pdfa2a`](Self::Pdfa2a) | 19005-2 | 1.7 | `2` / `A` | `2a` | yes |
/// | [`Pdfa3b`](Self::Pdfa3b) | 19005-3 | 1.7 | `3` / `B` | `3b` | no |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PdfaLevel {
    /// PDF/A-1b — ISO 19005-1, based on PDF 1.4 (classic xref, no object streams).
    Pdfa1b,
    /// PDF/A-1a — ISO 19005-1 **Tagged PDF** (level A): 1b + a logical structure
    /// tree, marked content and Unicode mapping. Built by [`super::tagged`].
    Pdfa1a,
    /// PDF/A-2b — ISO 19005-2, based on PDF 1.7. The historical default.
    #[default]
    Pdfa2b,
    /// PDF/A-2u — like 2b but additionally requires every glyph to be Unicode-
    /// mapped (a `/ToUnicode` CMap on each font). See [`Document::to_pdfa`].
    Pdfa2u,
    /// PDF/A-2a — ISO 19005-2 **Tagged PDF** (level A): 2u + a logical structure
    /// tree and marked content. Built by [`super::tagged`].
    Pdfa2a,
    /// PDF/A-3b — ISO 19005-3, based on PDF 1.7; permits embedded file
    /// attachments (`/AF`).
    Pdfa3b,
}

impl PdfaLevel {
    /// XMP `pdfaid:part` digit (`1`, `2`, `3`).
    fn part(self) -> u8 {
        match self {
            PdfaLevel::Pdfa1b | PdfaLevel::Pdfa1a => 1,
            PdfaLevel::Pdfa2b | PdfaLevel::Pdfa2u | PdfaLevel::Pdfa2a => 2,
            PdfaLevel::Pdfa3b => 3,
        }
    }

    /// XMP `pdfaid:conformance` letter (`A`, `B` or `U`).
    fn conformance(self) -> char {
        match self {
            PdfaLevel::Pdfa1a | PdfaLevel::Pdfa2a => 'A',
            PdfaLevel::Pdfa2u => 'U',
            _ => 'B',
        }
    }

    /// Whether this is a **level A** flavour — a Tagged PDF requiring a logical
    /// structure tree, marked content and a `/MarkInfo` flag (built by
    /// [`super::tagged`]).
    pub(crate) fn is_tagged(self) -> bool {
        matches!(self, PdfaLevel::Pdfa1a | PdfaLevel::Pdfa2a)
    }

    /// The file-header bytes the level mandates: PDF/A-1 is built on PDF 1.4,
    /// every later part on PDF 1.7. ISO 19005-1 §6.1.2 requires the header to
    /// declare 1.4 (or lower); a `%PDF-1.7` header would fail veraPDF for 1b/1a.
    pub(crate) fn header(self) -> &'static [u8] {
        match self {
            PdfaLevel::Pdfa1b | PdfaLevel::Pdfa1a => b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n",
            _ => b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n",
        }
    }
}

/// Strip the graphics-state / appearance constructs that ISO 19005-2 forbids,
/// in-place over an object map (operate on `Document::to_pdfa`'s working clone,
/// never on the live document).
///
/// Three rules are normalised, each a key-level removal that **cannot alter the
/// rendered result** — the keys carry no on-screen geometry, only interactivity
/// (`/AP` alternates), an information-only CID inventory, or a transfer function
/// that PDF/A bans outright:
///
/// * **6.2.5** — an `ExtGState` dictionary must not contain `/TR` (nor the
///   deprecated `/TR2`). `/TR` / `/TR2` are *only* defined inside an
///   `ExtGState` (ISO 32000-1 Table 58), so removing them wherever they occur
///   has no other meaning and is safe.
/// * **6.2.11.4.2** — if a CID font's `FontDescriptor` carries a `/CIDSet`, it
///   must list every CID present in the embedded program. Rather than recompute
///   a possibly-stale inherited set, we drop `/CIDSet` (it is optional and
///   purely informative; `/CIDSet` is only defined inside a CIDFont's
///   descriptor, so the removal is unambiguous).
/// * **6.3.3** — for every annotation appearance dictionary (`/AP`), the value
///   must contain only the `/N` (normal) entry; the `/D` (down) and `/R`
///   (rollover) alternates are removed. They affect interactive feedback only,
///   not the printed/normal appearance.
///
/// All three removals are idempotent.
pub(crate) fn sanitize_objects(objects: &mut BTreeMap<ObjectId, Object>) {
    for obj in objects.values_mut() {
        sanitize_object(obj);
    }
}

/// Recursively apply the PDF/A key-level normalisations to `obj` and everything
/// nested under it (dictionaries can sit inline inside arrays, other dicts, or a
/// stream's dictionary — e.g. inline `ExtGState` resources or an `/AP` value).
fn sanitize_object(obj: &mut Object) {
    match obj {
        Object::Dictionary(dict) => sanitize_dict_then_recurse(dict),
        Object::Stream(stream) => sanitize_dict_then_recurse(&mut stream.dict),
        Object::Array(items) => {
            for item in items {
                sanitize_object(item);
            }
        }
        _ => {}
    }
}

fn sanitize_dict_then_recurse(dict: &mut crate::object::Dictionary) {
    // ExtGState 6.2.5 — drop the (only-here-defined) transfer-function keys.
    dict.remove(b"TR");
    dict.remove(b"TR2");
    // CIDFont 6.2.11.4.2 — drop the optional, possibly-incomplete CID inventory.
    dict.remove(b"CIDSet");
    // Annotation 6.3.3 — an /AP appearance subdictionary keeps only /N.
    if let Some(Object::Dictionary(ap)) = dict.0.get_mut(b"AP".as_slice()) {
        if ap.contains(b"N") {
            ap.0.retain(|key, _| key.as_slice() == b"N");
        }
    }
    // Recurse into the remaining values (after the key removals above).
    for value in dict.0.values_mut() {
        sanitize_object(value);
    }
}

// ─── forbidden-construct removal: document JavaScript ─────────────────────────

/// Strip every **document-level JavaScript** trigger ISO 19005 forbids
/// (§6.6.1 / §6.9 — no `Action` of type `JavaScript`, no document-level scripts),
/// operating in-place over the working clone. Three sites are cleared on the
/// catalog `catalog_id` points at:
///
/// * `/Names /JavaScript` — the name-tree of document-level scripts run on open
///   (the whole `/JavaScript` entry is removed; an emptied `/Names` is left
///   otherwise intact so non-JS name trees — `/Dests`, `/EmbeddedFiles` — survive).
/// * `/OpenAction` — removed only when it is (or resolves to) a `/S /JavaScript`
///   action; a plain go-to `/OpenAction` (a destination) is permitted and kept.
/// * `/AA` — the catalog's additional-actions dictionary (document `WillClose` /
///   `WillSave` … triggers), which exists *only* to host actions and is dropped
///   wholesale.
///
/// Returns `true` when anything was removed. Idempotent — a second call finds
/// nothing left to strip. Any JS action streams left unreferenced are simply not
/// re-serialized.
pub(crate) fn strip_javascript(
    objects: &mut BTreeMap<ObjectId, Object>,
    catalog_id: ObjectId,
) -> bool {
    let Some(mut catalog) = objects.get(&catalog_id).and_then(Object::as_dict).cloned() else {
        return false;
    };
    let mut changed = false;

    // 1. /OpenAction — drop only a JavaScript action (keep a go-to destination).
    if let Some(action) = catalog.get(b"OpenAction") {
        if action_is_javascript(objects, action) {
            catalog.remove(b"OpenAction");
            changed = true;
        }
    }

    // 2. /AA — document additional-actions dict hosts only actions; drop it whole.
    if catalog.contains(b"AA") {
        catalog.remove(b"AA");
        changed = true;
    }

    // 3. /Names /JavaScript — the document-level script name tree. Mutate the
    //    /Names dict in place (inline or indirect) so sibling name trees survive.
    match catalog.0.get(b"Names".as_slice()).cloned() {
        Some(Object::Reference(names_id)) => {
            if let Some(mut names) = objects.get(&names_id).and_then(Object::as_dict).cloned() {
                if names.contains(b"JavaScript") {
                    names.remove(b"JavaScript");
                    objects.insert(names_id, Object::Dictionary(names));
                    changed = true;
                }
            }
        }
        Some(Object::Dictionary(mut names)) => {
            if names.contains(b"JavaScript") {
                names.remove(b"JavaScript");
                catalog.set(b"Names".to_vec(), Object::Dictionary(names));
                changed = true;
            }
        }
        _ => {}
    }

    if changed {
        objects.insert(catalog_id, Object::Dictionary(catalog));
    }
    changed
}

/// Whether `action` is (or, when indirect, resolves within `objects` to) an
/// action dictionary whose `/S` is `/JavaScript`.
fn action_is_javascript(objects: &BTreeMap<ObjectId, Object>, action: &Object) -> bool {
    let dict = match action {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => objects.get(id).and_then(Object::as_dict),
        _ => None,
    };
    dict.and_then(|d| d.get(b"S")).and_then(Object::as_name) == Some(b"JavaScript".as_slice())
}

// ─── font embedding / substitution (ISO 19005 §6.2.11.4.1 / §6.3.7) ──────────

/// Embed a program for **every font the PDF/A output references but does not
/// embed**, so the result satisfies the rule that all fonts be embedded
/// (ISO 19005-1 §6.3.4 / 19005-2 §6.2.11.4.1). Operates in-place over the working
/// clone; `next_free` is the first unused object number. Returns the new first-
/// unused number after any objects were appended.
///
/// The engine ships no copy of the original (absent) faces, so embedding here is
/// **substitution with a bundled, metric-compatible standard face** (Liberation
/// Sans — `crate::font::bundled`). Layout is preserved because the visible advances
/// come from the font dictionary's own width data, which is kept verbatim:
///
/// * **Simple fonts** (`/Type1`, `/TrueType`, `/MMType1`): a complete
///   `/FontDescriptor` carrying a `/FontFile2` (the substitute) is attached and
///   `/Subtype` is normalised to `/TrueType` (a simple TrueType font is the only
///   simple kind that may carry a `FontFile2`). The existing `/Encoding`,
///   `/Widths`, `/FirstChar`, `/LastChar` are left untouched; a base-14 font that
///   declared none gets a `/Widths` array measured from the substitute (so the
///   dictionary and program agree, §6.2.11.5) over `0..=255` of its WinAnsi
///   encoding — those widths match the standard metrics the substitute reproduces.
/// * **Composite fonts** (`/Type0`): the descendant `CIDFontType2`'s descriptor
///   gets the substitute as `/FontFile2` with `/CIDToGIDMap /Identity`; the
///   descendant's `/W`/`/DW` advances are kept, and a `/ToUnicode` CMap is
///   synthesised on the Type0 when it lacks one so text stays searchable.
///
/// A single shared `/FontFile2` stream backs every substituted font (one bundled
/// program, referenced many times). Fonts that are already embedded, and the
/// base-14 standard-14 set when they *remain* legal for the level, are unaffected
/// — but PDF/A forbids non-embedded fonts at every level, so the standard-14
/// faces are substituted here too.
pub(crate) fn embed_or_substitute_fonts(
    objects: &mut BTreeMap<ObjectId, Object>,
    next_free: u32,
) -> u32 {
    // Collect the font object ids needing work first (immutable borrow), then
    // mutate — a font dict and its descendant CIDFont are distinct objects.
    let mut simple: Vec<ObjectId> = Vec::new();
    let mut composite: Vec<ObjectId> = Vec::new();
    for (&id, obj) in objects.iter() {
        let Some(dict) = obj.as_dict() else { continue };
        if dict.get(b"Type").and_then(Object::as_name) != Some(b"Font".as_slice()) {
            continue;
        }
        match dict.get(b"Subtype").and_then(Object::as_name) {
            Some(b"Type0") => {
                if !composite_is_embedded(objects, dict) {
                    composite.push(id);
                }
            }
            Some(b"Type1") | Some(b"TrueType") | Some(b"MMType1") => {
                if !descriptor_is_embedded(objects, dict.get(b"FontDescriptor")) {
                    simple.push(id);
                }
            }
            // A CIDFont reached on its own (no Type0 parent in this map) is
            // handled via its parent; standalone we leave it to avoid double work.
            _ => {}
        }
    }

    if simple.is_empty() && composite.is_empty() {
        return next_free;
    }

    // One shared substitute program (the bundled face) for every font fixed here.
    let Some(face) =
        crate::font::bundled::bundled_program_for_base14(crate::font::bundled::Base14::Sans)
    else {
        return next_free; // bundled face failed to parse (covered by its own test)
    };
    let mut next = next_free;
    let fontfile_id = (next, 0u16);
    next += 1;
    objects.insert(fontfile_id, Object::Stream(substitute_fontfile_stream()));

    for id in simple {
        next = embed_simple_font(objects, id, fontfile_id, face, next);
    }
    for id in composite {
        next = embed_composite_font(objects, id, fontfile_id, face, next);
    }
    next
}

/// The bundled substitute program as an uncompressed `/FontFile2` stream
/// (`/Length` + `/Length1` set to the glyf TrueType program length, as PDF/A
/// requires a `FontFile2` to declare `/Length1`).
fn substitute_fontfile_stream() -> Stream {
    let program = crate::font::bundled::FALLBACK_TTF;
    let mut dict = Dictionary::new();
    dict.set(b"Length", Object::Integer(program.len() as i64));
    dict.set(b"Length1", Object::Integer(program.len() as i64));
    Stream::new(dict, program.to_vec())
}

/// Whether a simple font's `/FontDescriptor` (passed by its dictionary value)
/// already embeds a program (`/FontFile`/`2`/`3`).
fn descriptor_is_embedded(
    objects: &BTreeMap<ObjectId, Object>,
    descriptor: Option<&Object>,
) -> bool {
    deref_dict(objects, descriptor).is_some_and(|fd| {
        fd.contains(b"FontFile") || fd.contains(b"FontFile2") || fd.contains(b"FontFile3")
    })
}

/// Whether a `/Type0` font already embeds a program, looking through its
/// descendant CIDFont's `/FontDescriptor`.
fn composite_is_embedded(objects: &BTreeMap<ObjectId, Object>, type0: &Dictionary) -> bool {
    let descriptor = deref_array(objects, type0.get(b"DescendantFonts"))
        .and_then(|a| a.first())
        .and_then(|o| deref_dict(objects, Some(o)))
        .and_then(|cid| cid.get(b"FontDescriptor").cloned());
    descriptor_is_embedded(objects, descriptor.as_ref())
}

/// Attach the substitute to a **simple** font: build a `/FontDescriptor`
/// referencing the shared `/FontFile2`, normalise `/Subtype` to `/TrueType`, and
/// synthesise `/Widths` from the substitute when the font declares none. Returns
/// the new next-free object number (a `/FontDescriptor` object is appended).
fn embed_simple_font(
    objects: &mut BTreeMap<ObjectId, Object>,
    font_id: ObjectId,
    fontfile_id: ObjectId,
    face: &crate::font::truetype::TrueTypeFont,
    next_free: u32,
) -> u32 {
    let Some(mut font) = objects.get(&font_id).and_then(Object::as_dict).cloned() else {
        return next_free;
    };
    let base_name = font
        .get(b"BaseFont")
        .and_then(Object::as_name)
        .map_or_else(|| b"FallbackSans".to_vec(), <[u8]>::to_vec);

    let descriptor = font_descriptor(&base_name, fontfile_id, face);
    let fd_id = (next_free, 0u16);
    objects.insert(fd_id, Object::Dictionary(descriptor));

    // A simple font carrying a FontFile2 must be /TrueType (a /Type1 with a
    // FontFile2 is invalid). Encoding stays — the substitute resolves WinAnsi /
    // Differences code points through its (3,1) cmap.
    font.set(b"Subtype".to_vec(), Object::Name(b"TrueType".to_vec()));
    font.set(b"FontDescriptor".to_vec(), Object::Reference(fd_id));

    // Width consistency (§6.2.11.5): when the dict declares no /Widths (a base-14
    // font), measure them from the substitute over the WinAnsi code range so the
    // dictionary and the embedded program agree.
    if !font.contains(b"Widths") {
        let (first, widths) = winansi_widths(face);
        font.set(b"FirstChar".to_vec(), Object::Integer(first as i64));
        font.set(
            b"LastChar".to_vec(),
            Object::Integer((first as usize + widths.len() - 1) as i64),
        );
        font.set(b"Widths".to_vec(), Object::Array(widths));
        // The substitute is non-symbolic WinAnsi; make the encoding explicit if
        // the base-14 font left it implicit, so codes map deterministically.
        if !font.contains(b"Encoding") {
            font.set(
                b"Encoding".to_vec(),
                Object::Name(b"WinAnsiEncoding".to_vec()),
            );
        }
    }

    objects.insert(font_id, Object::Dictionary(font));
    next_free + 1
}

/// Attach the substitute to a **composite** (`/Type0`) font: give the descendant
/// `CIDFontType2`'s descriptor the shared `/FontFile2` + `/CIDToGIDMap /Identity`,
/// and synthesise a `/ToUnicode` on the Type0 if it has none. The descendant's
/// `/W`/`/DW` advances are preserved. Returns the new next-free object number.
fn embed_composite_font(
    objects: &mut BTreeMap<ObjectId, Object>,
    type0_id: ObjectId,
    fontfile_id: ObjectId,
    face: &crate::font::truetype::TrueTypeFont,
    next_free: u32,
) -> u32 {
    let Some(type0) = objects.get(&type0_id).and_then(Object::as_dict).cloned() else {
        return next_free;
    };
    // Resolve the descendant CIDFont object id (it is an array of one reference).
    let Some(cid_id) = deref_array(objects, type0.get(b"DescendantFonts"))
        .and_then(|a| a.first().cloned())
        .and_then(|o| o.as_reference())
    else {
        return next_free;
    };
    let Some(mut cid) = objects.get(&cid_id).and_then(Object::as_dict).cloned() else {
        return next_free;
    };

    let base_name = cid
        .get(b"BaseFont")
        .or_else(|| type0.get(b"BaseFont"))
        .and_then(Object::as_name)
        .map_or_else(|| b"FallbackSans".to_vec(), <[u8]>::to_vec);

    let mut descriptor = font_descriptor(&base_name, fontfile_id, face);
    // A CID font's descriptor may carry a /CIDSet; the substitute makes any prior
    // one stale, and the sanitiser drops it anyway — don't add one.
    descriptor.remove(b"CIDSet");
    let fd_id = (next_free, 0u16);
    let mut next = next_free + 1;
    objects.insert(fd_id, Object::Dictionary(descriptor));

    cid.set(b"FontDescriptor".to_vec(), Object::Reference(fd_id));
    // The substitute's glyph ids are addressed directly (CID == GID).
    cid.set(b"CIDToGIDMap".to_vec(), Object::Name(b"Identity".to_vec()));
    if !cid.contains(b"DW") {
        cid.set(b"DW".to_vec(), Object::Integer(1000));
    }
    objects.insert(cid_id, Object::Dictionary(cid));

    // Ensure the Type0 has a /ToUnicode so extracted text stays meaningful. Only
    // synthesise one when absent (don't clobber a faithful map from the source).
    if !type0.contains(b"ToUnicode") {
        let pairs = crate::font::embed::gid_to_unicode(face);
        let cmap = crate::font::embed::to_unicode_cmap(&pairs);
        let mut tu_dict = Dictionary::new();
        tu_dict.set(b"Length", Object::Integer(cmap.len() as i64));
        let tu_id = (next, 0u16);
        next += 1;
        objects.insert(tu_id, Object::Stream(Stream::new(tu_dict, cmap)));
        let mut t0 = type0;
        t0.set(b"ToUnicode".to_vec(), Object::Reference(tu_id));
        objects.insert(type0_id, Object::Dictionary(t0));
    }
    next
}

/// Build a complete `/FontDescriptor` for the substitute face, with `/Flags` and
/// metrics derived from the original `/BaseFont` name (serif/sans/mono, bold,
/// italic) and the substitute program (FontBBox, ascent/descent scaled to the PDF
/// 1000-unit glyph space). `fontfile_id` is the shared `/FontFile2` stream.
fn font_descriptor(
    base_name: &[u8],
    fontfile_id: ObjectId,
    face: &crate::font::truetype::TrueTypeFont,
) -> Dictionary {
    let name = String::from_utf8_lossy(base_name);
    let flags = descriptor_flags(&name);
    let scale = 1000.0 / face.units_per_em();
    let scaled = |v: f64| (v * scale).round() as i64;

    let mut fd = Dictionary::new();
    fd.set(b"Type", Object::Name(b"FontDescriptor".to_vec()));
    fd.set(b"FontName", Object::Name(base_name.to_vec()));
    fd.set(b"Flags", Object::Integer(flags as i64));
    // A non-degenerate FontBBox in PDF glyph space (the substitute's em box).
    fd.set(
        b"FontBBox",
        Object::Array(vec![
            Object::Integer(scaled(-200.0)),
            Object::Integer(scaled(-300.0)),
            Object::Integer(scaled(face.units_per_em() + 200.0)),
            Object::Integer(scaled(face.units_per_em())),
        ]),
    );
    fd.set(
        b"ItalicAngle",
        Object::Integer(if flags & FLAG_ITALIC != 0 { -12 } else { 0 }),
    );
    fd.set(
        b"Ascent",
        Object::Integer(scaled(0.905 * face.units_per_em())),
    );
    fd.set(
        b"Descent",
        Object::Integer(scaled(-0.212 * face.units_per_em())),
    );
    fd.set(
        b"CapHeight",
        Object::Integer(scaled(0.716 * face.units_per_em())),
    );
    // /StemV is required; pick a heavier nominal value for a bold face.
    fd.set(
        b"StemV",
        Object::Integer(if flags & FLAG_FORCE_BOLD != 0 {
            140
        } else {
            80
        }),
    );
    fd.set(b"FontFile2", Object::Reference(fontfile_id));
    fd
}

// `/Flags` bits (ISO 32000-1 Table 121, 1-indexed positions as 0-indexed masks).
const FLAG_FIXED_PITCH: u32 = 1 << 0; // bit 1
const FLAG_SERIF: u32 = 1 << 1; // bit 2
const FLAG_NONSYMBOLIC: u32 = 1 << 5; // bit 6
const FLAG_ITALIC: u32 = 1 << 6; // bit 7
const FLAG_FORCE_BOLD: u32 = 1 << 18; // bit 19

/// Compute a `/FontDescriptor` `/Flags` value from a `/BaseFont` name: classify
/// serif/sans/mono via the base-14 mapping, and read bold/italic from the name's
/// style tokens. Always non-symbolic (the substitute is a Latin text face).
fn descriptor_flags(base_font: &str) -> u32 {
    let mut flags = FLAG_NONSYMBOLIC;
    match crate::font::bundled::base14_kind(base_font) {
        Some(crate::font::bundled::Base14::Serif) => flags |= FLAG_SERIF,
        Some(crate::font::bundled::Base14::Mono) => flags |= FLAG_FIXED_PITCH,
        _ => {}
    }
    let lower = base_font.to_ascii_lowercase();
    if lower.contains("bold") {
        flags |= FLAG_FORCE_BOLD;
    }
    if lower.contains("italic") || lower.contains("oblique") {
        flags |= FLAG_ITALIC;
    }
    flags
}

/// A `/Widths` array for a WinAnsi simple font, measured from the substitute
/// program over codes `0..=255`: each code's WinAnsi Unicode scalar is mapped to
/// a glyph and its advance scaled to the PDF 1000-unit glyph space (missing
/// glyphs contribute `0`). Returns `(first_char, widths)` with `first_char = 0`.
fn winansi_widths(face: &crate::font::truetype::TrueTypeFont) -> (u8, Vec<Object>) {
    let scale = 1000.0 / face.units_per_em();
    let widths = (0u16..=255)
        .map(|code| {
            // WinAnsi byte → Unicode scalar → substitute glyph → advance. Codes
            // WinAnsi leaves undefined decode to U+FFFD, which the face won't map,
            // so they contribute a 0 width (never drawn).
            let cp = crate::font::winansi_to_char(code as u8) as u32;
            let advance = face
                .gid_for_unicode(cp)
                .map_or(0.0, |gid| face.advance_width(gid));
            Object::Integer((advance * scale).round() as i64)
        })
        .collect();
    (0, widths)
}

/// Resolve an (optionally indirect) object to a `&Dictionary` within `objects`.
fn deref_dict<'a>(
    objects: &'a BTreeMap<ObjectId, Object>,
    obj: Option<&'a Object>,
) -> Option<&'a Dictionary> {
    match obj? {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => objects.get(id).and_then(Object::as_dict),
        _ => None,
    }
}

/// Resolve an (optionally indirect) object to a `&[Object]` array within `objects`.
fn deref_array<'a>(
    objects: &'a BTreeMap<ObjectId, Object>,
    obj: Option<&'a Object>,
) -> Option<&'a [Object]> {
    match obj? {
        Object::Array(a) => Some(a),
        Object::Reference(id) => objects.get(id).and_then(Object::as_array),
        _ => None,
    }
}

pub(crate) fn xml_escape(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
}

/// Build the XMP metadata packet identifying the file at the requested
/// [`PdfaLevel`], carrying the document's **real** metadata from `meta` so the XMP
/// agrees with the `/Info` dictionary (ISO 19005 §6.7.3 / 19005-2 §6.6.2.1): a
/// `pdfaid` block (part/conformance), plus the supplied Dublin Core, `pdf:` and
/// `xmp:` properties. Every absent field is simply omitted (the `pdfaid` block is
/// always present; the `dc:`/`pdf:`/`xmp:` blocks only when they carry something).
pub fn xmp_metadata(level: PdfaLevel, meta: &crate::document::InfoFields) -> Vec<u8> {
    let esc = |s: &str| {
        let mut o = String::new();
        xml_escape(s, &mut o);
        o
    };
    let (part, conformance) = (level.part(), level.conformance());

    // Dublin Core: title, creator (author), description (subject).
    let mut dc = String::new();
    if let Some(t) = &meta.title {
        dc.push_str(&format!(
            "\n   <dc:title><rdf:Alt><rdf:li xml:lang=\"x-default\">{}</rdf:li></rdf:Alt></dc:title>",
            esc(t)
        ));
    }
    if let Some(a) = &meta.author {
        dc.push_str(&format!(
            "\n   <dc:creator><rdf:Seq><rdf:li>{}</rdf:li></rdf:Seq></dc:creator>",
            esc(a)
        ));
    }
    if let Some(s) = &meta.subject {
        dc.push_str(&format!(
            "\n   <dc:description><rdf:Alt><rdf:li xml:lang=\"x-default\">{}</rdf:li></rdf:Alt></dc:description>",
            esc(s)
        ));
    }

    // Adobe PDF namespace: producer, keywords.
    let mut pdf_ns = String::new();
    if let Some(p) = &meta.producer {
        pdf_ns.push_str(&format!("\n   <pdf:Producer>{}</pdf:Producer>", esc(p)));
    }
    if let Some(k) = &meta.keywords {
        pdf_ns.push_str(&format!("\n   <pdf:Keywords>{}</pdf:Keywords>", esc(k)));
    }

    // XMP basic namespace: creator tool, create/modify dates (ISO-8601).
    let mut xmp_ns = String::new();
    if let Some(c) = &meta.creator {
        xmp_ns.push_str(&format!(
            "\n   <xmp:CreatorTool>{}</xmp:CreatorTool>",
            esc(c)
        ));
    }
    if let Some(d) = meta
        .creation_date
        .as_deref()
        .and_then(crate::document::pdf_date_to_iso8601)
    {
        xmp_ns.push_str(&format!("\n   <xmp:CreateDate>{d}</xmp:CreateDate>"));
    }
    if let Some(d) = meta
        .mod_date
        .as_deref()
        .and_then(crate::document::pdf_date_to_iso8601)
    {
        xmp_ns.push_str(&format!("\n   <xmp:ModifyDate>{d}</xmp:ModifyDate>"));
    }

    let block = |ns_attr: &str, body: &str| -> String {
        if body.is_empty() {
            String::new()
        } else {
            format!("\n  <rdf:Description rdf:about=\"\" {ns_attr}>{body}\n  </rdf:Description>")
        }
    };

    // The leading BOM + fixed packet id are part of the XMP convention. The
    // `pdfaid` block is mandatory and always emitted first.
    let xmp = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
  <rdf:Description rdf:about=\"\" xmlns:pdfaid=\"http://www.aiim.org/pdfa/ns/id/\">\n\
   <pdfaid:part>{part}</pdfaid:part>\n\
   <pdfaid:conformance>{conformance}</pdfaid:conformance>\n\
  </rdf:Description>{}{}{}\n\
 </rdf:RDF>\n\
</x:xmpmeta>\n\
<?xpacket end=\"w\"?>",
        block("xmlns:dc=\"http://purl.org/dc/elements/1.1/\"", &dc),
        block("xmlns:pdf=\"http://ns.adobe.com/pdf/1.3/\"", &pdf_ns),
        block("xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"", &xmp_ns),
    );
    xmp.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `InfoFields` carrying just a title + producer, for the XMP tests.
    fn meta(title: &str, producer: &str) -> crate::document::InfoFields {
        crate::document::InfoFields {
            title: Some(title.to_string()),
            producer: Some(producer.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn xmp_identifies_pdfa_2b() {
        let xmp = String::from_utf8(xmp_metadata(
            PdfaLevel::Pdfa2b,
            &meta("My <Title>", "GigaPDF"),
        ))
        .unwrap();
        assert!(xmp.contains("<pdfaid:part>2</pdfaid:part>"));
        assert!(xmp.contains("<pdfaid:conformance>B</pdfaid:conformance>"));
        assert!(xmp.contains("My &lt;Title&gt;"), "title escaped");
        assert!(
            xmp.contains("<pdf:Producer>GigaPDF</pdf:Producer>"),
            "producer present"
        );
        assert!(xmp.starts_with("<?xpacket begin"));
        assert!(xmp.trim_end().ends_with("<?xpacket end=\"w\"?>"));
    }

    /// Each [`PdfaLevel`] emits the matching `pdfaid:part`/`conformance` pair and
    /// the file header its ISO part mandates (PDF/A-1 → 1.4, others → 1.7).
    #[test]
    fn xmp_and_header_match_every_level() {
        for (level, part, conf, header) in [
            (PdfaLevel::Pdfa1b, "1", "B", &b"%PDF-1.4"[..]),
            (PdfaLevel::Pdfa2b, "2", "B", &b"%PDF-1.7"[..]),
            (PdfaLevel::Pdfa2u, "2", "U", &b"%PDF-1.7"[..]),
            (PdfaLevel::Pdfa3b, "3", "B", &b"%PDF-1.7"[..]),
        ] {
            let xmp = String::from_utf8(xmp_metadata(level, &meta("t", "p"))).unwrap();
            assert!(
                xmp.contains(&format!("<pdfaid:part>{part}</pdfaid:part>")),
                "{level:?} part"
            );
            assert!(
                xmp.contains(&format!("<pdfaid:conformance>{conf}</pdfaid:conformance>")),
                "{level:?} conformance"
            );
            assert!(level.header().starts_with(header), "{level:?} header");
        }
    }

    /// The XMP packet reflects the real document metadata (every field), not a
    /// hard-coded title/producer.
    #[test]
    fn xmp_carries_full_metadata() {
        let fields = crate::document::InfoFields {
            title: Some("Quarterly Report".to_string()),
            author: Some("Jane Doe".to_string()),
            subject: Some("Finance".to_string()),
            keywords: Some("q3,finance".to_string()),
            creator: Some("Acme Writer".to_string()),
            producer: Some("Acme PDF".to_string()),
            creation_date: Some("D:20260101120000Z".to_string()),
            mod_date: None,
        };
        let xmp = String::from_utf8(xmp_metadata(PdfaLevel::Pdfa2b, &fields)).unwrap();
        assert!(xmp.contains("Quarterly Report"), "dc:title");
        assert!(xmp.contains("Jane Doe"), "dc:creator");
        assert!(xmp.contains("Finance"), "dc:description");
        assert!(
            xmp.contains("<pdf:Keywords>q3,finance</pdf:Keywords>"),
            "pdf:Keywords"
        );
        assert!(
            xmp.contains("<xmp:CreatorTool>Acme Writer</xmp:CreatorTool>"),
            "creator tool"
        );
        assert!(
            xmp.contains("<pdf:Producer>Acme PDF</pdf:Producer>"),
            "producer"
        );
        assert!(
            xmp.contains("<xmp:CreateDate>2026-01-01T12:00:00Z</xmp:CreateDate>"),
            "create date"
        );
    }

    #[test]
    fn default_level_is_2b() {
        assert_eq!(PdfaLevel::default(), PdfaLevel::Pdfa2b);
    }

    /// `sanitize_objects` removes the keys ISO 19005-2 forbids — `ExtGState /TR`
    /// (§6.2.5), CID `/CIDSet` (§6.2.11.4.2) — while leaving every other entry
    /// untouched, and reaches dictionaries nested inside streams.
    #[test]
    fn sanitize_strips_tr_and_cidset_keeps_rest() {
        let mut gs = Dictionary::new();
        gs.set(b"Type", Object::Name(b"ExtGState".to_vec()));
        gs.set(b"TR", Object::Name(b"Identity".to_vec()));
        gs.set(b"TR2", Object::Name(b"Default".to_vec()));
        gs.set(b"ca", Object::Real(0.5));

        let mut fd = Dictionary::new();
        fd.set(b"Type", Object::Name(b"FontDescriptor".to_vec()));
        fd.set(b"CIDSet", Object::Reference((9, 0)));
        fd.set(b"Flags", Object::Integer(4));

        // The font descriptor lives inside a stream's dictionary to prove the
        // recursion descends into stream dicts too.
        let stream_obj = Object::Stream(Stream::new(fd, b"raw".to_vec()));

        let mut objects: BTreeMap<ObjectId, Object> = BTreeMap::new();
        objects.insert((1, 0), Object::Dictionary(gs));
        objects.insert((2, 0), stream_obj);

        sanitize_objects(&mut objects);

        let gs = objects[&(1, 0)].as_dict().unwrap();
        assert!(!gs.contains(b"TR"), "/TR removed from ExtGState");
        assert!(!gs.contains(b"TR2"), "/TR2 removed from ExtGState");
        assert!(gs.contains(b"ca"), "unrelated /ca preserved");

        let fd = objects[&(2, 0)].as_dict().unwrap();
        assert!(!fd.contains(b"CIDSet"), "/CIDSet removed from descriptor");
        assert!(fd.contains(b"Flags"), "unrelated /Flags preserved");
    }

    /// An annotation `/AP` dictionary is reduced to its `/N` entry (§6.3.3); the
    /// `/D` and `/R` alternates are dropped and the rest of the annotation is
    /// left intact.
    #[test]
    fn sanitize_reduces_ap_to_normal_appearance() {
        let mut ap = Dictionary::new();
        ap.set(b"N", Object::Reference((10, 0)));
        ap.set(b"D", Object::Reference((11, 0)));
        ap.set(b"R", Object::Reference((12, 0)));

        let mut annot = Dictionary::new();
        annot.set(b"Type", Object::Name(b"Annot".to_vec()));
        annot.set(b"Subtype", Object::Name(b"Widget".to_vec()));
        annot.set(b"AP", Object::Dictionary(ap));
        annot.set(b"Rect", Object::Array(vec![Object::Integer(0)]));

        let mut objects: BTreeMap<ObjectId, Object> = BTreeMap::new();
        objects.insert((1, 0), Object::Dictionary(annot));
        sanitize_objects(&mut objects);

        let annot = objects[&(1, 0)].as_dict().unwrap();
        assert!(annot.contains(b"Rect"), "annotation body preserved");
        let ap = annot.get(b"AP").and_then(Object::as_dict).unwrap();
        assert!(ap.contains(b"N"), "/N kept");
        assert!(!ap.contains(b"D"), "/D dropped");
        assert!(!ap.contains(b"R"), "/R dropped");
        assert_eq!(ap.0.len(), 1, "AP holds only /N");
    }
}
