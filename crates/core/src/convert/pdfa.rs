//! PDF/A metadata (XMP packet) for the archival export.
//!
//! [`Document::to_pdfa`](crate::Document::to_pdfa) adds the structural pieces of
//! PDF/A conformance — this XMP identification packet plus an sRGB
//! OutputIntent (see [`super::srgb_icc`]). The exact conformance level is chosen
//! by [`PdfaLevel`]. Full conformance additionally requires every font embedded;
//! that's documented on `to_pdfa`.

use std::collections::BTreeMap;

use crate::object::{Object, ObjectId};

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
/// [`PdfaLevel`], with a Dublin Core title and a PDF producer.
pub fn xmp_metadata(level: PdfaLevel, title: &str, producer: &str) -> Vec<u8> {
    let (mut t, mut p) = (String::new(), String::new());
    xml_escape(title, &mut t);
    xml_escape(producer, &mut p);
    let (part, conformance) = (level.part(), level.conformance());
    // The leading BOM + fixed packet id are part of the XMP convention.
    let xmp = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
  <rdf:Description rdf:about=\"\" xmlns:pdfaid=\"http://www.aiim.org/pdfa/ns/id/\">\n\
   <pdfaid:part>{part}</pdfaid:part>\n\
   <pdfaid:conformance>{conformance}</pdfaid:conformance>\n\
  </rdf:Description>\n\
  <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n\
   <dc:title><rdf:Alt><rdf:li xml:lang=\"x-default\">{t}</rdf:li></rdf:Alt></dc:title>\n\
  </rdf:Description>\n\
  <rdf:Description rdf:about=\"\" xmlns:pdf=\"http://ns.adobe.com/pdf/1.3/\">\n\
   <pdf:Producer>{p}</pdf:Producer>\n\
  </rdf:Description>\n\
 </rdf:RDF>\n\
</x:xmpmeta>\n\
<?xpacket end=\"w\"?>"
    );
    xmp.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmp_identifies_pdfa_2b() {
        let xmp = String::from_utf8(xmp_metadata(PdfaLevel::Pdfa2b, "My <Title>", "GigaPDF")).unwrap();
        assert!(xmp.contains("<pdfaid:part>2</pdfaid:part>"));
        assert!(xmp.contains("<pdfaid:conformance>B</pdfaid:conformance>"));
        assert!(xmp.contains("My &lt;Title&gt;"), "title escaped");
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
            let xmp = String::from_utf8(xmp_metadata(level, "t", "p")).unwrap();
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

    #[test]
    fn default_level_is_2b() {
        assert_eq!(PdfaLevel::default(), PdfaLevel::Pdfa2b);
    }

    use crate::object::{Dictionary, Stream};

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
