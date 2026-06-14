//! A from-scratch PDF page builder — the keystone for every `*→PDF` conversion.
//!
//! Reverse conversions (TXT/HTML/RTF/Office → PDF) all reduce to "lay positioned
//! text + boxes + images onto pages", which this builder does using the
//! standard-14 fonts (WinAnsi) — zero embedded font bytes. Coordinates are
//! **top-down points** (origin top-left), matching the conversion model; the
//! builder flips to PDF's bottom-up space internally.
//!
//! Text outside WinAnsi (CP1252) — CJK, most non-Latin — is substituted with
//! `?`; for full Unicode the caller embeds a font and uses
//! [`Document::add_text`](crate::Document::add_text) instead.

use crate::object::{Dictionary, Object, ObjectId, Stream, StringKind};
use std::collections::BTreeMap;

/// A standard-14 font face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdFont {
    Helvetica,
    HelveticaBold,
    HelveticaOblique,
    HelveticaBoldOblique,
    Times,
    TimesBold,
    TimesItalic,
    Courier,
    CourierBold,
}

impl StdFont {
    fn base_font(self) -> &'static str {
        match self {
            StdFont::Helvetica => "Helvetica",
            StdFont::HelveticaBold => "Helvetica-Bold",
            StdFont::HelveticaOblique => "Helvetica-Oblique",
            StdFont::HelveticaBoldOblique => "Helvetica-BoldOblique",
            StdFont::Times => "Times-Roman",
            StdFont::TimesBold => "Times-Bold",
            StdFont::TimesItalic => "Times-Italic",
            StdFont::Courier => "Courier",
            StdFont::CourierBold => "Courier-Bold",
        }
    }

    fn resource_name(self) -> &'static str {
        match self {
            StdFont::Helvetica => "F1",
            StdFont::HelveticaBold => "F2",
            StdFont::HelveticaOblique => "F3",
            StdFont::HelveticaBoldOblique => "F4",
            StdFont::Times => "F5",
            StdFont::TimesBold => "F6",
            StdFont::TimesItalic => "F7",
            StdFont::Courier => "F8",
            StdFont::CourierBold => "F9",
        }
    }

    const ALL: [StdFont; 9] = [
        StdFont::Helvetica,
        StdFont::HelveticaBold,
        StdFont::HelveticaOblique,
        StdFont::HelveticaBoldOblique,
        StdFont::Times,
        StdFont::TimesBold,
        StdFont::TimesItalic,
        StdFont::Courier,
        StdFont::CourierBold,
    ];

    /// Pick a face from generic family + weight/style flags.
    pub fn pick(serif: bool, mono: bool, bold: bool, italic: bool) -> StdFont {
        if mono {
            return if bold { StdFont::CourierBold } else { StdFont::Courier };
        }
        if serif {
            return match (bold, italic) {
                (true, _) => StdFont::TimesBold,
                (false, true) => StdFont::TimesItalic,
                (false, false) => StdFont::Times,
            };
        }
        match (bold, italic) {
            (true, true) => StdFont::HelveticaBoldOblique,
            (true, false) => StdFont::HelveticaBold,
            (false, true) => StdFont::HelveticaOblique,
            (false, false) => StdFont::Helvetica,
        }
    }
}

/// Map a char to its WinAnsi (CP1252) byte, or `None` if unrepresentable.
fn winansi_byte(c: char) -> Option<u8> {
    let u = c as u32;
    match u {
        0x20..=0x7E | 0xA0..=0xFF => Some(u as u8),
        0x20AC => Some(0x80),
        0x201A => Some(0x82),
        0x0192 => Some(0x83),
        0x201E => Some(0x84),
        0x2026 => Some(0x85),
        0x2020 => Some(0x86),
        0x2021 => Some(0x87),
        0x02C6 => Some(0x88),
        0x2030 => Some(0x89),
        0x0160 => Some(0x8A),
        0x2039 => Some(0x8B),
        0x0152 => Some(0x8C),
        0x017D => Some(0x8E),
        0x2018 => Some(0x91),
        0x2019 => Some(0x92),
        0x201C => Some(0x93),
        0x201D => Some(0x94),
        0x2022 => Some(0x95),
        0x2013 => Some(0x96),
        0x2014 => Some(0x97),
        0x02DC => Some(0x98),
        0x2122 => Some(0x99),
        0x0161 => Some(0x9A),
        0x203A => Some(0x9B),
        0x0153 => Some(0x9C),
        0x017E => Some(0x9E),
        0x0178 => Some(0x9F),
        _ => None,
    }
}

/// Encode `text` as the bytes of a PDF literal string body (WinAnsi, `?`
/// fallback, with `(`, `)` and `\` escaped).
fn literal_string(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 2);
    for ch in text.chars() {
        let b = winansi_byte(ch).unwrap_or(b'?');
        if matches!(b, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

#[derive(Debug)]
struct BuilderPage {
    width: f64,
    height: f64,
    content: Vec<u8>,
}

/// Accumulates pages and their content, then serializes a complete PDF.
#[derive(Debug, Default)]
pub struct PdfBuilder {
    pages: Vec<BuilderPage>,
}

impl PdfBuilder {
    pub fn new() -> PdfBuilder {
        PdfBuilder::default()
    }

    /// Append a page of `width`×`height` points; returns its index.
    pub fn add_page(&mut self, width: f64, height: f64) -> usize {
        self.pages.push(BuilderPage {
            width: width.max(1.0),
            height: height.max(1.0),
            content: Vec::new(),
        });
        self.pages.len() - 1
    }

    fn num(v: f64) -> String {
        crate::content::num(v)
    }

    /// Draw a text run. `x`/`y` are the run's **top-left** in top-down points;
    /// `size` is in points; `color` is RGB `0..=1`.
    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &mut self,
        page: usize,
        x: f64,
        y: f64,
        size: f64,
        text: &str,
        font: StdFont,
        color: [f64; 3],
    ) {
        let Some(p) = self.pages.get_mut(page) else { return };
        // PDF baseline ≈ top + size*0.8, flipped to bottom-up space.
        let baseline = p.height - (y + size * 0.8);
        let mut s = format!(
            "\nq\n{} {} {} rg\nBT\n/{} {} Tf\n{} {} Td\n(",
            Self::num(color[0]),
            Self::num(color[1]),
            Self::num(color[2]),
            font.resource_name(),
            Self::num(size),
            Self::num(x),
            Self::num(baseline),
        )
        .into_bytes();
        s.extend_from_slice(&literal_string(text));
        s.extend_from_slice(b") Tj\nET\nQ\n");
        p.content.extend_from_slice(&s);
    }

    /// Draw a rectangle outline/fill. `x`/`y` top-left, top-down points.
    #[allow(clippy::too_many_arguments)]
    pub fn rect(
        &mut self,
        page: usize,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        stroke: Option<[f64; 3]>,
        fill: Option<[f64; 3]>,
    ) {
        let Some(p) = self.pages.get_mut(page) else { return };
        let y_pdf = p.height - y - h; // bottom edge
        let mut s = String::from("\nq\n");
        if let Some([r, g, b]) = fill {
            s.push_str(&format!("{} {} {} rg\n", Self::num(r), Self::num(g), Self::num(b)));
        }
        if let Some([r, g, b]) = stroke {
            s.push_str(&format!("{} {} {} RG\n", Self::num(r), Self::num(g), Self::num(b)));
        }
        s.push_str(&format!(
            "{} {} {} {} re\n{}\nQ\n",
            Self::num(x),
            Self::num(y_pdf),
            Self::num(w.max(0.1)),
            Self::num(h.max(0.1)),
            match (fill.is_some(), stroke.is_some()) {
                (true, true) => "B",
                (true, false) => "f",
                _ => "S",
            }
        ));
        p.content.extend_from_slice(s.as_bytes());
    }

    /// Serialize the complete PDF.
    pub fn finish(self) -> Vec<u8> {
        let mut objects: BTreeMap<ObjectId, Object> = BTreeMap::new();
        // Fixed ids: 1=Catalog, 2=Pages, 3=Resources, 4..12 = the 9 std fonts.
        let catalog_id = (1u32, 0u16);
        let pages_id = (2u32, 0u16);
        let resources_id = (3u32, 0u16);

        // Standard fonts + the shared Resources dict.
        let mut font_dict = Dictionary::new();
        for (i, font) in StdFont::ALL.iter().enumerate() {
            let id = (4 + i as u32, 0u16);
            let mut f = Dictionary::new();
            f.set(b"Type", name(b"Font"));
            f.set(b"Subtype", name(b"Type1"));
            f.set(b"BaseFont", name(font.base_font().as_bytes()));
            f.set(b"Encoding", name(b"WinAnsiEncoding"));
            objects.insert(id, Object::Dictionary(f));
            font_dict.set(font.resource_name().as_bytes().to_vec(), Object::Reference(id));
        }
        let mut resources = Dictionary::new();
        resources.set(b"Font", Object::Dictionary(font_dict));
        objects.insert(resources_id, Object::Dictionary(resources));

        // Pages: content stream + page dict per page. Ids start after the fonts.
        let mut next = 4 + StdFont::ALL.len() as u32;
        let mut kids = Vec::new();
        for page in &self.pages {
            let content_id = (next, 0u16);
            let page_id = (next + 1, 0u16);
            next += 2;

            let mut cdict = Dictionary::new();
            cdict.set(b"Length", Object::Integer(page.content.len() as i64));
            objects.insert(content_id, Object::Stream(Stream::new(cdict, page.content.clone())));

            let mut pdict = Dictionary::new();
            pdict.set(b"Type", name(b"Page"));
            pdict.set(b"Parent", Object::Reference(pages_id));
            pdict.set(
                b"MediaBox",
                Object::Array(vec![
                    Object::Real(0.0),
                    Object::Real(0.0),
                    Object::Real(page.width),
                    Object::Real(page.height),
                ]),
            );
            pdict.set(b"Resources", Object::Reference(resources_id));
            pdict.set(b"Contents", Object::Reference(content_id));
            objects.insert(page_id, Object::Dictionary(pdict));
            kids.push(Object::Reference(page_id));
        }

        let count = kids.len() as i64;
        let mut pages = Dictionary::new();
        pages.set(b"Type", name(b"Pages"));
        pages.set(b"Kids", Object::Array(kids));
        pages.set(b"Count", Object::Integer(count.max(0)));
        objects.insert(pages_id, Object::Dictionary(pages));

        let mut catalog = Dictionary::new();
        catalog.set(b"Type", name(b"Catalog"));
        catalog.set(b"Pages", Object::Reference(pages_id));
        objects.insert(catalog_id, Object::Dictionary(catalog));

        let mut trailer = Dictionary::new();
        trailer.set(b"Root", Object::Reference(catalog_id));
        // A file id (deterministic — the host can re-stamp).
        let id = Object::String(b"gigapdf-engine".to_vec(), StringKind::Hex);
        trailer.set(b"ID", Object::Array(vec![id.clone(), id]));

        crate::serialize::to_pdf(&objects, &trailer)
    }
}

fn name(bytes: &[u8]) -> Object {
    Object::Name(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_valid_pdf_with_text() {
        let mut b = PdfBuilder::new();
        let p = b.add_page(612.0, 792.0);
        b.text(p, 72.0, 100.0, 14.0, "Hello (PDF) — café", StdFont::Helvetica, [0.0, 0.0, 0.0]);
        b.rect(p, 50.0, 50.0, 200.0, 30.0, Some([0.5, 0.5, 0.5]), None);
        let pdf = b.finish();
        assert_eq!(&pdf[0..5], b"%PDF-");
        assert!(pdf.windows(5).any(|w| w == b"%%EOF"), "has EOF marker");

        // Re-open with the engine and confirm structure + text extraction.
        let doc = crate::Document::open(&pdf).expect("re-open built PDF");
        assert_eq!(doc.page_count(), 1);
        let runs = doc.page_text_runs(1).unwrap();
        assert!(runs.iter().any(|r| r.text.contains("Hello")), "text extractable");
    }

    #[test]
    fn winansi_maps_specials_and_falls_back() {
        assert_eq!(winansi_byte('A'), Some(b'A'));
        assert_eq!(winansi_byte('é'), Some(0xE9));
        assert_eq!(winansi_byte('—'), Some(0x97)); // em dash
        assert_eq!(winansi_byte('€'), Some(0x80));
        assert_eq!(winansi_byte('中'), None); // CJK → caller substitutes '?'
        assert_eq!(literal_string("a(b)\\c"), b"a\\(b\\)\\\\c");
    }
}
