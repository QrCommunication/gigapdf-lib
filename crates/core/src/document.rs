//! High-level PDF document: open a real file, walk the page tree, decode page
//! content. Built on our own lexer/parser/inflate — zero dependencies.
//!
//! Robust open strategy: rather than trusting the cross-reference table (which
//! is frequently broken in real files, and is a compressed stream in PDF 1.5+),
//! we scan the whole file for `n g obj … endobj` definitions and the `trailer`
//! dictionary. Later definitions override earlier ones, which naturally handles
//! incremental updates. The catalog is found via `trailer /Root`, falling back
//! to any `/Type /Catalog` object.

use std::collections::{BTreeMap, BTreeSet};

use crate::annot::{self, Annotation};
use crate::content::{self, ContentElement, TextRun};
use crate::error::{EngineError, Result};
use crate::form::FormField;
use crate::filters::decode_stream;
use crate::lexer::{Lexer, Token};
use crate::link::{Link, LinkTarget};
use crate::object::{Dictionary, Object, ObjectId, Stream, StringKind};
use crate::ocg::Layer;
use crate::outline::OutlineItem;
use crate::parser::Parser;

/// A parsed PDF document held in memory.
#[derive(Debug, Clone)]
pub struct Document {
    objects: BTreeMap<ObjectId, Object>,
    trailer: Dictionary,
}

/// A full-text search hit: the page, the matching line's text, and its bounding
/// box in PDF user space (origin bottom-left) for highlighting.
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub page: u32,
    pub text: String,
    pub bounds: content::Bounds,
}

/// Push every indirect reference contained in `object` onto `out`.
fn collect_refs(object: &Object, out: &mut Vec<ObjectId>) {
    match object {
        Object::Reference(id) => out.push(*id),
        Object::Array(items) => items.iter().for_each(|o| collect_refs(o, out)),
        Object::Dictionary(dict) => dict.0.values().for_each(|v| collect_refs(v, out)),
        Object::Stream(stream) => stream.dict.0.values().for_each(|v| collect_refs(v, out)),
        _ => {}
    }
}

/// Rewrite an object's indirect references through `map` (for grafting between
/// documents). References absent from the map are kept as-is.
fn remap_object(object: &Object, map: &BTreeMap<ObjectId, ObjectId>) -> Object {
    match object {
        Object::Reference(id) => Object::Reference(map.get(id).copied().unwrap_or(*id)),
        Object::Array(items) => Object::Array(items.iter().map(|o| remap_object(o, map)).collect()),
        Object::Dictionary(dict) => Object::Dictionary(remap_dict(dict, map)),
        Object::Stream(stream) => Object::Stream(Stream {
            dict: remap_dict(&stream.dict, map),
            raw: stream.raw.clone(),
        }),
        other => other.clone(),
    }
}

fn remap_dict(dict: &Dictionary, map: &BTreeMap<ObjectId, ObjectId>) -> Dictionary {
    let mut out = Dictionary::new();
    for (key, value) in &dict.0 {
        out.0.insert(key.clone(), remap_object(value, map));
    }
    out
}

/// Write a `(...)` literal string, escaping the bytes that need it.
fn push_pdf_string(out: &mut Vec<u8>, text: &str) {
    out.push(b'(');
    for &byte in &crate::font::encode_winansi(text) {
        if matches!(byte, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.push(b')');
}

/// Build a field's appearance form (dictionary without `/Length`) and its
/// content stream, sizing the text to the widget rectangle. A `value`
/// containing newlines is laid out as multiple top-aligned lines (multiline
/// text and list boxes); a single line is vertically centred.
fn build_text_field_appearance(rect: [f64; 4], value: &str) -> (Dictionary, Vec<u8>) {
    let w = rect[2] - rect[0];
    let h = rect[3] - rect[1];
    let lines: Vec<&str> = value.split('\n').collect();
    let multiline = lines.len() > 1;

    let size = if multiline {
        (h / (lines.len() as f64 + 0.5)).clamp(6.0, 12.0)
    } else {
        (h * 0.6).clamp(6.0, 14.0)
    };
    let leading = size * 1.15;
    let first_baseline = if multiline {
        h - size
    } else {
        (h - size) / 2.0 + size * 0.2
    };

    let mut content = Vec::new();
    content.extend_from_slice(b"/Tx BMC\nq\nBT\n");
    content.extend_from_slice(format!("/Helv {} Tf 0 g\n", content::num(size)).as_bytes());
    content.extend_from_slice(format!("{} TL\n", content::num(leading)).as_bytes());
    content.extend_from_slice(format!("2 {} Td\n", content::num(first_baseline)).as_bytes());
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            content.extend_from_slice(b"T*\n");
        }
        push_pdf_string(&mut content, line);
        content.extend_from_slice(b" Tj\n");
    }
    content.extend_from_slice(b"ET\nQ\nEMC\n");

    let mut helv = Dictionary::new();
    helv.set(b"Type".to_vec(), annot::name(b"Font"));
    helv.set(b"Subtype".to_vec(), annot::name(b"Type1"));
    helv.set(b"BaseFont".to_vec(), annot::name(b"Helvetica"));
    let mut fonts = Dictionary::new();
    fonts.set(b"Helv".to_vec(), Object::Dictionary(helv));
    let mut resources = Dictionary::new();
    resources.set(b"Font".to_vec(), Object::Dictionary(fonts));

    let mut form = Dictionary::new();
    form.set(b"Type".to_vec(), annot::name(b"XObject"));
    form.set(b"Subtype".to_vec(), annot::name(b"Form"));
    form.set(b"BBox".to_vec(), annot::real_array(&[0.0, 0.0, w, h]));
    form.set(b"Resources".to_vec(), Object::Dictionary(resources));
    (form, content)
}

impl Document {
    /// Parse a PDF from raw bytes.
    pub fn open(bytes: &[u8]) -> Result<Self> {
        Self::open_with_password(bytes, b"")
    }

    /// Open a (possibly encrypted) PDF, decrypting with `password`.
    pub fn open_with_password(bytes: &[u8], password: &[u8]) -> Result<Self> {
        let (mut objects, mut trailer) = scan(bytes);
        if objects.is_empty() {
            return Err(EngineError::parse(0, "no PDF objects found"));
        }
        // PDF 1.5+: `/Root` lives in the xref-stream dict (no classic trailer),
        // and the catalog/pages are packed inside compressed object streams.
        recover_trailer_from_xref(&mut trailer, &objects);
        // Decrypt top-level objects BEFORE extracting object streams, so the
        // (now-plaintext) ObjStm contents are read directly.
        decrypt_objects(&mut objects, &trailer, password)?;
        extract_object_streams(&mut objects);
        Ok(Self { objects, trailer })
    }

    /// Digitally sign the document with an engine-managed signer, producing a
    /// signed PDF (`adbe.pkcs7.detached`). The signer carries a self-signed
    /// certificate (an ephemeral "digital ID", like Adobe's self-signed IDs):
    /// this proves integrity + authorship, not a CA-backed identity (non-eIDAS).
    /// `date` is a PDF date string such as `"D:20260614120000Z"`.
    pub fn sign(
        &mut self,
        signer: &crate::sign::Signer,
        name: &str,
        reason: &str,
        date: &str,
    ) -> Result<Vec<u8>> {
        const CONTENTS_BYTES: usize = 8192; // room for the CMS (hex = 16384 chars)
        let lit = |s: &str| Object::String(crate::font::encode_pdf_text(s), StringKind::Literal);

        // 1. Signature value dictionary with fixed-width placeholders.
        let sig_id = (self.next_object_number(), 0u16);
        let mut sig = Dictionary::new();
        sig.set(b"Type".to_vec(), Object::Name(b"Sig".to_vec()));
        sig.set(b"Filter".to_vec(), Object::Name(b"Adobe.PPKLite".to_vec()));
        sig.set(b"SubFilter".to_vec(), Object::Name(b"adbe.pkcs7.detached".to_vec()));
        sig.set(b"Name".to_vec(), lit(name));
        sig.set(b"Reason".to_vec(), lit(reason));
        sig.set(b"M".to_vec(), lit(date));
        // 4 × 10-digit numbers → a fixed-width array we can patch in place.
        sig.set(
            b"ByteRange".to_vec(),
            Object::Array(vec![Object::Integer(9_999_999_999); 4]),
        );
        sig.set(
            b"Contents".to_vec(),
            Object::String(vec![0u8; CONTENTS_BYTES], StringKind::Hex),
        );
        self.objects.insert(sig_id, Object::Dictionary(sig));

        // 2. Signature field = invisible widget on page 1, linked to the value.
        let field_id = (self.next_object_number(), 0u16);
        let mut field = Dictionary::new();
        field.set(b"Type".to_vec(), Object::Name(b"Annot".to_vec()));
        field.set(b"Subtype".to_vec(), Object::Name(b"Widget".to_vec()));
        field.set(b"FT".to_vec(), Object::Name(b"Sig".to_vec()));
        field.set(b"T".to_vec(), lit("Signature1"));
        field.set(b"V".to_vec(), Object::Reference(sig_id));
        field.set(b"Rect".to_vec(), annot::real_array(&[0.0, 0.0, 0.0, 0.0]));
        field.set(b"F".to_vec(), Object::Integer(132)); // Print + Locked
        if let Ok(page_id) = self.page_object_id(1) {
            field.set(b"P".to_vec(), Object::Reference(page_id));
        }
        self.objects.insert(field_id, Object::Dictionary(field));

        if let Ok(page_id) = self.page_object_id(1) {
            if let Some(mut page) = self.objects.get(&page_id).and_then(Object::as_dict).cloned() {
                let mut annots = page
                    .get(b"Annots")
                    .map(|o| self.resolve(o))
                    .and_then(Object::as_array)
                    .map(<[Object]>::to_vec)
                    .unwrap_or_default();
                annots.push(Object::Reference(field_id));
                page.set(b"Annots".to_vec(), Object::Array(annots));
                self.objects.insert(page_id, Object::Dictionary(page));
            }
        }

        // 3. Register the field in the AcroForm and flag the document as signed.
        let catalog_id = self.catalog_id()?;
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        let mut acroform = catalog
            .get(b"AcroForm")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        let mut fields = acroform
            .get(b"Fields")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        fields.push(Object::Reference(field_id));
        acroform.set(b"Fields".to_vec(), Object::Array(fields));
        acroform.set(b"SigFlags".to_vec(), Object::Integer(3));
        catalog.set(b"AcroForm".to_vec(), Object::Dictionary(acroform));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));

        // 4. Serialize, then patch /ByteRange and fill /Contents with the CMS.
        let mut bytes = self.save();
        // The signature's /Contents is the only one written as a hex string
        // (`/Contents <…>`); a page's /Contents is an indirect reference.
        let lt = find_subsequence(&bytes, b"/Contents <")
            .map(|p| p + b"/Contents <".len() - 1) // index of the '<'
            .ok_or_else(|| EngineError::Missing("signature /Contents placeholder".into()))?;
        let gt = bytes[lt..]
            .iter()
            .position(|&b| b == b'>')
            .map(|p| lt + p)
            .ok_or_else(|| EngineError::Missing("signature /Contents end".into()))?;

        let total = bytes.len();
        let byte_range = [0usize, lt, gt + 1, total - (gt + 1)];

        let br = find_subsequence(&bytes, b"/ByteRange [")
            .map(|p| p + b"/ByteRange [".len())
            .ok_or_else(|| EngineError::Missing("signature /ByteRange".into()))?;
        let mut p = br;
        for (i, value) in byte_range.iter().enumerate() {
            bytes[p..p + 10].copy_from_slice(format!("{value:010}").as_bytes());
            p += 10 + usize::from(i < 3); // 10 digits, then a separator space
        }

        // Hash everything except the /Contents hex, build the CMS, hex-fill it.
        let mut signed = Vec::with_capacity(byte_range[1] + byte_range[3]);
        signed.extend_from_slice(&bytes[0..lt]);
        signed.extend_from_slice(&bytes[gt + 1..]);
        let cms = signer.detached_cms(&signed);

        let capacity = gt - (lt + 1); // hex digit slots between < and >
        let mut hex = String::with_capacity(capacity);
        for byte in &cms {
            hex.push_str(&format!("{byte:02X}"));
        }
        if hex.len() > capacity {
            return Err(EngineError::Unsupported(
                "signature too large for the reserved /Contents space".into(),
            ));
        }
        while hex.len() < capacity {
            hex.push('0');
        }
        bytes[lt + 1..gt].copy_from_slice(hex.as_bytes());
        Ok(bytes)
    }

    /// Serialize the document encrypted with the Standard Security Handler
    /// (RC4 128-bit). `id0` is the file identifier (host-provided randomness);
    /// `permissions` is the `/P` flags value.
    pub fn save_encrypted(&self, user_password: &[u8], id0: &[u8], permissions: i32) -> Vec<u8> {
        let (security, encrypt_dict) =
            crate::security::Security::new_rc4(user_password, id0, permissions);
        crate::serialize::to_pdf_encrypted(
            &self.objects,
            &self.trailer,
            &security,
            &encrypt_dict,
            id0,
        )
    }

    /// Number of objects parsed (diagnostic).
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Serialize the (possibly edited) document to a fresh, valid PDF.
    pub fn save(&self) -> Vec<u8> {
        crate::serialize::to_pdf(&self.objects, &self.trailer)
    }

    /// Fetch an indirect object by id.
    pub fn get(&self, id: ObjectId) -> Option<&Object> {
        self.objects.get(&id)
    }

    /// Follow indirect references until a direct object is reached.
    pub fn resolve<'a>(&'a self, object: &'a Object) -> &'a Object {
        let mut current = object;
        for _ in 0..64 {
            match current {
                Object::Reference(id) => match self.objects.get(id) {
                    Some(next) => current = next,
                    None => return &Object::Null,
                },
                other => return other,
            }
        }
        &Object::Null
    }

    /// The document catalog dictionary.
    fn catalog(&self) -> Result<&Dictionary> {
        if let Some(root) = self.trailer.get(b"Root") {
            if let Some(dict) = self.resolve(root).as_dict() {
                return Ok(dict);
            }
        }
        // Fallback: any /Type /Catalog object.
        for object in self.objects.values() {
            if let Some(dict) = object.as_dict() {
                if dict.get(b"Type").and_then(Object::as_name) == Some(b"Catalog".as_slice()) {
                    return Ok(dict);
                }
            }
        }
        Err(EngineError::Missing("document catalog".into()))
    }

    /// Number of pages in the document (0 if the page tree can't be read).
    pub fn page_count(&self) -> usize {
        self.page_ids().map(|ids| ids.len()).unwrap_or(0)
    }

    /// Object ids of all pages, in reading order.
    pub fn page_ids(&self) -> Result<Vec<ObjectId>> {
        let root = self
            .catalog()?
            .get(b"Pages")
            .ok_or_else(|| EngineError::Missing("catalog /Pages".into()))?
            .clone();
        let mut ids = Vec::new();
        self.collect_pages(&root, &mut ids, 0)?;
        Ok(ids)
    }

    fn collect_pages(&self, node: &Object, out: &mut Vec<ObjectId>, depth: usize) -> Result<()> {
        if depth > 50 {
            return Err(EngineError::Unsupported("page tree too deep".into()));
        }
        let node_id = node.as_reference();
        let dict = match self.resolve(node).as_dict() {
            Some(dict) => dict,
            None => return Ok(()),
        };

        let is_pages_node = dict.get(b"Type").and_then(Object::as_name) == Some(b"Pages".as_slice())
            || dict.contains(b"Kids");

        if is_pages_node {
            if let Some(kids) = dict.get(b"Kids") {
                if let Some(items) = self.resolve(kids).as_array() {
                    for kid in items {
                        self.collect_pages(kid, out, depth + 1)?;
                    }
                }
            }
        } else if let Some(id) = node_id {
            out.push(id); // a leaf page
        }
        Ok(())
    }

    /// The page dictionary for a 1-based page number.
    pub fn page_dict(&self, page_no: u32) -> Result<&Dictionary> {
        let ids = self.page_ids()?;
        let id = ids
            .get(page_no.saturating_sub(1) as usize)
            .ok_or(EngineError::PageNotFound(page_no))?;
        self.objects
            .get(id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))
    }

    /// The decoded (filters applied) content stream of a 1-based page.
    ///
    /// Multiple content streams are concatenated with a newline, as a PDF
    /// consumer would.
    pub fn page_content(&self, page_no: u32) -> Result<Vec<u8>> {
        let page = self.page_dict(page_no)?;
        let contents = page
            .get(b"Contents")
            .ok_or_else(|| EngineError::Missing("page /Contents".into()))?
            .clone();
        let mut out = Vec::new();
        self.append_content(&contents, &mut out)?;
        Ok(out)
    }

    // ─── content editing (Word-like) ─────────────────────────────────────────

    /// 1-based page number → its object id.
    fn page_object_id(&self, page_no: u32) -> Result<ObjectId> {
        let ids = self.page_ids()?;
        ids.get(page_no.saturating_sub(1) as usize)
            .copied()
            .ok_or(EngineError::PageNotFound(page_no))
    }

    /// Next free object number (one past the current maximum).
    fn next_object_number(&self) -> u32 {
        self.objects.keys().map(|(n, _)| *n).max().unwrap_or(0) + 1
    }

    /// The text runs on a page (1-based), in reading order. Text is decoded
    /// font-aware (WinAnsi + `/ToUnicode` for CID/Type0 and custom encodings)
    /// so extraction has no tofu.
    pub fn page_text_runs(&self, page_no: u32) -> Result<Vec<TextRun>> {
        let content = self.page_content(page_no)?;
        let fonts = self.page_font_decoders(page_no);
        content::extract_text_runs_with(&content, &fonts)
    }

    /// Build per-font text decoders from a page's `/Resources /Font`, reading
    /// each font's `/Subtype` (Type0 ⇒ 2-byte codes) and `/ToUnicode` CMap.
    fn page_font_decoders(&self, page_no: u32) -> content::FontDecoders {
        let mut decoders = content::FontDecoders::new();
        let Ok(page) = self.page_dict(page_no) else {
            return decoders;
        };
        let font_dict = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|res| res.get(b"Font"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict);
        let Some(font_dict) = font_dict else {
            return decoders;
        };
        for (name, value) in &font_dict.0 {
            let Some(font) = self.resolve(value).as_dict() else {
                continue;
            };
            let two_byte =
                font.get(b"Subtype").and_then(Object::as_name) == Some(b"Type0".as_slice());
            let to_unicode = font
                .get(b"ToUnicode")
                .map(|o| self.resolve(o))
                .and_then(Object::as_stream)
                .and_then(|stream| decode_stream(stream).ok())
                .map(|bytes| crate::font::cmap::ToUnicode::parse(&bytes))
                .filter(|cmap| !cmap.is_empty());
            decoders.insert(
                name.clone(),
                crate::font::cmap::TextDecoder {
                    two_byte,
                    to_unicode,
                },
            );
        }
        decoders
    }

    /// Map each font resource name on a page to a recovered [`TextStyle`]
    /// (family/weight/style) parsed from its `/BaseFont`. Used by the Office
    /// exporters to carry real fonts, not just sizes.
    fn page_base_fonts(&self, page_no: u32) -> BTreeMap<String, crate::convert::TextStyle> {
        let mut out = BTreeMap::new();
        let Ok(page) = self.page_dict(page_no) else {
            return out;
        };
        let font_dict = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|res| res.get(b"Font"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict);
        let Some(font_dict) = font_dict else {
            return out;
        };
        for (name, value) in &font_dict.0 {
            let Some(font) = self.resolve(value).as_dict() else {
                continue;
            };
            if let Some(base) = font.get(b"BaseFont").and_then(Object::as_name) {
                let style = crate::convert::style::parse_base_font(&String::from_utf8_lossy(base));
                out.insert(String::from_utf8_lossy(name).into_owned(), style);
            }
        }
        out
    }

    /// Replace a text run's text in place (keeps position and font).
    pub fn replace_text_run(&mut self, page_no: u32, index: usize, new_text: &str) -> Result<()> {
        let content = self.page_content(page_no)?;
        let edited = content::replace_text_run(&content, index, new_text)?;
        self.set_page_content(page_no, edited)
    }

    /// Remove a text run, preserving the rest of the page (background intact).
    pub fn remove_text_run(&mut self, page_no: u32, index: usize) -> Result<()> {
        let content = self.page_content(page_no)?;
        let edited = content::remove_text_run(&content, index)?;
        self.set_page_content(page_no, edited)
    }

    /// All addressable elements (text, images, shapes) of a page, in order.
    pub fn page_elements(&self, page_no: u32) -> Result<Vec<ContentElement>> {
        let content = self.page_content(page_no)?;
        let fonts = self.page_font_decoders(page_no);
        content::extract_elements_with(&content, &fonts)
    }

    /// Redact a rectangular region (page user space): permanently **remove**
    /// every content element overlapping it from the content stream. Returns how
    /// many elements were removed.
    ///
    /// This is true redaction by deletion — the text/graphics are gone from the
    /// stream (uncopyable, unrecoverable) and **whatever was behind them (a
    /// gradient, image or pattern background) is preserved untouched**. Pass a
    /// `cover` colour only when you want to visibly mark the censored area
    /// (legal redaction); `None` leaves the background showing through.
    pub fn redact_region(
        &mut self,
        page_no: u32,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        cover: Option<[f64; 3]>,
    ) -> Result<usize> {
        let region = content::Bounds { x, y, width, height };
        let mut hits: Vec<usize> = self
            .page_elements(page_no)?
            .into_iter()
            .filter(|e| e.bounds.is_some_and(|b| b.intersects(&region)))
            .map(|e| e.index)
            .collect();
        // Remove highest index first so the remaining target indices stay valid.
        hits.sort_unstable_by(|a, b| b.cmp(a));
        for index in &hits {
            self.remove_element(page_no, *index)?;
        }
        // Optional visible cover; by default the background shows through.
        if let Some(color) = cover {
            self.add_rectangle(page_no, x, y, width, height, None, Some(color), 0.0)?;
        }
        Ok(hits.len())
    }

    /// The page's `/MediaBox` `[x0 y0 x1 y1]`, defaulting to US Letter.
    fn read_media_box(&self, page: &Dictionary) -> [f64; 4] {
        if let Some(values) = page
            .get(b"MediaBox")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            let nums: Vec<f64> = values.iter().filter_map(Object::as_f64).collect();
            if nums.len() == 4 {
                return [nums[0], nums[1], nums[2], nums[3]];
            }
        }
        [0.0, 0.0, 612.0, 792.0]
    }

    /// Rasterize a page to a PNG at `scale` device pixels per PDF point, using
    /// the built-in zero-dependency renderer (vector graphics; text glyphs and
    /// images are added by later renderer slices).
    pub fn render_page(&self, page_no: u32, scale: f64) -> Result<Vec<u8>> {
        Ok(self.render_page_canvas(page_no, scale)?.to_png())
    }

    /// Rasterize a page to an RGBA [`Canvas`](crate::raster::Canvas) at `scale`
    /// device pixels per point. Shared by `render_page` and `ocr_page`.
    fn render_page_canvas(&self, page_no: u32, scale: f64) -> Result<crate::raster::Canvas> {
        let media_box = self.read_media_box(self.page_dict(page_no)?);
        let [x0, y0, x1, y1] = media_box;
        let w_pts = (x1 - x0).abs();
        let h_pts = (y1 - y0).abs();
        let scale = scale.max(0.01);
        let width = ((w_pts * scale).ceil() as u32).max(1);
        let height = ((h_pts * scale).ceil() as u32).max(1);
        let base = content::PageMatrix::new(scale, 0.0, 0.0, -scale, -x0 * scale, (y0 + h_pts) * scale);
        let content = self.page_content(page_no)?;
        let fonts = self.page_render_fonts(page_no);
        let images = self.page_images(page_no);
        Ok(crate::raster::render_content(&content, width, height, base, &fonts, &images))
    }

    /// OCR a page with the built-in zero-dependency recognizer. The page is
    /// rasterized at `scale` (≥ 2.0 recommended for small text), binarized, and
    /// recognized; returns the text plus word boxes in **PDF user space** so the
    /// host can highlight or overlay. Works on scanned (image-only) pages — for
    /// pages that already carry a text layer, prefer [`structured_text`](Self::structured_text).
    pub fn ocr_page(&self, page_no: u32, scale: f64) -> Vec<crate::raster::ocr::OcrWord> {
        let Ok(canvas) = self.render_page_canvas(page_no, scale) else {
            return Vec::new();
        };
        let (w, h) = (canvas.width as usize, canvas.height as usize);
        let gray: Vec<u8> = canvas
            .pixels
            .chunks_exact(4)
            .map(|p| ((p[0] as u32 + p[1] as u32 + p[2] as u32) / 3) as u8)
            .collect();
        let result = crate::raster::ocr::ocr(&gray, w, h);

        // Map image pixels (top-left origin) back to PDF user space (bottom-left).
        let media = self
            .page_dict(page_no)
            .map(|p| self.read_media_box(p))
            .unwrap_or([0.0, 0.0, 612.0, 792.0]);
        let (x0, y0) = (media[0], media[1]);
        let page_h = (media[3] - media[1]).abs();
        let s = scale.max(0.01);
        result
            .words
            .into_iter()
            .map(|word| crate::raster::ocr::OcrWord {
                text: word.text,
                x: x0 + word.x / s,
                y: y0 + page_h - (word.y + word.height) / s,
                width: word.width / s,
                height: word.height / s,
            })
            .collect()
    }

    /// OCR a page and return only the recognized text (newline-separated lines).
    pub fn ocr_page_text(&self, page_no: u32, scale: f64) -> String {
        let Ok(canvas) = self.render_page_canvas(page_no, scale) else {
            return String::new();
        };
        let (w, h) = (canvas.width as usize, canvas.height as usize);
        let gray: Vec<u8> = canvas
            .pixels
            .chunks_exact(4)
            .map(|p| ((p[0] as u32 + p[1] as u32 + p[2] as u32) / 3) as u8)
            .collect();
        crate::raster::ocr::ocr(&gray, w, h).text
    }

    /// Extract every page's editable content (positioned text, re-embedded
    /// images, shape rectangles) into the conversion model, normalizing PDF
    /// bottom-up user space to top-down points. This is the shared front-end for
    /// all the Office exporters — they reconstruct real objects from this, never
    /// a page raster.
    fn convert_pages(&self) -> Vec<crate::convert::ConvPage> {
        use crate::content::ElementKind;
        use crate::convert::{ConvPage, PlacedImage, PlacedShape, PlacedText};

        let mut pages = Vec::new();
        for page_no in 1..=self.page_count() as u32 {
            let Ok(page) = self.page_dict(page_no) else {
                continue;
            };
            let media = self.read_media_box(page);
            let (x0, y0) = (media[0], media[1]);
            let page_w = (media[2] - media[0]).abs();
            let page_h = (media[3] - media[1]).abs();

            let elements = self.page_elements(page_no).unwrap_or_default();
            let images = self.page_images(page_no);
            let font_styles = self.page_base_fonts(page_no);
            // Encode each referenced image once per page (a single XObject may be
            // drawn several times) so repeated placements share the PNG bytes.
            let mut png_cache: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

            let mut conv = ConvPage {
                width: page_w,
                height: page_h,
                ..ConvPage::default()
            };
            for element in elements {
                let Some(b) = element.bounds else { continue };
                let left = b.x - x0;
                let top = page_h - (b.y - y0) - b.height;
                match element.kind {
                    ElementKind::Text => {
                        if element.label.trim().is_empty() {
                            continue;
                        }
                        // Recover the run's style from its font resource, then
                        // overlay the fill colour the interpreter captured.
                        let mut style = element
                            .font
                            .as_deref()
                            .and_then(|f| font_styles.get(f))
                            .cloned()
                            .unwrap_or_default();
                        style.color = element.color;
                        conv.texts.push(PlacedText {
                            text: element.label,
                            x: left,
                            y: top,
                            width: b.width,
                            height: b.height,
                            style,
                        });
                    }
                    ElementKind::Image => {
                        let key = element.label.into_bytes();
                        if let Some(image) = images.get(&key) {
                            let png = png_cache
                                .entry(key)
                                .or_insert_with(|| {
                                    crate::raster::png::encode_png(
                                        image.width,
                                        image.height,
                                        &image.rgba,
                                    )
                                })
                                .clone();
                            conv.images.push(PlacedImage {
                                png,
                                x: left,
                                y: top,
                                width: b.width,
                                height: b.height,
                            });
                        }
                    }
                    ElementKind::Path => {
                        conv.shapes.push(PlacedShape {
                            x: left,
                            y: top,
                            width: b.width,
                            height: b.height,
                        });
                    }
                }
            }
            pages.push(conv);
        }
        pages
    }

    /// Convert the document to an editable OpenDocument Text (`.odt`): every text
    /// run becomes a positioned text box, every image a placed picture — real,
    /// editable content rather than a page image.
    pub fn to_odt(&self) -> Vec<u8> {
        crate::convert::office::to_odt(&self.convert_pages())
    }

    /// Convert the document to an editable Word document (`.docx`): positioned
    /// text boxes + anchored pictures + shape rectangles, one section per page.
    pub fn to_docx(&self) -> Vec<u8> {
        crate::convert::office::to_docx(&self.convert_pages())
    }

    /// Convert the document to an editable PowerPoint presentation (`.pptx`):
    /// one slide per page, each text run a positioned box, each image a picture.
    pub fn to_pptx(&self) -> Vec<u8> {
        crate::convert::office::to_pptx(&self.convert_pages())
    }

    /// Reconstruct each page's text into a row/column grid and export an Excel
    /// workbook (`.xlsx`), one sheet per page. Tabular PDFs become real cells;
    /// prose collapses to a single column so all document text is preserved.
    pub fn to_xlsx(&self) -> Vec<u8> {
        let grids = self.convert_grids();
        crate::convert::office::to_xlsx(&grids)
    }

    /// As [`to_xlsx`](Self::to_xlsx) but to an OpenDocument Spreadsheet (`.ods`).
    pub fn to_ods(&self) -> Vec<u8> {
        let grids = self.convert_grids();
        crate::convert::office::to_ods(&grids)
    }

    /// Convert the document's text to an RTF document (one paragraph per text
    /// line). Pairs with [`reverse::rtf_to_pdf`](crate::convert::reverse::rtf_to_pdf).
    pub fn to_rtf(&self) -> Vec<u8> {
        let text = self.to_text();
        let paragraphs: Vec<String> = text
            .split(['\n', '\u{000C}'])
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        crate::convert::reverse::to_rtf(&paragraphs)
    }

    /// Re-serialize the document with PDF/A-2b archival metadata: an XMP
    /// identification packet, an sRGB `OutputIntent` (embedded ICC profile) and a
    /// trailer `/ID`. **Note:** full PDF/A conformance also requires every font to
    /// be embedded; embed non-embedded fonts via [`needed_fonts`](Self::needed_fonts)
    /// and [`embed_truetype_font`](Self::embed_truetype_font) first for a
    /// validator-clean result.
    pub fn to_pdfa(&self) -> Vec<u8> {
        use crate::object::StringKind::{Hex, Literal};
        let Ok(catalog_id) = self.catalog_id() else {
            return self.save();
        };
        let mut objects = self.objects.clone();
        let mut trailer = self.trailer.clone();

        let meta_id = (self.next_object_number(), 0u16);
        let icc_id = (meta_id.0 + 1, 0u16);

        // XMP metadata stream (must stay uncompressed for PDF/A).
        let xmp = crate::convert::pdfa::xmp_metadata("GigaPDF Document", "GigaPDF Engine");
        let mut mdict = Dictionary::new();
        mdict.set(b"Type", annot::name(b"Metadata"));
        mdict.set(b"Subtype", annot::name(b"XML"));
        mdict.set(b"Length", Object::Integer(xmp.len() as i64));
        objects.insert(meta_id, Object::Stream(Stream::new(mdict, xmp)));

        // sRGB ICC profile stream.
        let icc = crate::convert::srgb_icc::SRGB_ICC;
        let mut idict = Dictionary::new();
        idict.set(b"N", Object::Integer(3));
        idict.set(b"Length", Object::Integer(icc.len() as i64));
        objects.insert(icc_id, Object::Stream(Stream::new(idict, icc.to_vec())));

        // OutputIntent referencing the profile.
        let mut oi = Dictionary::new();
        oi.set(b"Type", annot::name(b"OutputIntent"));
        oi.set(b"S", annot::name(b"GTS_PDFA1"));
        oi.set(
            b"OutputConditionIdentifier",
            Object::String(b"sRGB IEC61966-2.1".to_vec(), Literal),
        );
        oi.set(b"Info", Object::String(b"sRGB IEC61966-2.1".to_vec(), Literal));
        oi.set(b"DestOutputProfile", Object::Reference(icc_id));

        // Attach Metadata + OutputIntents to the catalog.
        let mut catalog = objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        catalog.set(b"Metadata", Object::Reference(meta_id));
        catalog.set(b"OutputIntents", Object::Array(vec![Object::Dictionary(oi)]));
        objects.insert(catalog_id, Object::Dictionary(catalog));

        // PDF/A requires a trailer /ID. Derive one deterministically.
        if !trailer.contains(b"ID") {
            let seed = format!("gigapdf:{}", objects.len());
            let digest = crate::crypto::md5::md5(seed.as_bytes()).to_vec();
            let id = Object::String(digest, Hex);
            trailer.set(b"ID", Object::Array(vec![id.clone(), id]));
        }

        crate::serialize::to_pdf(&objects, &trailer)
    }

    /// Per-page reconstructed table grids (shared by the spreadsheet exporters).
    fn convert_grids(&self) -> Vec<Vec<Vec<String>>> {
        self.convert_pages()
            .iter()
            .map(|page| crate::convert::table::reconstruct(&page.texts))
            .collect()
    }

    /// Decode the page's image XObjects (`DeviceRGB`/`DeviceGray`, 8 bpc, Flate
    /// or raw — JPEG/JPX are skipped) into RGBA buffers for the rasterizer.
    fn page_images(&self, page_no: u32) -> crate::raster::render::RenderImages {
        let mut out = crate::raster::render::RenderImages::new();
        let Ok(page) = self.page_dict(page_no) else {
            return out;
        };
        let xobjects = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|res| res.get(b"XObject"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict);
        let Some(xobjects) = xobjects else {
            return out;
        };
        for (name, value) in &xobjects.0 {
            let Some(stream) = self.resolve(value).as_stream() else {
                continue;
            };
            let dict = &stream.dict;
            if dict.get(b"Subtype").and_then(Object::as_name) != Some(b"Image".as_slice()) {
                continue;
            }
            let width = dict.get(b"Width").and_then(Object::as_i64).unwrap_or(0);
            let height = dict.get(b"Height").and_then(Object::as_i64).unwrap_or(0);
            let bpc = dict.get(b"BitsPerComponent").and_then(Object::as_i64).unwrap_or(8);
            if width <= 0 || height <= 0 || bpc != 8 {
                continue;
            }
            // Skip compressed-photo filters we don't decode yet.
            let filter = self.first_filter(dict);
            if matches!(filter.as_deref(), Some(b"DCTDecode") | Some(b"JPXDecode")) {
                continue;
            }
            let components = match dict.get(b"ColorSpace").map(|o| self.resolve(o)).and_then(Object::as_name) {
                Some(b"DeviceRGB") => 3,
                Some(b"DeviceGray") => 1,
                _ => continue, // Indexed/ICCBased/CMYK not handled yet
            };
            let Ok(samples) = decode_stream(stream) else {
                continue;
            };
            let (w, h) = (width as usize, height as usize);
            if samples.len() < w * h * components {
                continue;
            }
            let mut rgba = Vec::with_capacity(w * h * 4);
            for px in samples.chunks_exact(components).take(w * h) {
                let (r, g, b) = if components == 1 {
                    (px[0], px[0], px[0])
                } else {
                    (px[0], px[1], px[2])
                };
                rgba.extend_from_slice(&[r, g, b, 255]);
            }
            out.insert(
                name.clone(),
                crate::raster::render::RenderImage {
                    width: width as u32,
                    height: height as u32,
                    rgba,
                },
            );
        }
        out
    }

    /// The first filter name of a stream dict (`/Filter` may be a name or array).
    fn first_filter(&self, dict: &Dictionary) -> Option<Vec<u8>> {
        match dict.get(b"Filter").map(|o| self.resolve(o)) {
            Some(Object::Name(n)) => Some(n.clone()),
            Some(Object::Array(items)) => items
                .first()
                .map(|o| self.resolve(o))
                .and_then(Object::as_name)
                .map(<[u8]>::to_vec),
            _ => None,
        }
    }

    /// Serialize the document, Flate-compressing every uncompressed stream.
    /// Already-filtered streams are left as-is (never double-compressed); a
    /// stream is only replaced when compression actually shrinks it.
    pub fn save_compressed(&self) -> Vec<u8> {
        let mut objects = self.objects.clone();
        for object in objects.values_mut() {
            if let Object::Stream(stream) = object {
                if stream.dict.contains(b"Filter") || stream.raw.len() <= 64 {
                    continue;
                }
                let compressed = crate::filters::deflate::flate_encode(&stream.raw);
                if compressed.len() < stream.raw.len() {
                    stream
                        .dict
                        .set(b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec()));
                    stream
                        .dict
                        .set(b"Length".to_vec(), Object::Integer(compressed.len() as i64));
                    stream.raw = compressed;
                }
            }
        }
        crate::serialize::to_pdf(&objects, &self.trailer)
    }

    /// Reading-order text lines of a page (structured text): each line's text
    /// plus its union bounding box. Replaces an external structured-text engine.
    pub fn structured_text(&self, page_no: u32) -> Vec<content::TextLine> {
        content::group_lines(&self.page_elements(page_no).unwrap_or_default())
    }

    /// Full-text search across the document. Returns one [`SearchMatch`] per line
    /// containing `query` (substring; `case_insensitive` folds ASCII case), with
    /// the line text and its bounding box for highlighting.
    pub fn search(&self, query: &str, case_insensitive: bool) -> Vec<SearchMatch> {
        let needle = if case_insensitive {
            query.to_lowercase()
        } else {
            query.to_string()
        };
        let mut matches = Vec::new();
        if needle.is_empty() {
            return matches;
        }
        for page in 1..=self.page_count() as u32 {
            for line in self.structured_text(page) {
                let hay = if case_insensitive {
                    line.text.to_lowercase()
                } else {
                    line.text.clone()
                };
                if hay.contains(&needle) {
                    matches.push(SearchMatch {
                        page,
                        text: line.text,
                        bounds: line.bounds,
                    });
                }
            }
        }
        matches
    }

    /// Extract the document's text, one run per line, pages separated by a form
    /// feed (`\x0C`). Font-aware (zero tofu).
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for page in 1..=self.page_count() as u32 {
            if let Ok(runs) = self.page_text_runs(page) {
                for run in runs {
                    out.push_str(&run.text);
                    out.push('\n');
                }
            }
            out.push('\u{000C}');
        }
        out
    }

    /// Convert the document to standalone HTML with absolutely-positioned,
    /// styled text (font/weight/colour) and inlined images — real selectable
    /// content, not a page raster. A reflow-level conversion (layout, not
    /// pixel-perfect rendering).
    pub fn to_html(&self) -> String {
        crate::convert::web::to_html(&self.convert_pages())
    }

    /// Build per-font render data (embedded TrueType program + decoder) from a
    /// page's `/Resources /Font`, for the rasterizer's glyph rendering.
    fn page_render_fonts(&self, page_no: u32) -> crate::raster::render::RenderFonts {
        let mut out = crate::raster::render::RenderFonts::new();
        let Ok(page) = self.page_dict(page_no) else {
            return out;
        };
        let font_dict = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|res| res.get(b"Font"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict);
        let Some(font_dict) = font_dict else {
            return out;
        };
        for (name, value) in &font_dict.0 {
            let Some(font) = self.resolve(value).as_dict() else {
                continue;
            };
            let two_byte =
                font.get(b"Subtype").and_then(Object::as_name) == Some(b"Type0".as_slice());
            let to_unicode = font
                .get(b"ToUnicode")
                .map(|o| self.resolve(o))
                .and_then(Object::as_stream)
                .and_then(|s| decode_stream(s).ok())
                .map(|bytes| crate::font::cmap::ToUnicode::parse(&bytes))
                .filter(|c| !c.is_empty());
            out.insert(
                name.clone(),
                crate::raster::render::RenderFont {
                    program: self.font_program(font),
                    decoder: crate::font::cmap::TextDecoder {
                        two_byte,
                        to_unicode,
                    },
                    two_byte,
                },
            );
        }
        out
    }

    /// Extract and parse the embedded glyph program of a font, descending into
    /// the CIDFont for a Type0 font. `/FontFile2` is TrueType; `/FontFile3` is
    /// CFF/OpenType (tried as both). Type1 (`/FontFile`) is not yet rasterized.
    fn font_program(&self, font: &Dictionary) -> Option<crate::font::GlyphSource> {
        let carrier = if font.get(b"Subtype").and_then(Object::as_name)
            == Some(b"Type0".as_slice())
        {
            font.get(b"DescendantFonts")
                .map(|o| self.resolve(o))
                .and_then(Object::as_array)
                .and_then(|a| a.first())
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)?
        } else {
            font
        };
        let descriptor = carrier
            .get(b"FontDescriptor")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;

        if let Some(bytes) = self.font_file_bytes(descriptor, b"FontFile2") {
            if let Some(ttf) = crate::font::truetype::TrueTypeFont::parse(&bytes) {
                return Some(crate::font::GlyphSource::TrueType(ttf));
            }
        }
        if let Some(bytes) = self.font_file_bytes(descriptor, b"FontFile3") {
            if let Some(cff) = crate::font::cff::CffFont::parse(&bytes) {
                return Some(crate::font::GlyphSource::Cff(cff));
            }
            if let Some(ttf) = crate::font::truetype::TrueTypeFont::parse(&bytes) {
                return Some(crate::font::GlyphSource::TrueType(ttf));
            }
        }
        None
    }

    fn font_file_bytes(&self, descriptor: &Dictionary, key: &[u8]) -> Option<Vec<u8>> {
        let stream = descriptor
            .get(key)
            .map(|o| self.resolve(o))
            .and_then(Object::as_stream)?;
        decode_stream(stream).ok()
    }

    /// Index of the element at page point `(x, y)` (user space), preferring the
    /// smallest box when several overlap. `None` if nothing is hit.
    pub fn element_at(&self, page_no: u32, x: f64, y: f64) -> Result<Option<usize>> {
        let elements = self.page_elements(page_no)?;
        let mut best: Option<(usize, f64)> = None;
        for element in &elements {
            if let Some(bounds) = element.bounds {
                if bounds.contains(x, y) {
                    let area = bounds.area();
                    if best.is_none_or(|(_, best_area)| area < best_area) {
                        best = Some((element.index, area));
                    }
                }
            }
        }
        Ok(best.map(|(index, _)| index))
    }

    /// Remove an element (text, image, or whole shape) by its index from
    /// [`page_elements`], preserving everything else.
    pub fn remove_element(&mut self, page_no: u32, index: usize) -> Result<()> {
        let content = self.page_content(page_no)?;
        let edited = content::remove_element(&content, index)?;
        self.set_page_content(page_no, edited)
    }

    /// Duplicate an element (text, image, or shape) in place.
    pub fn duplicate_element(&mut self, page_no: u32, index: usize) -> Result<()> {
        let content = self.page_content(page_no)?;
        let edited = content::duplicate_element(&content, index)?;
        self.set_page_content(page_no, edited)
    }

    /// Move an element (text, image, or shape) by `(dx, dy)` user-space units.
    pub fn move_element(&mut self, page_no: u32, index: usize, dx: f64, dy: f64) -> Result<()> {
        let content = self.page_content(page_no)?;
        let edited = content::move_element(&content, index, dx, dy)?;
        self.set_page_content(page_no, edited)
    }

    /// Draw a rectangle (frame / table cell / filled box) on a page. Colours are
    /// RGB in `0.0..=1.0`; pass `None` to skip stroke or fill.
    #[allow(clippy::too_many_arguments)]
    pub fn add_rectangle(
        &mut self,
        page_no: u32,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        stroke: Option<[f64; 3]>,
        fill: Option<[f64; 3]>,
        line_width: f64,
    ) -> Result<()> {
        let mut content = self.page_content(page_no)?;
        content.push(b'\n');
        content.extend_from_slice(&content::rectangle_ops(
            x, y, width, height, stroke, fill, line_width,
        ));
        self.set_page_content(page_no, content)
    }

    /// Draw a straight line (table rule / separator / underline) on a page.
    #[allow(clippy::too_many_arguments)]
    pub fn add_line(
        &mut self,
        page_no: u32,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        stroke: [f64; 3],
        line_width: f64,
    ) -> Result<()> {
        let mut content = self.page_content(page_no)?;
        content.push(b'\n');
        content.extend_from_slice(&content::line_ops(x1, y1, x2, y2, stroke, line_width));
        self.set_page_content(page_no, content)
    }

    // ─── annotations ─────────────────────────────────────────────────────────

    fn read_rect(&self, dict: &Dictionary) -> [f64; 4] {
        let mut rect = [0.0f64; 4];
        if let Some(items) = dict.get(b"Rect").map(|o| self.resolve(o)).and_then(Object::as_array) {
            for (i, value) in items.iter().take(4).enumerate() {
                rect[i] = self.resolve(value).as_f64().unwrap_or(0.0);
            }
        }
        rect
    }

    /// List a page's annotations.
    pub fn page_annotations(&self, page_no: u32) -> Result<Vec<Annotation>> {
        let page = self.page_dict(page_no)?;
        let items = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let mut out = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let Some(dict) = self.resolve(item).as_dict() else {
                continue;
            };
            let subtype = dict
                .get(b"Subtype")
                .and_then(Object::as_name)
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            let rect = self.read_rect(dict);
            let contents = match dict.get(b"Contents").map(|o| self.resolve(o)) {
                Some(Object::String(bytes, _)) => crate::font::decode_pdf_text(bytes),
                _ => String::new(),
            };
            out.push(Annotation {
                index,
                subtype,
                rect,
                contents,
            });
        }
        Ok(out)
    }

    /// Remove the annotation at `index` from a page's `/Annots`.
    pub fn remove_annotation(&mut self, page_no: u32, index: usize) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        let mut items = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        if index >= items.len() {
            return Err(EngineError::Missing(format!("annotation #{index}")));
        }
        items.remove(index);
        page.set(b"Annots".to_vec(), Object::Array(items));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    fn add_annotation(&mut self, page_no: u32, mut built: annot::Built) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;
        let rect = self.read_rect(&built.dict);

        let appearance_id = (self.next_object_number(), 0u16);
        let annotation_id = (appearance_id.0 + 1, 0u16);

        // Appearance form XObject.
        let mut form = Dictionary::new();
        form.set(b"Type".to_vec(), annot::name(b"XObject"));
        form.set(b"Subtype".to_vec(), annot::name(b"Form"));
        form.set(b"BBox".to_vec(), annot::real_array(&rect));
        form.set(b"Resources".to_vec(), Object::Dictionary(built.resources));
        form.set(
            b"Length".to_vec(),
            Object::Integer(built.appearance.len() as i64),
        );
        self.objects.insert(
            appearance_id,
            Object::Stream(Stream::new(form, built.appearance)),
        );

        // Annotation dict with /AP /N -> form.
        let mut appearance = Dictionary::new();
        appearance.set(b"N".to_vec(), Object::Reference(appearance_id));
        built.dict.set(b"AP".to_vec(), Object::Dictionary(appearance));
        built.dict.set(b"Type".to_vec(), annot::name(b"Annot"));
        self.objects
            .insert(annotation_id, Object::Dictionary(built.dict));

        // Append to the page's /Annots.
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        let mut items = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        items.push(Object::Reference(annotation_id));
        page.set(b"Annots".to_vec(), Object::Array(items));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    /// Add a rectangle (Square) annotation.
    pub fn add_square_annotation(
        &mut self,
        page_no: u32,
        rect: [f64; 4],
        stroke: Option<[f64; 3]>,
        fill: Option<[f64; 3]>,
        line_width: f64,
    ) -> Result<()> {
        self.add_annotation(page_no, annot::square(rect, stroke, fill, line_width))
    }

    /// Add a Highlight annotation (translucent colour over the rectangle).
    pub fn add_highlight(&mut self, page_no: u32, rect: [f64; 4], color: [f64; 3]) -> Result<()> {
        self.add_annotation(page_no, annot::highlight(rect, color))
    }

    /// Add a Line annotation.
    #[allow(clippy::too_many_arguments)]
    pub fn add_line_annotation(
        &mut self,
        page_no: u32,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        color: [f64; 3],
        line_width: f64,
    ) -> Result<()> {
        self.add_annotation(page_no, annot::line(x1, y1, x2, y2, color, line_width))
    }

    /// Add a FreeText annotation (a text box on the page).
    pub fn add_free_text(
        &mut self,
        page_no: u32,
        rect: [f64; 4],
        text: &str,
        font_size: f64,
        color: [f64; 3],
    ) -> Result<()> {
        self.add_annotation(page_no, annot::free_text(rect, text, font_size, color))
    }

    /// Embed a TrueType font program (`.ttf`, glyf-based) as a Type0 /
    /// CIDFontType2 font with Identity-H encoding, full per-glyph widths and a
    /// `ToUnicode` map. Returns the Type0 font's object number — pass it to
    /// [`add_text`](Self::add_text). The host downloads the bytes (e.g. via
    /// [`font::google::css_url`](crate::font::google::css_url)) and the engine
    /// bakes them in, so the output renders the same font everywhere.
    pub fn embed_truetype_font(&mut self, family: &str, ttf: &[u8]) -> Result<u32> {
        use crate::object::StringKind::Literal;
        let parsed = crate::font::truetype::TrueTypeFont::parse(ttf)
            .ok_or_else(|| EngineError::Unsupported("not a glyf-based TrueType font".into()))?;
        let ps_name = postscript_name(family);

        let advances = crate::font::embed::scaled_advances(&parsed);
        let unicode = crate::font::embed::gid_to_unicode(&parsed);
        let tounicode = crate::font::embed::to_unicode_cmap(&unicode);

        // Five consecutive ids: FontFile2, FontDescriptor, CIDFont, Type0, ToUnicode.
        let ff_id = (self.next_object_number(), 0u16);
        let fd_id = (ff_id.0 + 1, 0u16);
        let cid_id = (ff_id.0 + 2, 0u16);
        let t0_id = (ff_id.0 + 3, 0u16);
        let tu_id = (ff_id.0 + 4, 0u16);

        // FontFile2 — the raw program (compressed later by save_compressed).
        let mut ff = Dictionary::new();
        ff.set(b"Length".to_vec(), Object::Integer(ttf.len() as i64));
        ff.set(b"Length1".to_vec(), Object::Integer(ttf.len() as i64));
        self.objects
            .insert(ff_id, Object::Stream(Stream::new(ff, ttf.to_vec())));

        // FontDescriptor — generic metrics (fine for display; exact values would
        // need OS/2/hhea parsing).
        let mut fd = Dictionary::new();
        fd.set(b"Type", annot::name(b"FontDescriptor"));
        fd.set(b"FontName", annot::name(ps_name.as_bytes()));
        fd.set(b"Flags", Object::Integer(32)); // Nonsymbolic
        fd.set(
            b"FontBBox",
            Object::Array(vec![
                Object::Integer(0),
                Object::Integer(-200),
                Object::Integer(1000),
                Object::Integer(900),
            ]),
        );
        fd.set(b"ItalicAngle", Object::Integer(0));
        fd.set(b"Ascent", Object::Integer(800));
        fd.set(b"Descent", Object::Integer(-200));
        fd.set(b"CapHeight", Object::Integer(700));
        fd.set(b"StemV", Object::Integer(80));
        fd.set(b"FontFile2", Object::Reference(ff_id));
        self.objects.insert(fd_id, Object::Dictionary(fd));

        // CIDFontType2 with Identity CIDToGIDMap (CID = GID) and full widths.
        let w_inner: Vec<Object> = advances.iter().map(|&w| Object::Integer(w as i64)).collect();
        let mut cidsi = Dictionary::new();
        cidsi.set(b"Registry", Object::String(b"Adobe".to_vec(), Literal));
        cidsi.set(b"Ordering", Object::String(b"Identity".to_vec(), Literal));
        cidsi.set(b"Supplement", Object::Integer(0));
        let mut cid = Dictionary::new();
        cid.set(b"Type", annot::name(b"Font"));
        cid.set(b"Subtype", annot::name(b"CIDFontType2"));
        cid.set(b"BaseFont", annot::name(ps_name.as_bytes()));
        cid.set(b"CIDSystemInfo", Object::Dictionary(cidsi));
        cid.set(b"FontDescriptor", Object::Reference(fd_id));
        cid.set(b"CIDToGIDMap", annot::name(b"Identity"));
        cid.set(b"DW", Object::Integer(1000));
        cid.set(
            b"W",
            Object::Array(vec![Object::Integer(0), Object::Array(w_inner)]),
        );
        self.objects.insert(cid_id, Object::Dictionary(cid));

        // ToUnicode CMap (copy/extract round-trips).
        let mut tu = Dictionary::new();
        tu.set(b"Length", Object::Integer(tounicode.len() as i64));
        self.objects
            .insert(tu_id, Object::Stream(Stream::new(tu, tounicode)));

        // Type0 wrapper.
        let mut t0 = Dictionary::new();
        t0.set(b"Type", annot::name(b"Font"));
        t0.set(b"Subtype", annot::name(b"Type0"));
        t0.set(b"BaseFont", annot::name(ps_name.as_bytes()));
        t0.set(b"Encoding", annot::name(b"Identity-H"));
        t0.set(
            b"DescendantFonts",
            Object::Array(vec![Object::Reference(cid_id)]),
        );
        t0.set(b"ToUnicode", Object::Reference(tu_id));
        self.objects.insert(t0_id, Object::Dictionary(t0));

        Ok(t0_id.0)
    }

    /// Add a real, selectable text run to a page's content stream, set in a font
    /// previously embedded with [`embed_truetype_font`](Self::embed_truetype_font).
    /// `x`/`y` are the text origin in PDF user space (origin bottom-left); `size`
    /// is in points; `color` is the RGB fill `0..=1`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_text(
        &mut self,
        page_no: u32,
        x: f64,
        y: f64,
        size: f64,
        text: &str,
        font_obj: u32,
        color: [f64; 3],
    ) -> Result<()> {
        let ttf = self
            .embedded_truetype(font_obj)
            .ok_or_else(|| EngineError::Unsupported("font_obj is not an embedded TrueType font".into()))?;
        // Identity-H shows two-byte glyph ids directly.
        let mut hex = String::new();
        for ch in text.chars() {
            let gid = ttf.gid_for_unicode(ch as u32).unwrap_or(0);
            hex.push_str(&format!("{gid:04X}"));
        }
        let res_name = format!("GF{font_obj}");
        let snippet = format!(
            "\nq\n{r} {g} {b} rg\nBT\n/{res} {size} Tf\n{x} {y} Td\n<{hex}> Tj\nET\nQ\n",
            r = content::num(color[0]),
            g = content::num(color[1]),
            b = content::num(color[2]),
            res = res_name,
            size = content::num(size),
            x = content::num(x),
            y = content::num(y),
        );
        let mut content = self.page_content(page_no)?;
        content.extend_from_slice(snippet.as_bytes());
        self.set_page_content(page_no, content)?;
        self.register_page_font(page_no, res_name.as_bytes(), (font_obj, 0))?;
        Ok(())
    }

    /// List the `/BaseFont` names that the document **references but does not
    /// embed** — the fonts a host would download (Google Fonts) and embed to make
    /// the document self-contained or editable. Deduplicated, sorted.
    pub fn needed_fonts(&self) -> Vec<String> {
        let mut needed = std::collections::BTreeSet::new();
        for page_no in 1..=self.page_count() as u32 {
            let resources = self.effective_resources(page_no);
            let Some(fonts) = resources
                .get(b"Font")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
            else {
                continue;
            };
            for value in fonts.0.values() {
                let Some(font) = self.resolve(value).as_dict() else {
                    continue;
                };
                if self.font_is_embedded(font) {
                    continue;
                }
                if let Some(base) = font.get(b"BaseFont").and_then(Object::as_name) {
                    // Strip a subset prefix ("ABCDEF+") for a clean family name.
                    let name = String::from_utf8_lossy(base);
                    let clean = name.split_once('+').map_or(name.as_ref(), |(_, n)| n);
                    needed.insert(clean.to_string());
                }
            }
        }
        needed.into_iter().collect()
    }

    /// Whether a font dictionary embeds its program (`FontFile`/`2`/`3`), looking
    /// through a Type0's descendant `FontDescriptor`.
    fn font_is_embedded(&self, font: &Dictionary) -> bool {
        let descriptor = if font.get(b"Subtype").and_then(Object::as_name) == Some(b"Type0") {
            font.get(b"DescendantFonts")
                .map(|o| self.resolve(o))
                .and_then(Object::as_array)
                .and_then(<[Object]>::first)
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
                .and_then(|cid| cid.get(b"FontDescriptor"))
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
        } else {
            font.get(b"FontDescriptor")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
        };
        descriptor.is_some_and(|fd| {
            fd.contains(b"FontFile") || fd.contains(b"FontFile2") || fd.contains(b"FontFile3")
        })
    }

    /// Parse the embedded TrueType program behind a Type0 font object, by walking
    /// Type0 → DescendantFonts → FontDescriptor → FontFile2.
    fn embedded_truetype(&self, font_obj: u32) -> Option<crate::font::truetype::TrueTypeFont> {
        let t0 = self.objects.get(&(font_obj, 0)).and_then(Object::as_dict)?;
        let desc = t0
            .get(b"DescendantFonts")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)?
            .first()?;
        let cid = self.resolve(desc).as_dict()?;
        let fd = cid
            .get(b"FontDescriptor")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;
        let ff = fd
            .get(b"FontFile2")
            .map(|o| self.resolve(o))
            .and_then(Object::as_stream)?;
        let bytes = decode_stream(ff).ok()?;
        crate::font::truetype::TrueTypeFont::parse(&bytes)
    }

    /// The nearest `/Resources` dictionary up the page tree (own or inherited),
    /// cloned so the caller can mutate and re-attach it to the page.
    fn effective_resources(&self, page_no: u32) -> Dictionary {
        let Ok(page_id) = self.page_object_id(page_no) else {
            return Dictionary::new();
        };
        let mut current = self.objects.get(&page_id).and_then(Object::as_dict).cloned();
        while let Some(dict) = current {
            if let Some(res) = dict
                .get(b"Resources")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
            {
                return res.clone();
            }
            current = dict
                .get(b"Parent")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
                .cloned();
        }
        Dictionary::new()
    }

    /// Register `name -> font_ref` in a page's `/Resources /Font`, preserving any
    /// inherited resources by materializing them onto the page first.
    fn register_page_font(&mut self, page_no: u32, name: &[u8], font_ref: ObjectId) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;
        let mut resources = self.effective_resources(page_no);
        let mut fonts = resources
            .get(b"Font")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        fonts.set(name.to_vec(), Object::Reference(font_ref));
        resources.set(b"Font".to_vec(), Object::Dictionary(fonts));
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        page.set(b"Resources".to_vec(), Object::Dictionary(resources));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    /// Add an Underline annotation under a text rectangle.
    pub fn add_underline(&mut self, page_no: u32, rect: [f64; 4], color: [f64; 3]) -> Result<()> {
        self.add_annotation(page_no, annot::underline(rect, color))
    }

    /// Add a StrikeOut annotation through a text rectangle.
    pub fn add_strike_out(&mut self, page_no: u32, rect: [f64; 4], color: [f64; 3]) -> Result<()> {
        self.add_annotation(page_no, annot::strike_out(rect, color))
    }

    /// Add an Ink (freehand) annotation from one or more polylines (each a list
    /// of `(x, y)` points in page user space).
    pub fn add_ink(
        &mut self,
        page_no: u32,
        paths: &[Vec<(f64, f64)>],
        color: [f64; 3],
        line_width: f64,
    ) -> Result<()> {
        self.add_annotation(page_no, annot::ink(paths, color, line_width))
    }

    /// Add a rubber-stamp annotation (a labelled, bordered box).
    pub fn add_stamp(
        &mut self,
        page_no: u32,
        rect: [f64; 4],
        label: &str,
        color: [f64; 3],
    ) -> Result<()> {
        self.add_annotation(page_no, annot::stamp(rect, label, color))
    }

    /// The form-XObject id of an annotation's normal appearance (`/AP /N`),
    /// resolving an appearance-state sub-dictionary via `/AS` when present.
    fn annotation_appearance_id(&self, dict: &Dictionary) -> Option<ObjectId> {
        let normal = dict
            .get(b"AP")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?
            .get(b"N")?;
        if let Some(id) = normal.as_reference() {
            if self.objects.get(&id).and_then(Object::as_stream).is_some() {
                return Some(id);
            }
        }
        let states = self.resolve(normal).as_dict()?;
        if let Some(key) = dict.get(b"AS").and_then(Object::as_name) {
            if let Some(id) = states.get(key).and_then(Object::as_reference) {
                return Some(id);
            }
        }
        states.0.values().find_map(Object::as_reference)
    }

    /// "Flatten" a page's annotations: paint each annotation's appearance into
    /// the page content as an XObject, then drop the `/Annots` markup. Returns
    /// how many annotations were baked. Annotations without an appearance are
    /// left untouched (and the markup is kept if any couldn't be baked).
    pub fn flatten_annotations(&mut self, page_no: u32) -> Result<usize> {
        let page_id = self.page_object_id(page_no)?;
        let page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        let annots = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => return Ok(0),
        };

        let mut forms: Vec<ObjectId> = Vec::new();
        let mut all_bakeable = true;
        for item in &annots {
            match self.resolve(item).as_dict() {
                Some(dict) => match self.annotation_appearance_id(dict) {
                    Some(id) => forms.push(id),
                    None => all_bakeable = false,
                },
                None => all_bakeable = false,
            }
        }
        if forms.is_empty() {
            return Ok(0);
        }

        // A content stream that draws every appearance form, named uniquely.
        let mut draw = Vec::new();
        let mut xobjects = Dictionary::new();
        for (i, form_id) in forms.iter().enumerate() {
            let resource_name = format!("GpFlat{i}");
            xobjects.set(
                resource_name.clone().into_bytes(),
                Object::Reference(*form_id),
            );
            draw.extend_from_slice(format!("q /{resource_name} Do Q\n").as_bytes());
        }
        let draw_id = (self.next_object_number(), 0u16);
        let mut draw_dict = Dictionary::new();
        draw_dict.set(b"Length".to_vec(), Object::Integer(draw.len() as i64));
        self.objects
            .insert(draw_id, Object::Stream(Stream::new(draw_dict, draw)));

        // Re-fetch and edit the page: append the draw stream to /Contents,
        // merge the XObject resources, and drop the baked annotations.
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();

        let mut contents = match page.get(b"Contents").map(|o| self.resolve(o)) {
            Some(Object::Array(items)) => items.clone(),
            Some(_) => vec![page.get(b"Contents").cloned().unwrap()],
            None => Vec::new(),
        };
        contents.push(Object::Reference(draw_id));
        page.set(b"Contents".to_vec(), Object::Array(contents));

        let mut resources = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        let mut xobject_dict = resources
            .get(b"XObject")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        for (key, value) in xobjects.0 {
            xobject_dict.0.insert(key, value);
        }
        resources.set(b"XObject".to_vec(), Object::Dictionary(xobject_dict));
        page.set(b"Resources".to_vec(), Object::Dictionary(resources));

        // Only drop the markup if every annotation was baked; otherwise keep the
        // un-bakeable ones rather than silently losing them.
        if all_bakeable {
            page.remove(b"Annots");
        }
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(forms.len())
    }

    // ─── page operations & metadata ──────────────────────────────────────────

    /// Set a page's rotation (normalized to 0/90/180/270 degrees).
    pub fn rotate_page(&mut self, page_no: u32, degrees: i32) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;
        let normalized = (degrees.rem_euclid(360) / 90) * 90;
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        page.set(b"Rotate".to_vec(), Object::Integer(normalized as i64));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    /// The /Pages tree node whose /Kids contains `child`, if any.
    fn find_kids_parent(&self, child: ObjectId) -> Option<ObjectId> {
        for (id, object) in &self.objects {
            if let Some(kids) = object.as_dict().and_then(|d| d.get(b"Kids")).and_then(Object::as_array) {
                if kids.iter().any(|o| o.as_reference() == Some(child)) {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// Decrement /Count on `node` and all its /Parent ancestors.
    fn decrement_count(&mut self, start: ObjectId) {
        let mut node = start;
        for _ in 0..64 {
            let mut dict = match self.objects.get(&node).and_then(Object::as_dict) {
                Some(d) => d.clone(),
                None => break,
            };
            let count = dict.get(b"Count").and_then(Object::as_i64).unwrap_or(0);
            dict.set(b"Count".to_vec(), Object::Integer((count - 1).max(0)));
            let parent = dict.get(b"Parent").and_then(Object::as_reference);
            self.objects.insert(node, Object::Dictionary(dict));
            match parent {
                Some(p) => node = p,
                None => break,
            }
        }
    }

    /// Delete a page (cannot delete the last remaining page).
    pub fn delete_page(&mut self, page_no: u32) -> Result<()> {
        if self.page_count() <= 1 {
            return Err(EngineError::Unsupported("cannot delete the only page".into()));
        }
        let page_id = self.page_object_id(page_no)?;
        let parent_id = self
            .find_kids_parent(page_id)
            .ok_or_else(|| EngineError::Missing("page tree parent".into()))?;

        let mut parent = self
            .objects
            .get(&parent_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("page tree parent".into()))?
            .clone();
        if let Some(kids) = parent.get(b"Kids").and_then(Object::as_array) {
            let remaining: Vec<Object> = kids
                .iter()
                .filter(|o| o.as_reference() != Some(page_id))
                .cloned()
                .collect();
            parent.set(b"Kids".to_vec(), Object::Array(remaining));
        }
        self.objects.insert(parent_id, Object::Dictionary(parent));
        self.decrement_count(parent_id);
        Ok(())
    }

    /// Rebuild the page tree as a single flat `/Pages` node with `ordered` pages.
    fn rebuild_page_tree(&mut self, ordered: &[ObjectId]) -> Result<()> {
        let root_id = self
            .catalog()?
            .get(b"Pages")
            .and_then(Object::as_reference)
            .ok_or_else(|| EngineError::Missing("catalog /Pages".into()))?;

        let mut root = self
            .objects
            .get(&root_id)
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        root.set(b"Type".to_vec(), Object::Name(b"Pages".to_vec()));
        root.set(
            b"Kids".to_vec(),
            Object::Array(ordered.iter().map(|id| Object::Reference(*id)).collect()),
        );
        root.set(b"Count".to_vec(), Object::Integer(ordered.len() as i64));
        root.remove(b"Parent");
        self.objects.insert(root_id, Object::Dictionary(root));

        for id in ordered {
            if let Some(mut page) = self.objects.get(id).and_then(Object::as_dict).cloned() {
                page.set(b"Parent".to_vec(), Object::Reference(root_id));
                self.objects.insert(*id, Object::Dictionary(page));
            }
        }
        Ok(())
    }

    /// Move a page from 1-based position `from` to 1-based position `to`.
    pub fn move_page(&mut self, from: u32, to: u32) -> Result<()> {
        let mut ids = self.page_ids()?;
        let len = ids.len();
        let from = from.saturating_sub(1) as usize;
        let to = to.saturating_sub(1) as usize;
        if from >= len || to >= len {
            return Err(EngineError::PageNotFound((from.max(to) + 1) as u32));
        }
        let id = ids.remove(from);
        ids.insert(to.min(ids.len()), id);
        self.rebuild_page_tree(&ids)
    }

    /// Drop every object not reachable from the trailer's `/Root` or `/Info`.
    fn gc(&mut self) {
        let mut reachable: BTreeSet<ObjectId> = BTreeSet::new();
        let mut stack: Vec<ObjectId> = Vec::new();
        for key in [b"Root".as_slice(), b"Info".as_slice()] {
            if let Some(id) = self.trailer.get(key).and_then(Object::as_reference) {
                stack.push(id);
            }
        }
        while let Some(id) = stack.pop() {
            if !reachable.insert(id) {
                continue;
            }
            if let Some(object) = self.objects.get(&id) {
                collect_refs(object, &mut stack);
            }
        }
        self.objects.retain(|id, _| reachable.contains(id));
    }

    /// Produce a new PDF containing only the given 1-based pages, in that order.
    pub fn extract_pages(&self, pages: &[u32]) -> Result<Vec<u8>> {
        let all = self.page_ids()?;
        let selected: Vec<ObjectId> = pages
            .iter()
            .filter_map(|&p| all.get(p.saturating_sub(1) as usize).copied())
            .collect();
        if selected.is_empty() {
            return Err(EngineError::PageNotFound(0));
        }
        let mut clone = self.clone();
        clone.rebuild_page_tree(&selected)?;
        clone.gc();
        Ok(clone.save())
    }

    /// Append all pages of another PDF to the end of this document.
    pub fn append_pages_from(&mut self, other_pdf: &[u8]) -> Result<()> {
        let other = Document::open(other_pdf)?;
        let other_pages = other.page_ids()?;

        // Objects reachable from the other document's pages.
        let mut reachable: Vec<ObjectId> = Vec::new();
        let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
        let mut stack = other_pages.clone();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(object) = other.objects.get(&id) {
                reachable.push(id);
                collect_refs(object, &mut stack);
            }
        }

        // Allocate fresh ids in this document and copy + remap.
        let mut next = self.next_object_number();
        let mut map: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
        for &id in &reachable {
            map.insert(id, (next, 0));
            next += 1;
        }
        for &id in &reachable {
            if let Some(object) = other.objects.get(&id) {
                self.objects.insert(map[&id], remap_object(object, &map));
            }
        }

        // Attach the new pages under this document's root.
        let root_id = self
            .catalog()?
            .get(b"Pages")
            .and_then(Object::as_reference)
            .ok_or_else(|| EngineError::Missing("catalog /Pages".into()))?;
        let mut root = self
            .objects
            .get(&root_id)
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        let mut kids = root
            .get(b"Kids")
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        let count = root.get(b"Count").and_then(Object::as_i64).unwrap_or(kids.len() as i64);

        for &page in &other_pages {
            let new_page = map[&page];
            kids.push(Object::Reference(new_page));
            if let Some(mut page_dict) = self.objects.get(&new_page).and_then(Object::as_dict).cloned() {
                page_dict.set(b"Parent".to_vec(), Object::Reference(root_id));
                self.objects.insert(new_page, Object::Dictionary(page_dict));
            }
        }
        root.set(b"Kids".to_vec(), Object::Array(kids));
        root.set(
            b"Count".to_vec(),
            Object::Integer(count + other_pages.len() as i64),
        );
        self.objects.insert(root_id, Object::Dictionary(root));
        Ok(())
    }

    /// The document's `/Info` dictionary id, creating it if absent.
    fn info_dict_id(&mut self) -> ObjectId {
        if let Some(id) = self.trailer.get(b"Info").and_then(Object::as_reference) {
            return id;
        }
        let id = (self.next_object_number(), 0u16);
        self.objects.insert(id, Object::Dictionary(Dictionary::new()));
        self.trailer.set(b"Info".to_vec(), Object::Reference(id));
        id
    }

    /// Set a document metadata entry (e.g. "Title", "Author", "Subject",
    /// "Keywords", "Creator", "Producer").
    pub fn set_metadata(&mut self, key: &str, value: &str) -> Result<()> {
        let id = self.info_dict_id();
        let mut info = self
            .objects
            .get(&id)
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        info.set(
            key.as_bytes().to_vec(),
            Object::String(crate::font::encode_pdf_text(value), StringKind::Literal),
        );
        self.objects.insert(id, Object::Dictionary(info));
        Ok(())
    }

    /// Read a document metadata entry.
    pub fn get_metadata(&self, key: &str) -> Option<String> {
        let info = self.trailer.get(b"Info").map(|o| self.resolve(o))?;
        match info.as_dict()?.get(key.as_bytes()).map(|o| self.resolve(o)) {
            Some(Object::String(bytes, _)) => Some(crate::font::decode_pdf_text(bytes)),
            _ => None,
        }
    }

    // ─── destinations, hyperlinks & outline ──────────────────────────────────

    /// Object id of the document catalog (the `/Root`).
    fn catalog_id(&self) -> Result<ObjectId> {
        if let Some(id) = self.trailer.get(b"Root").and_then(Object::as_reference) {
            return Ok(id);
        }
        // Fallback: the id of any /Type /Catalog object.
        self.objects
            .iter()
            .find(|(_, obj)| {
                obj.as_dict()
                    .and_then(|d| d.get(b"Type"))
                    .and_then(Object::as_name)
                    == Some(b"Catalog".as_slice())
            })
            .map(|(id, _)| *id)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))
    }

    /// 1-based page number of a page object id, if it is a page.
    fn page_number_of(&self, target: ObjectId) -> Option<u32> {
        self.page_ids()
            .ok()?
            .iter()
            .position(|id| *id == target)
            .map(|i| i as u32 + 1)
    }

    /// Resolve a named destination (catalog `/Dests` dict or `/Names /Dests`
    /// name tree, top level) to its destination object.
    fn lookup_named_dest(&self, key: &[u8]) -> Option<Object> {
        let catalog = self.catalog().ok()?;
        // PDF 1.1 style: catalog /Dests is a dictionary of name -> dest.
        if let Some(dests) = catalog.get(b"Dests").map(|o| self.resolve(o)).and_then(Object::as_dict) {
            if let Some(entry) = dests.get(key) {
                let resolved = self.resolve(entry);
                // A named dest may wrap its array in a /D dictionary entry.
                if let Some(d) = resolved.as_dict().and_then(|d| d.get(b"D")) {
                    return Some(self.resolve(d).clone());
                }
                return Some(resolved.clone());
            }
        }
        // PDF 1.2+ style: catalog /Names /Dests is a name tree.
        let names = catalog
            .get(b"Names")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?
            .get(b"Dests")
            .map(|o| self.resolve(o))?;
        self.search_name_tree(names, key, 0)
    }

    /// Walk a name tree looking for `key`, returning the associated value.
    fn search_name_tree(&self, node: &Object, key: &[u8], depth: usize) -> Option<Object> {
        if depth > 32 {
            return None;
        }
        let dict = self.resolve(node).as_dict()?;
        if let Some(names) = dict.get(b"Names").map(|o| self.resolve(o)).and_then(Object::as_array) {
            let mut i = 0;
            while i + 1 < names.len() {
                if let Object::String(bytes, _) = self.resolve(&names[i]) {
                    if bytes.as_slice() == key {
                        let value = self.resolve(&names[i + 1]);
                        if let Some(d) = value.as_dict().and_then(|d| d.get(b"D")) {
                            return Some(self.resolve(d).clone());
                        }
                        return Some(value.clone());
                    }
                }
                i += 2;
            }
        }
        if let Some(kids) = dict.get(b"Kids").map(|o| self.resolve(o)).and_then(Object::as_array) {
            for kid in kids {
                if let Some(found) = self.search_name_tree(kid, key, depth + 1) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// Resolve a destination object (`[pageRef …]` array, or a named dest as a
    /// name/string) to a 1-based page number.
    fn dest_to_page(&self, dest: &Object) -> Option<u32> {
        match self.resolve(dest) {
            Object::Array(items) => {
                let page_id = items.first()?.as_reference()?;
                self.page_number_of(page_id)
            }
            Object::Name(name) => {
                let target = self.lookup_named_dest(name)?;
                self.dest_to_page(&target)
            }
            Object::String(bytes, _) => {
                let target = self.lookup_named_dest(bytes)?;
                self.dest_to_page(&target)
            }
            _ => None,
        }
    }

    /// Destination page of an annotation/outline dict, from `/Dest` or a
    /// `/A << /S /GoTo /D … >>` action.
    fn dest_page_of(&self, dict: &Dictionary) -> Option<u32> {
        if let Some(dest) = dict.get(b"Dest") {
            if let Some(page) = self.dest_to_page(dest) {
                return Some(page);
            }
        }
        let action = dict.get(b"A").map(|o| self.resolve(o)).and_then(Object::as_dict)?;
        if action.get(b"S").and_then(Object::as_name) == Some(b"GoTo".as_slice()) {
            if let Some(d) = action.get(b"D") {
                return self.dest_to_page(d);
            }
        }
        None
    }

    /// List a page's hyperlink annotations.
    pub fn page_links(&self, page_no: u32) -> Result<Vec<Link>> {
        let page = self.page_dict(page_no)?;
        let items = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let mut out = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let Some(dict) = self.resolve(item).as_dict() else {
                continue;
            };
            if dict.get(b"Subtype").and_then(Object::as_name) != Some(b"Link".as_slice()) {
                continue;
            }
            let rect = self.read_rect(dict);
            let target = self.link_target(dict);
            out.push(Link { index, rect, target });
        }
        Ok(out)
    }

    fn link_target(&self, dict: &Dictionary) -> LinkTarget {
        if let Some(action) = dict.get(b"A").map(|o| self.resolve(o)).and_then(Object::as_dict) {
            if action.get(b"S").and_then(Object::as_name) == Some(b"URI".as_slice()) {
                if let Some(Object::String(bytes, _)) = action.get(b"URI").map(|o| self.resolve(o)) {
                    return LinkTarget::Uri(String::from_utf8_lossy(bytes).into_owned());
                }
            }
        }
        match self.dest_page_of(dict) {
            Some(page) => LinkTarget::Page(page),
            None => LinkTarget::Unknown,
        }
    }

    /// Append a ready-made annotation dictionary to a page's `/Annots`.
    fn append_annotation_dict(&mut self, page_no: u32, dict: Dictionary) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;
        let annotation_id = (self.next_object_number(), 0u16);
        self.objects.insert(annotation_id, Object::Dictionary(dict));
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        let mut items = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        items.push(Object::Reference(annotation_id));
        page.set(b"Annots".to_vec(), Object::Array(items));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    fn base_link_dict(rect: [f64; 4]) -> Dictionary {
        let mut dict = Dictionary::new();
        dict.set(b"Type".to_vec(), annot::name(b"Annot"));
        dict.set(b"Subtype".to_vec(), annot::name(b"Link"));
        dict.set(b"Rect".to_vec(), annot::real_array(&rect));
        // A zero-width border so the link has no visible outline.
        dict.set(
            b"Border".to_vec(),
            Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(0),
            ]),
        );
        dict
    }

    /// Add a hyperlink to an external URI over `rect`.
    pub fn add_uri_link(&mut self, page_no: u32, rect: [f64; 4], uri: &str) -> Result<()> {
        let mut dict = Self::base_link_dict(rect);
        let mut action = Dictionary::new();
        action.set(b"Type".to_vec(), annot::name(b"Action"));
        action.set(b"S".to_vec(), annot::name(b"URI"));
        action.set(
            b"URI".to_vec(),
            Object::String(uri.as_bytes().to_vec(), StringKind::Literal),
        );
        dict.set(b"A".to_vec(), Object::Dictionary(action));
        self.append_annotation_dict(page_no, dict)
    }

    /// Add an internal hyperlink over `rect` that jumps to `target_page`.
    pub fn add_goto_link(&mut self, page_no: u32, rect: [f64; 4], target_page: u32) -> Result<()> {
        let target_id = self.page_object_id(target_page)?;
        let mut dict = Self::base_link_dict(rect);
        dict.set(
            b"Dest".to_vec(),
            Object::Array(vec![Object::Reference(target_id), annot::name(b"Fit")]),
        );
        self.append_annotation_dict(page_no, dict)
    }

    /// The document outline (bookmarks) flattened in reading order.
    pub fn outline_items(&self) -> Vec<OutlineItem> {
        let mut out = Vec::new();
        let root = match self
            .catalog()
            .ok()
            .and_then(|c| c.get(b"Outlines"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
        {
            Some(dict) => dict,
            None => return out,
        };
        if let Some(first) = root.get(b"First").and_then(Object::as_reference) {
            self.walk_outline(first, 0, &mut out, 0);
        }
        out
    }

    fn walk_outline(&self, start: ObjectId, level: usize, out: &mut Vec<OutlineItem>, depth: usize) {
        if depth > 64 {
            return;
        }
        let mut current = Some(start);
        let mut guard = 0;
        while let Some(id) = current {
            guard += 1;
            if guard > 100_000 {
                break; // defend against a cyclic /Next chain
            }
            let Some(dict) = self.objects.get(&id).and_then(Object::as_dict) else {
                break;
            };
            let title = dict.get(b"Title").map(|o| self.string_value(o)).unwrap_or_default();
            let page = self.dest_page_of(dict);
            out.push(OutlineItem { title, level, page });
            if let Some(child) = dict.get(b"First").and_then(Object::as_reference) {
                self.walk_outline(child, level + 1, out, depth + 1);
            }
            current = dict.get(b"Next").and_then(Object::as_reference);
        }
    }

    /// Replace the entire document outline from a flat `(title, page, level)`
    /// list (pre-order; `level` 0 = top). An empty list clears the outline.
    pub fn set_outline(&mut self, items: &[(String, Option<u32>, usize)]) -> Result<()> {
        let catalog_id = self.catalog_id()?;
        if items.is_empty() {
            if let Some(mut catalog) = self.objects.get(&catalog_id).and_then(Object::as_dict).cloned() {
                catalog.remove(b"Outlines");
                self.objects.insert(catalog_id, Object::Dictionary(catalog));
            }
            return Ok(());
        }

        let base = self.next_object_number();
        let outlines_id = (base, 0u16);
        let item_ids: Vec<ObjectId> =
            (0..items.len()).map(|i| (base + 1 + i as u32, 0u16)).collect();

        // Tree linkage computed from the flat level list via an ancestor stack.
        let mut parent = vec![outlines_id; items.len()];
        let mut prev_idx: Vec<Option<usize>> = vec![None; items.len()];
        let mut next_idx: Vec<Option<usize>> = vec![None; items.len()];
        let mut first_child: BTreeMap<ObjectId, usize> = BTreeMap::new();
        let mut last_child: BTreeMap<ObjectId, usize> = BTreeMap::new();
        let mut stack: Vec<usize> = Vec::new();

        for i in 0..items.len() {
            let level = items[i].2;
            while let Some(&top) = stack.last() {
                if items[top].2 >= level {
                    stack.pop();
                } else {
                    break;
                }
            }
            let parent_id = stack.last().map(|&t| item_ids[t]).unwrap_or(outlines_id);
            parent[i] = parent_id;
            if let Some(&prev) = last_child.get(&parent_id) {
                next_idx[prev] = Some(i);
                prev_idx[i] = Some(prev);
            } else {
                first_child.insert(parent_id, i);
            }
            last_child.insert(parent_id, i);
            stack.push(i);
        }

        // Number of descendants of item `i` = contiguous block of deeper levels.
        let subtree_size = |i: usize| -> usize {
            let level = items[i].2;
            items[i + 1..]
                .iter()
                .take_while(|(_, _, l)| *l > level)
                .count()
        };

        for (i, (title, page, _)) in items.iter().enumerate() {
            let id = item_ids[i];
            let mut dict = Dictionary::new();
            dict.set(
                b"Title".to_vec(),
                Object::String(crate::font::encode_pdf_text(title), StringKind::Literal),
            );
            dict.set(b"Parent".to_vec(), Object::Reference(parent[i]));
            if let Some(prev) = prev_idx[i] {
                dict.set(b"Prev".to_vec(), Object::Reference(item_ids[prev]));
            }
            if let Some(next) = next_idx[i] {
                dict.set(b"Next".to_vec(), Object::Reference(item_ids[next]));
            }
            if let Some(&child) = first_child.get(&id) {
                dict.set(b"First".to_vec(), Object::Reference(item_ids[child]));
            }
            if let Some(&child) = last_child.get(&id) {
                dict.set(b"Last".to_vec(), Object::Reference(item_ids[child]));
            }
            let descendants = subtree_size(i);
            if descendants > 0 {
                // Positive: the item is open, showing all its descendants.
                dict.set(b"Count".to_vec(), Object::Integer(descendants as i64));
            }
            if let Some(p) = page {
                if let Ok(target_id) = self.page_object_id(*p) {
                    dict.set(
                        b"Dest".to_vec(),
                        Object::Array(vec![Object::Reference(target_id), annot::name(b"Fit")]),
                    );
                }
            }
            self.objects.insert(id, Object::Dictionary(dict));
        }

        // The /Outlines root.
        let mut root = Dictionary::new();
        root.set(b"Type".to_vec(), annot::name(b"Outlines"));
        if let Some(&child) = first_child.get(&outlines_id) {
            root.set(b"First".to_vec(), Object::Reference(item_ids[child]));
        }
        if let Some(&child) = last_child.get(&outlines_id) {
            root.set(b"Last".to_vec(), Object::Reference(item_ids[child]));
        }
        root.set(b"Count".to_vec(), Object::Integer(items.len() as i64));
        self.objects.insert(outlines_id, Object::Dictionary(root));

        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        catalog.set(b"Outlines".to_vec(), Object::Reference(outlines_id));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));
        Ok(())
    }

    // ─── optional content (layers / OCG) ─────────────────────────────────────

    /// The document's optional-content layers (PDF OCGs), ordered as in the
    /// default configuration's `/Order` (then discovery order).
    pub fn layers(&self) -> Vec<Layer> {
        let Some(ocp) = self
            .catalog()
            .ok()
            .and_then(|c| c.get(b"OCProperties"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
        else {
            return Vec::new();
        };
        let Some(ocgs) = ocp.get(b"OCGs").map(|o| self.resolve(o)).and_then(Object::as_array) else {
            return Vec::new();
        };
        let cfg = ocp.get(b"D").map(|o| self.resolve(o)).and_then(Object::as_dict);
        let off = self.oc_ref_ids(cfg.and_then(|c| c.get(b"OFF")));
        let locked = self.oc_ref_ids(cfg.and_then(|c| c.get(b"Locked")));
        let mut order = Vec::new();
        self.oc_order_ids(cfg.and_then(|c| c.get(b"Order")), &mut order);

        let mut out = Vec::new();
        for obj in ocgs {
            let Some(oid) = obj.as_reference() else { continue };
            let name = self
                .objects
                .get(&oid)
                .and_then(Object::as_dict)
                .and_then(|d| d.get(b"Name"))
                .map(|o| self.string_value(o))
                .unwrap_or_default();
            out.push(Layer {
                id: oid.0,
                name,
                visible: !off.contains(&oid.0),
                locked: locked.contains(&oid.0),
                order: order.iter().position(|&x| x == oid.0).unwrap_or(usize::MAX),
            });
        }
        // /Order entries first (ascending), then any remaining in discovery order.
        for (i, layer) in out.iter_mut().enumerate() {
            if layer.order == usize::MAX {
                layer.order = order.len() + i;
            }
        }
        out.sort_by_key(|l| l.order);
        out
    }

    /// Create a new (initially visible, unlocked) optional-content layer.
    /// Returns the OCG's object number — the id for the toggle/remove calls.
    pub fn add_layer(&mut self, name: &str) -> Result<u32> {
        let ocg_id = (self.next_object_number(), 0u16);
        let mut ocg = Dictionary::new();
        ocg.set(b"Type".to_vec(), annot::name(b"OCG"));
        ocg.set(
            b"Name".to_vec(),
            Object::String(crate::font::encode_pdf_text(name), StringKind::Literal),
        );
        self.objects.insert(ocg_id, Object::Dictionary(ocg));

        let catalog_id = self.catalog_id()?;
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        let mut ocp = catalog
            .get(b"OCProperties")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(Dictionary::new);
        let mut ocgs = ocp
            .get(b"OCGs")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        ocgs.push(Object::Reference(ocg_id));
        ocp.set(b"OCGs".to_vec(), Object::Array(ocgs));

        let mut cfg = ocp
            .get(b"D")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(Dictionary::new);
        if cfg.get(b"Name").is_none() {
            cfg.set(b"Name".to_vec(), Object::String(b"Default".to_vec(), StringKind::Literal));
        }
        let mut order = cfg
            .get(b"Order")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        order.push(Object::Reference(ocg_id));
        cfg.set(b"Order".to_vec(), Object::Array(order));
        ocp.set(b"D".to_vec(), Object::Dictionary(cfg));

        catalog.set(b"OCProperties".to_vec(), Object::Dictionary(ocp));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));
        Ok(ocg_id.0)
    }

    /// Show or hide a layer (membership of `/D /OFF`).
    pub fn set_layer_visibility(&mut self, layer_id: u32, visible: bool) -> Result<()> {
        let oid = self
            .oc_object_id(layer_id)
            .ok_or_else(|| EngineError::Missing("optional content group".into()))?;
        self.with_oc_default_config(|cfg| Self::set_oc_membership(cfg, b"OFF", oid, !visible))
    }

    /// Lock or unlock a layer (membership of `/D /Locked`).
    pub fn set_layer_locked(&mut self, layer_id: u32, locked: bool) -> Result<()> {
        let oid = self
            .oc_object_id(layer_id)
            .ok_or_else(|| EngineError::Missing("optional content group".into()))?;
        self.with_oc_default_config(|cfg| Self::set_oc_membership(cfg, b"Locked", oid, locked))
    }

    /// Remove a layer from the optional-content configuration. Content still
    /// tagged with the OCG renders unconditionally afterwards (spec-compliant).
    pub fn remove_layer(&mut self, layer_id: u32) -> Result<()> {
        let catalog_id = self.catalog_id()?;
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        let Some(mut ocp) = catalog
            .get(b"OCProperties")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
        else {
            return Ok(());
        };
        if let Some(mut ocgs) = ocp.get(b"OCGs").and_then(Object::as_array).map(<[Object]>::to_vec) {
            ocgs.retain(|o| o.as_reference().map(|r| r.0) != Some(layer_id));
            ocp.set(b"OCGs".to_vec(), Object::Array(ocgs));
        }
        if let Some(mut cfg) = ocp.get(b"D").map(|o| self.resolve(o)).and_then(Object::as_dict).cloned()
        {
            for key in [b"OFF".as_ref(), b"ON", b"Locked", b"Order"] {
                if let Some(mut arr) = cfg.get(key).and_then(Object::as_array).map(<[Object]>::to_vec) {
                    Self::remove_oc_ref_deep(&mut arr, layer_id);
                    if arr.is_empty() {
                        cfg.remove(key);
                    } else {
                        cfg.set(key.to_vec(), Object::Array(arr));
                    }
                }
            }
            ocp.set(b"D".to_vec(), Object::Dictionary(cfg));
        }
        catalog.set(b"OCProperties".to_vec(), Object::Dictionary(ocp));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));
        Ok(())
    }

    /// Resolve a layer's object number to its full `ObjectId` (preserving the
    /// generation) by locating it in `/OCProperties /OCGs`.
    fn oc_object_id(&self, layer_id: u32) -> Option<ObjectId> {
        self.catalog()
            .ok()
            .and_then(|c| c.get(b"OCProperties"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|ocp| ocp.get(b"OCGs"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .and_then(|arr| arr.iter().filter_map(|o| o.as_reference()).find(|r| r.0 == layer_id))
    }

    /// Object numbers of the top-level references in an `/OFF`-style array.
    fn oc_ref_ids(&self, obj: Option<&Object>) -> Vec<u32> {
        obj.map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(|arr| arr.iter().filter_map(|o| o.as_reference().map(|r| r.0)).collect())
            .unwrap_or_default()
    }

    /// Flatten the (possibly nested) `/Order` array into layer object numbers.
    fn oc_order_ids(&self, obj: Option<&Object>, out: &mut Vec<u32>) {
        if let Some(arr) = obj.map(|o| self.resolve(o)).and_then(Object::as_array) {
            for item in arr {
                match item {
                    Object::Reference(r) => out.push(r.0),
                    Object::Array(_) => self.oc_order_ids(Some(item), out),
                    _ => {}
                }
            }
        }
    }

    /// Get-or-create the default OC configuration (`/OCProperties /D`), apply
    /// `f`, and write it back through the catalog.
    fn with_oc_default_config<F: FnOnce(&mut Dictionary)>(&mut self, f: F) -> Result<()> {
        let catalog_id = self.catalog_id()?;
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        let mut ocp = catalog
            .get(b"OCProperties")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(Dictionary::new);
        let mut cfg = ocp
            .get(b"D")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(Dictionary::new);
        f(&mut cfg);
        ocp.set(b"D".to_vec(), Object::Dictionary(cfg));
        catalog.set(b"OCProperties".to_vec(), Object::Dictionary(ocp));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));
        Ok(())
    }

    /// Ensure `oid` is present in (or absent from) `cfg[key]`, dropping the key
    /// when the resulting array is empty.
    fn set_oc_membership(cfg: &mut Dictionary, key: &[u8], oid: ObjectId, present: bool) {
        let mut arr: Vec<Object> = cfg.get(key).and_then(Object::as_array).map(<[Object]>::to_vec).unwrap_or_default();
        arr.retain(|o| o.as_reference().map(|r| r.0) != Some(oid.0));
        if present {
            arr.push(Object::Reference(oid));
        }
        if arr.is_empty() {
            cfg.remove(key);
        } else {
            cfg.set(key.to_vec(), Object::Array(arr));
        }
    }

    /// Remove every reference to `layer_id` from an array, recursing into nested
    /// `/Order` sub-arrays.
    fn remove_oc_ref_deep(arr: &mut Vec<Object>, layer_id: u32) {
        arr.retain(|o| o.as_reference().map(|r| r.0) != Some(layer_id));
        for o in arr.iter_mut() {
            if let Object::Array(inner) = o {
                Self::remove_oc_ref_deep(inner, layer_id);
            }
        }
    }

    // ─── page structure (resize / insert / duplicate) ────────────────────────

    /// Set a page's `/MediaBox` to `[0 0 width height]` (points).
    pub fn resize_page(&mut self, page_no: u32, width: f64, height: f64) -> Result<()> {
        let id = self.page_object_id(page_no)?;
        let mut page = self
            .objects
            .get(&id)
            .and_then(Object::as_dict)
            .cloned()
            .ok_or(EngineError::PageNotFound(page_no))?;
        page.set(b"MediaBox".to_vec(), Self::media_box_array(width, height));
        self.objects.insert(id, Object::Dictionary(page));
        Ok(())
    }

    /// Insert a blank page of `width`×`height` points immediately after the
    /// 1-based `after` page (`after == 0` prepends). Returns its object number.
    pub fn add_page(&mut self, width: f64, height: f64, after: u32) -> Result<u32> {
        let content_id = (self.next_object_number(), 0u16);
        self.objects
            .insert(content_id, Object::Stream(Stream::new(Dictionary::new(), Vec::new())));
        let page_id = (self.next_object_number(), 0u16);
        let mut page = Dictionary::new();
        page.set(b"Type".to_vec(), annot::name(b"Page"));
        page.set(b"MediaBox".to_vec(), Self::media_box_array(width, height));
        page.set(b"Contents".to_vec(), Object::Reference(content_id));
        page.set(b"Resources".to_vec(), Object::Dictionary(Dictionary::new()));
        self.objects.insert(page_id, Object::Dictionary(page));
        self.insert_page_after(page_id, after)?;
        Ok(page_id.0)
    }

    /// Duplicate the 1-based `page_no`, inserting the copy right after it. The
    /// content streams are cloned (independent edits); resources are shared.
    /// Returns the new page's object number.
    pub fn copy_page(&mut self, page_no: u32) -> Result<u32> {
        let src_id = self.page_object_id(page_no)?;
        let mut page = self
            .objects
            .get(&src_id)
            .and_then(Object::as_dict)
            .cloned()
            .ok_or(EngineError::PageNotFound(page_no))?;
        let new_contents = self.clone_page_contents(&page);
        page.set(b"Contents".to_vec(), new_contents);
        let new_page_id = (self.next_object_number(), 0u16);
        self.objects.insert(new_page_id, Object::Dictionary(page));
        self.insert_page_after(new_page_id, page_no)?;
        Ok(new_page_id.0)
    }

    fn media_box_array(width: f64, height: f64) -> Object {
        Object::Array(vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Real(width),
            Object::Real(height),
        ])
    }

    /// Clone a page's content stream object(s) into fresh objects and return the
    /// new `/Contents` value (a single reference, or an array of them).
    fn clone_page_contents(&mut self, page: &Dictionary) -> Object {
        let Some(contents) = page.get(b"Contents").cloned() else {
            return Object::Null;
        };
        let stream_ids: Vec<ObjectId> = match &contents {
            Object::Reference(r) => match self.objects.get(r) {
                Some(Object::Array(arr)) => arr.iter().filter_map(Object::as_reference).collect(),
                Some(_) => vec![*r],
                None => Vec::new(),
            },
            Object::Array(arr) => arr.iter().filter_map(Object::as_reference).collect(),
            _ => Vec::new(),
        };
        let mut new_refs = Vec::new();
        for sid in stream_ids {
            if let Some(obj) = self.objects.get(&sid).cloned() {
                let nid = (self.next_object_number(), 0u16);
                self.objects.insert(nid, obj);
                new_refs.push(Object::Reference(nid));
            }
        }
        match new_refs.len() {
            0 => Object::Null,
            1 => new_refs.into_iter().next().unwrap(),
            _ => Object::Array(new_refs),
        }
    }

    /// Insert `new_page_id` into the page tree just after the 1-based `after`
    /// page (`0` = front). Sets the new page's `/Parent` and bumps `/Count` up
    /// the ancestor chain.
    fn insert_page_after(&mut self, new_page_id: ObjectId, after: u32) -> Result<()> {
        let ids = self.page_ids()?;
        if ids.is_empty() {
            return Err(EngineError::Missing("document has no pages".into()));
        }
        let ref_idx = (after.max(1) as usize - 1).min(ids.len() - 1);
        let ref_page_id = ids[ref_idx];
        let parent_id = self
            .objects
            .get(&ref_page_id)
            .and_then(Object::as_dict)
            .and_then(|d| d.get(b"Parent"))
            .and_then(Object::as_reference)
            .ok_or_else(|| EngineError::Missing("page /Parent".into()))?;

        let mut parent = self
            .objects
            .get(&parent_id)
            .and_then(Object::as_dict)
            .cloned()
            .ok_or_else(|| EngineError::Missing("pages tree node".into()))?;
        let mut kids = parent
            .get(b"Kids")
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        let pos = if after == 0 {
            0
        } else {
            kids.iter()
                .position(|o| o.as_reference() == Some(ref_page_id))
                .map(|p| p + 1)
                .unwrap_or(kids.len())
        };
        kids.insert(pos, Object::Reference(new_page_id));
        parent.set(b"Kids".to_vec(), Object::Array(kids));
        self.objects.insert(parent_id, Object::Dictionary(parent));

        if let Some(mut page) = self.objects.get(&new_page_id).and_then(Object::as_dict).cloned() {
            page.set(b"Parent".to_vec(), Object::Reference(parent_id));
            self.objects.insert(new_page_id, Object::Dictionary(page));
        }

        // Increment /Count on the parent and every ancestor Pages node.
        let mut node = Some(parent_id);
        let mut guard = 0;
        while let Some(nid) = node {
            guard += 1;
            if guard > 64 {
                break;
            }
            let Some(mut d) = self.objects.get(&nid).and_then(Object::as_dict).cloned() else {
                break;
            };
            let count = d.get(b"Count").and_then(Object::as_i64).unwrap_or(0);
            d.set(b"Count".to_vec(), Object::Integer(count + 1));
            let up = d.get(b"Parent").and_then(Object::as_reference);
            self.objects.insert(nid, Object::Dictionary(d));
            node = up;
        }
        Ok(())
    }

    // ─── interactive forms (AcroForm) ────────────────────────────────────────

    fn string_value(&self, object: &Object) -> String {
        match self.resolve(object) {
            Object::String(bytes, _) => crate::font::decode_pdf_text(bytes),
            Object::Name(name) => String::from_utf8_lossy(name).into_owned(),
            _ => String::new(),
        }
    }

    /// List the document's interactive form fields.
    pub fn form_fields(&self) -> Result<Vec<FormField>> {
        let mut out = Vec::new();
        let acroform = match self.catalog().ok().and_then(|c| c.get(b"AcroForm")) {
            Some(obj) => self.resolve(obj).clone(),
            None => return Ok(out),
        };
        let fields = acroform
            .as_dict()
            .and_then(|d| d.get(b"Fields"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        for field in &fields {
            self.collect_field(field, "", &mut out, 0);
        }
        Ok(out)
    }

    fn collect_field(&self, field: &Object, prefix: &str, out: &mut Vec<FormField>, depth: usize) {
        if depth > 32 {
            return;
        }
        let Some(dict) = self.resolve(field).as_dict() else {
            return;
        };
        let partial = dict.get(b"T").map(|o| self.string_value(o)).unwrap_or_default();
        let name = match (prefix.is_empty(), partial.is_empty()) {
            (true, _) => partial.clone(),
            (false, true) => prefix.to_string(),
            (false, false) => format!("{prefix}.{partial}"),
        };

        // A field with kids that are themselves named fields is a branch node.
        if let Some(kids) = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            let has_named_kids = kids.iter().any(|k| {
                self.resolve(k)
                    .as_dict()
                    .is_some_and(|d| d.contains(b"T"))
            });
            if has_named_kids {
                for kid in kids {
                    self.collect_field(kid, &name, out, depth + 1);
                }
                return;
            }
        }

        let field_type = dict
            .get(b"FT")
            .and_then(Object::as_name)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        let value = self.field_value_string(dict);
        let flags = dict.get(b"Ff").and_then(Object::as_i64).unwrap_or(0) as u32;
        let max_len = dict
            .get(b"MaxLen")
            .and_then(Object::as_i64)
            .filter(|n| *n >= 0)
            .map(|n| n as u32);
        let options = match field_type.as_str() {
            "Ch" => self
                .choice_options(dict)
                .into_iter()
                .map(|(_, display)| display)
                .collect(),
            "Btn" => self.button_export_states(dict),
            _ => Vec::new(),
        };
        out.push(FormField {
            name,
            field_type,
            value,
            flags,
            options,
            max_len,
        });
    }

    /// Read a field's `/V` as a display string, joining array values (multi-
    /// select choice) with newlines.
    fn field_value_string(&self, dict: &Dictionary) -> String {
        match dict.get(b"V").map(|o| self.resolve(o)) {
            Some(Object::Array(items)) => items
                .iter()
                .map(|i| self.string_value(i))
                .collect::<Vec<_>>()
                .join("\n"),
            Some(other) => self.string_value(other),
            None => String::new(),
        }
    }

    /// Choice `/Opt` entries as `(export, display)` pairs. An entry may be a
    /// bare string (export == display) or a `[export, display]` array.
    fn choice_options(&self, dict: &Dictionary) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(opt) = dict
            .get(b"Opt")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            for entry in opt {
                match self.resolve(entry) {
                    Object::Array(pair) if pair.len() >= 2 => {
                        out.push((self.string_value(&pair[0]), self.string_value(&pair[1])));
                    }
                    Object::Array(pair) if pair.len() == 1 => {
                        let s = self.string_value(&pair[0]);
                        out.push((s.clone(), s));
                    }
                    other => {
                        let s = self.string_value(other);
                        out.push((s.clone(), s));
                    }
                }
            }
        }
        out
    }

    /// Export "on" states of a button field (the non-`Off` keys of every
    /// widget's `/AP /N` appearance sub-dictionary), de-duplicated in order.
    fn button_export_states(&self, dict: &Dictionary) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push_from = |this: &Self, d: &Dictionary| {
            if let Some(states) = d
                .get(b"AP")
                .map(|o| this.resolve(o))
                .and_then(Object::as_dict)
                .and_then(|ap| ap.get(b"N"))
                .map(|o| this.resolve(o))
                .and_then(Object::as_dict)
            {
                for key in states.0.keys() {
                    if key.as_slice() != b"Off" {
                        let name = String::from_utf8_lossy(key).into_owned();
                        if !out.contains(&name) {
                            out.push(name);
                        }
                    }
                }
            }
        };
        push_from(self, dict);
        if let Some(kids) = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            for kid in kids {
                if let Some(kid_dict) = self.resolve(kid).as_dict() {
                    push_from(self, kid_dict);
                }
            }
        }
        out
    }

    /// Object id of a terminal field with the given fully-qualified name.
    fn find_field_id(&self, target: &str) -> Option<ObjectId> {
        let acroform = self.catalog().ok()?.get(b"AcroForm").map(|o| self.resolve(o))?;
        let fields = acroform
            .as_dict()?
            .get(b"Fields")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)?
            .to_vec();
        fields
            .iter()
            .find_map(|f| self.find_field_rec(f, "", target, 0))
    }

    fn find_field_rec(&self, field: &Object, prefix: &str, target: &str, depth: usize) -> Option<ObjectId> {
        if depth > 32 {
            return None;
        }
        let id = field.as_reference();
        let dict = self.resolve(field).as_dict()?;
        let partial = dict.get(b"T").map(|o| self.string_value(o)).unwrap_or_default();
        let name = match (prefix.is_empty(), partial.is_empty()) {
            (true, _) => partial.clone(),
            (false, true) => prefix.to_string(),
            (false, false) => format!("{prefix}.{partial}"),
        };
        if let Some(kids) = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            let has_named_kids = kids
                .iter()
                .any(|k| self.resolve(k).as_dict().is_some_and(|d| d.contains(b"T")));
            if has_named_kids {
                return kids
                    .iter()
                    .find_map(|k| self.find_field_rec(k, &name, target, depth + 1));
            }
        }
        if name == target {
            id
        } else {
            None
        }
    }

    fn set_need_appearances(&mut self) {
        let acro_id = match self
            .catalog()
            .ok()
            .and_then(|c| c.get(b"AcroForm"))
            .and_then(Object::as_reference)
        {
            Some(id) => id,
            None => return,
        };
        if let Some(mut acro) = self.objects.get(&acro_id).and_then(Object::as_dict).cloned() {
            acro.set(b"NeedAppearances".to_vec(), Object::Boolean(true));
            self.objects.insert(acro_id, Object::Dictionary(acro));
        }
    }

    /// Regenerate a widget's `/AP /N` to display `text`, or flag the form for
    /// viewer-side regeneration when the field has no own rectangle.
    fn regenerate_text_appearance(&mut self, widget: &mut Dictionary, text: &str) {
        if !widget.contains(b"Rect") {
            self.set_need_appearances();
            return;
        }
        let rect = self.read_rect(widget);
        let (mut form, content) = build_text_field_appearance(rect, text);
        form.set(b"Length".to_vec(), Object::Integer(content.len() as i64));
        let ap_id = (self.next_object_number(), 0u16);
        self.objects
            .insert(ap_id, Object::Stream(Stream::new(form, content)));
        let mut appearance = Dictionary::new();
        appearance.set(b"N".to_vec(), Object::Reference(ap_id));
        widget.set(b"AP".to_vec(), Object::Dictionary(appearance));
    }

    fn require_field(&self, name: &str) -> Result<(ObjectId, Dictionary)> {
        let id = self
            .find_field_id(name)
            .ok_or_else(|| EngineError::Missing(format!("form field '{name}'")))?;
        let dict = self
            .objects
            .get(&id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing(format!("form field '{name}'")))?
            .clone();
        Ok((id, dict))
    }

    /// Fill a text field (single- or multi-line) by fully-qualified name,
    /// regenerating its appearance. `/MaxLen` is honoured by truncation.
    pub fn set_text_field(&mut self, name: &str, value: &str) -> Result<()> {
        let (id, mut dict) = self.require_field(name)?;
        let value = match dict.get(b"MaxLen").and_then(Object::as_i64) {
            Some(max) if max >= 0 && !value.contains('\n') => {
                value.chars().take(max as usize).collect::<String>()
            }
            _ => value.to_string(),
        };
        dict.set(
            b"V".to_vec(),
            Object::String(crate::font::encode_pdf_text(&value), StringKind::Literal),
        );
        self.regenerate_text_appearance(&mut dict, &value);
        self.objects.insert(id, Object::Dictionary(dict));
        Ok(())
    }

    /// First non-`Off` appearance state of a widget's `/AP /N`, if any.
    fn widget_on_state(&self, dict: &Dictionary) -> Option<Vec<u8>> {
        let states = dict
            .get(b"AP")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|ap| ap.get(b"N"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;
        states
            .0
            .keys()
            .find(|k| k.as_slice() != b"Off")
            .cloned()
    }

    /// The "on" state of a checkbox, looking at the field and then its widgets.
    fn checkbox_on_state(&self, dict: &Dictionary) -> Vec<u8> {
        if let Some(state) = self.widget_on_state(dict) {
            return state;
        }
        if let Some(kids) = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            for kid in kids {
                if let Some(kid_dict) = self.resolve(kid).as_dict() {
                    if let Some(state) = self.widget_on_state(kid_dict) {
                        return state;
                    }
                }
            }
        }
        b"Yes".to_vec()
    }

    /// Check or uncheck a checkbox by fully-qualified name. The appearance
    /// state `/AS` is set on the field and on every widget kid.
    pub fn set_checkbox(&mut self, name: &str, checked: bool) -> Result<()> {
        let (id, mut dict) = self.require_field(name)?;
        let state = if checked {
            self.checkbox_on_state(&dict)
        } else {
            b"Off".to_vec()
        };
        dict.set(b"V".to_vec(), Object::Name(state.clone()));
        dict.set(b"AS".to_vec(), Object::Name(state.clone()));
        let kids: Vec<ObjectId> = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(|a| a.iter().filter_map(Object::as_reference).collect())
            .unwrap_or_default();
        for kid_id in kids {
            if let Some(mut kid) = self.objects.get(&kid_id).and_then(Object::as_dict).cloned() {
                kid.set(b"AS".to_vec(), Object::Name(state.clone()));
                self.objects.insert(kid_id, Object::Dictionary(kid));
            }
        }
        self.objects.insert(id, Object::Dictionary(dict));
        Ok(())
    }

    /// Select one option of a radio-button group by its export value. Every
    /// widget kid's `/AS` is set to that value (matching kid) or `/Off`.
    pub fn set_radio(&mut self, name: &str, export_value: &str) -> Result<()> {
        let (id, mut dict) = self.require_field(name)?;
        let target = export_value.as_bytes().to_vec();
        let mut matched = false;

        let kids: Vec<ObjectId> = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(|a| a.iter().filter_map(Object::as_reference).collect())
            .unwrap_or_default();

        if kids.is_empty() {
            // A radio whose single widget is merged into the field object.
            if self.widget_on_state(&dict).as_deref() == Some(target.as_slice()) {
                matched = true;
            }
            dict.set(b"AS".to_vec(), Object::Name(target.clone()));
        } else {
            for kid_id in &kids {
                let Some(mut kid) = self.objects.get(kid_id).and_then(Object::as_dict).cloned()
                else {
                    continue;
                };
                let is_target = self.widget_on_state(&kid).as_deref() == Some(target.as_slice());
                let state = if is_target {
                    matched = true;
                    target.clone()
                } else {
                    b"Off".to_vec()
                };
                kid.set(b"AS".to_vec(), Object::Name(state));
                self.objects.insert(*kid_id, Object::Dictionary(kid));
            }
        }

        if !matched {
            return Err(EngineError::Unsupported(format!(
                "radio '{name}' has no option '{export_value}'"
            )));
        }
        dict.set(b"V".to_vec(), Object::Name(target));
        self.objects.insert(id, Object::Dictionary(dict));
        Ok(())
    }

    /// Set the selection of a choice field (combo box or list box) by
    /// fully-qualified name. Values match an option's export *or* display
    /// string; an editable combo also accepts a free-text value. `/V`, `/I`
    /// (indices) and the appearance are updated.
    pub fn set_choice_field(&mut self, name: &str, values: &[&str]) -> Result<()> {
        let (id, mut dict) = self.require_field(name)?;
        let options = self.choice_options(&dict);
        let flags = dict.get(b"Ff").and_then(Object::as_i64).unwrap_or(0) as u32;
        let editable = flags & crate::form::flags::COMBO != 0
            && flags & crate::form::flags::EDIT != 0;

        // Resolve each requested value to (export, display, index).
        let mut chosen: Vec<(String, String, Option<usize>)> = Vec::new();
        for &want in values {
            if let Some((idx, (export, display))) = options
                .iter()
                .enumerate()
                .find(|(_, (e, d))| e == want || d == want)
            {
                chosen.push((export.clone(), display.clone(), Some(idx)));
            } else if editable {
                chosen.push((want.to_string(), want.to_string(), None));
            } else {
                return Err(EngineError::Unsupported(format!(
                    "choice field '{name}' has no option '{want}'"
                )));
            }
        }

        // /V: a single string, or an array for a multi-selection.
        if chosen.len() <= 1 {
            let export = chosen.first().map(|c| c.0.clone()).unwrap_or_default();
            dict.set(
                b"V".to_vec(),
                Object::String(crate::font::encode_pdf_text(&export), StringKind::Literal),
            );
        } else {
            let array = chosen
                .iter()
                .map(|c| Object::String(crate::font::encode_pdf_text(&c.0), StringKind::Literal))
                .collect();
            dict.set(b"V".to_vec(), Object::Array(array));
        }

        // /I: selected indices (ascending), omitted when nothing is indexable.
        let mut indices: Vec<i64> = chosen.iter().filter_map(|c| c.2).map(|i| i as i64).collect();
        indices.sort_unstable();
        if indices.is_empty() {
            dict.remove(b"I");
        } else {
            dict.set(
                b"I".to_vec(),
                Object::Array(indices.into_iter().map(Object::Integer).collect()),
            );
        }

        let display = chosen
            .iter()
            .map(|c| c.1.clone())
            .collect::<Vec<_>>()
            .join("\n");
        self.regenerate_text_appearance(&mut dict, &display);
        self.objects.insert(id, Object::Dictionary(dict));
        Ok(())
    }

    /// Replace a page's content with `content` bytes, stored as a single new
    /// uncompressed stream. The page `/Contents` is repointed at it.
    pub fn set_page_content(&mut self, page_no: u32, content: Vec<u8>) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;

        let new_id = (self.next_object_number(), 0u16);
        let mut dict = Dictionary::new();
        dict.set(b"Length".to_vec(), Object::Integer(content.len() as i64));
        self.objects
            .insert(new_id, Object::Stream(Stream::new(dict, content)));

        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        page.set(b"Contents".to_vec(), Object::Reference(new_id));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    fn append_content(&self, object: &Object, out: &mut Vec<u8>) -> Result<()> {
        match self.resolve(object) {
            Object::Stream(stream) => {
                let decoded = decode_stream(stream)?;
                out.extend_from_slice(&decoded);
                out.push(b'\n');
            }
            Object::Array(items) => {
                for item in items {
                    self.append_content(item, out)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Decrypt every object's strings and stream bytes in place when the trailer
/// declares an `/Encrypt` dictionary. A wrong or unsupported password leaves
/// the objects untouched (the document stays unreadable rather than corrupted).
fn decrypt_objects(
    objects: &mut BTreeMap<ObjectId, Object>,
    trailer: &Dictionary,
    password: &[u8],
) -> Result<()> {
    let Some(encrypt_ref) = trailer.get(b"Encrypt").and_then(Object::as_reference) else {
        return Ok(()); // not encrypted
    };
    let id0 = match trailer.get(b"ID") {
        Some(Object::Array(items)) => match items.first() {
            Some(Object::String(b, _)) => b.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    let Some(encrypt_dict) = objects.get(&encrypt_ref).and_then(Object::as_dict).cloned() else {
        return Ok(()); // malformed /Encrypt reference — best effort, leave as-is
    };
    let Some(security) = crate::security::Security::open(&encrypt_dict, &id0, password) else {
        return Err(EngineError::Unsupported(
            "encrypted PDF: wrong password or unsupported security handler".into(),
        ));
    };
    let ids: Vec<ObjectId> = objects.keys().copied().collect();
    for id in ids {
        if id == encrypt_ref {
            continue;
        }
        // Cross-reference streams are never encrypted.
        let is_xref = objects
            .get(&id)
            .and_then(Object::as_dict)
            .and_then(|d| d.get(b"Type"))
            .and_then(Object::as_name)
            == Some(b"XRef".as_slice());
        if is_xref {
            continue;
        }
        if let Some(obj) = objects.remove(&id) {
            objects.insert(id, decrypt_in_object(obj, id.0, id.1, &security));
        }
    }
    Ok(())
}

fn decrypt_in_object(
    object: Object,
    num: u32,
    gen: u16,
    sec: &crate::security::Security,
) -> Object {
    match object {
        Object::String(bytes, kind) => Object::String(sec.decrypt(num, gen, &bytes), kind),
        Object::Array(items) => Object::Array(
            items
                .into_iter()
                .map(|o| decrypt_in_object(o, num, gen, sec))
                .collect(),
        ),
        Object::Dictionary(dict) => Object::Dictionary(decrypt_in_dict(dict, num, gen, sec)),
        Object::Stream(stream) => {
            let dict = decrypt_in_dict(stream.dict, num, gen, sec);
            let raw = sec.decrypt(num, gen, &stream.raw);
            Object::Stream(Stream::new(dict, raw))
        }
        other => other,
    }
}

fn decrypt_in_dict(
    dict: Dictionary,
    num: u32,
    gen: u16,
    sec: &crate::security::Security,
) -> Dictionary {
    let mut out = Dictionary::new();
    for (key, value) in dict.0 {
        out.0.insert(key, decrypt_in_object(value, num, gen, sec));
    }
    out
}

/// Sanitize a family name into a valid PostScript `/BaseFont` name (ASCII
/// letters/digits/hyphen; spaces and other characters dropped).
fn postscript_name(family: &str) -> String {
    let cleaned: String = family
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if cleaned.is_empty() {
        "EmbeddedFont".to_string()
    } else {
        cleaned
    }
}

/// First index of `needle` within `haystack`.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Scan a whole PDF for `n g obj` definitions and `trailer` dictionaries.
fn scan(data: &[u8]) -> (BTreeMap<ObjectId, Object>, Dictionary) {
    let mut objects = BTreeMap::new();
    let mut trailer = Dictionary::new();
    let mut lexer = Lexer::new(data);

    loop {
        let token = match lexer.next_token() {
            Ok(Token::Eof) => break,
            Ok(token) => token,
            Err(_) => continue, // lexer guarantees progress on error
        };

        match token {
            Token::Integer(n) if n >= 0 => {
                let after_n = lexer.position();
                if let Some(id) = try_object_header(&mut lexer, n) {
                    let body_start = lexer.position();
                    let mut parser = Parser::at(data, body_start);
                    if let Ok(object) = parser.parse_value() {
                        objects.insert(id, object);
                        lexer.set_position(parser.position());
                        continue;
                    }
                }
                lexer.set_position(after_n);
            }
            Token::Keyword(k) if k == b"trailer" => {
                let mut parser = Parser::at(data, lexer.position());
                if let Ok(Object::Dictionary(dict)) = parser.parse_value() {
                    for (key, value) in dict.0 {
                        trailer.0.insert(key, value); // last trailer wins (most recent)
                    }
                    lexer.set_position(parser.position());
                }
            }
            _ => {}
        }
    }

    (objects, trailer)
}

/// After an `Integer(n)`, check for `g obj`. Returns the object id on match,
/// leaving the lexer right after `obj`. On no match the lexer is left wherever
/// it stopped; callers rewind.
fn try_object_header(lexer: &mut Lexer, n: i64) -> Option<ObjectId> {
    let g = match lexer.next_token() {
        Ok(Token::Integer(g)) if (0..=u16::MAX as i64).contains(&g) => g,
        _ => return None,
    };
    match lexer.next_token() {
        Ok(Token::Keyword(k)) if k == b"obj" => Some((n as u32, g as u16)),
        _ => None,
    }
}

/// PDF 1.5+ keeps `/Root` in the cross-reference *stream* dictionary rather than
/// a classic `trailer`. If the scanned trailer lacks `/Root`, lift it (and
/// `/Info`) from any `/Type /XRef` stream object.
fn recover_trailer_from_xref(trailer: &mut Dictionary, objects: &BTreeMap<ObjectId, Object>) {
    if trailer.contains(b"Root") {
        return;
    }
    for object in objects.values() {
        let Some(stream) = object.as_stream() else {
            continue;
        };
        if stream.dict.get(b"Type").and_then(Object::as_name) != Some(b"XRef".as_slice()) {
            continue;
        }
        if let Some(root) = stream.dict.get(b"Root") {
            trailer.set(b"Root".to_vec(), root.clone());
        }
        if let Some(info) = stream.dict.get(b"Info") {
            trailer.set(b"Info".to_vec(), info.clone());
        }
        if trailer.contains(b"Root") {
            return;
        }
    }
}

/// PDF 1.5+ packs non-stream objects (catalog, pages, fonts…) into compressed
/// `/Type /ObjStm` streams. Decode each and add the objects it carries to the
/// map, without overriding objects already found directly.
fn extract_object_streams(objects: &mut BTreeMap<ObjectId, Object>) {
    let streams: Vec<Stream> = objects
        .values()
        .filter_map(Object::as_stream)
        .filter(|s| s.dict.get(b"Type").and_then(Object::as_name) == Some(b"ObjStm".as_slice()))
        .cloned()
        .collect();

    for stream in streams {
        let decoded = match decode_stream(&stream) {
            Ok(bytes) => bytes,
            Err(_) => continue, // a bad ObjStm must not fail the whole open
        };
        let count = stream
            .dict
            .get(b"N")
            .and_then(Object::as_i64)
            .unwrap_or(0)
            .max(0) as usize;
        let first = stream
            .dict
            .get(b"First")
            .and_then(Object::as_i64)
            .unwrap_or(0)
            .max(0) as usize;

        // The decoded stream starts with `count` pairs of (object number, offset).
        let mut header = Parser::new(&decoded);
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let number = match header.parse_value() {
                Ok(Object::Integer(v)) if v >= 0 => v as u32,
                _ => break,
            };
            let offset = match header.parse_value() {
                Ok(Object::Integer(v)) if v >= 0 => v as usize,
                _ => break,
            };
            entries.push((number, offset));
        }

        for (number, offset) in entries {
            let pos = first + offset;
            if pos >= decoded.len() {
                continue;
            }
            let mut parser = Parser::at(&decoded, pos);
            if let Ok(object) = parser.parse_value() {
                objects.entry((number, 0)).or_insert(object);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> Vec<u8> {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("../../fixtures");
        path.push(name);
        std::fs::read(&path).unwrap_or_else(|_| panic!("missing fixture {}", path.display()))
    }

    fn has_op(content: &[u8], op: &[u8]) -> bool {
        content.windows(op.len()).any(|w| w == op)
    }

    #[test]
    fn layers_create_toggle_remove_roundtrip() {
        let pdf = crate::convert::reverse::txt_to_pdf("layer test");
        let mut doc = Document::open(&pdf).unwrap();
        assert!(doc.layers().is_empty());

        // Create → visible, unlocked.
        let id = doc.add_layer("Watermark").unwrap();
        assert!(id > 0);
        let layers = doc.layers();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].name, "Watermark");
        assert!(layers[0].visible && !layers[0].locked);

        // Hide + lock.
        doc.set_layer_visibility(id, false).unwrap();
        doc.set_layer_locked(id, true).unwrap();
        let layers = doc.layers();
        assert!(!layers[0].visible && layers[0].locked);

        // Survives a save/open round-trip.
        let reopened = Document::open(&doc.save()).unwrap();
        let layers = reopened.layers();
        assert_eq!(layers.len(), 1);
        assert!(!layers[0].visible && layers[0].locked);

        // Show again, then remove.
        doc.set_layer_visibility(id, true).unwrap();
        assert!(doc.layers()[0].visible);
        doc.remove_layer(id).unwrap();
        assert!(doc.layers().is_empty());
    }

    #[test]
    fn page_resize_add_copy_roundtrip() {
        let pdf = crate::convert::reverse::txt_to_pdf("page ops");
        let mut doc = Document::open(&pdf).unwrap();
        assert_eq!(doc.page_ids().unwrap().len(), 1);

        doc.resize_page(1, 200.0, 300.0).unwrap();
        let (w, h) = {
            let mb = doc.page_dict(1).unwrap().get(b"MediaBox").and_then(Object::as_array).unwrap();
            (mb[2].as_f64(), mb[3].as_f64())
        };
        assert_eq!((w, h), (Some(200.0), Some(300.0)));

        assert!(doc.add_page(400.0, 500.0, 1).unwrap() > 0);
        assert_eq!(doc.page_ids().unwrap().len(), 2);

        assert!(doc.copy_page(1).unwrap() > 0);
        assert_eq!(doc.page_ids().unwrap().len(), 3);

        let reopened = Document::open(&doc.save()).unwrap();
        assert_eq!(reopened.page_ids().unwrap().len(), 3);
    }

    #[test]
    fn opens_simple_text_and_decodes_content() {
        let doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let ids = doc.page_ids().unwrap();
        assert!(!ids.is_empty(), "expected at least one page");

        let content = doc.page_content(1).unwrap();
        assert!(
            has_op(&content, b"Tj") || has_op(&content, b"TJ"),
            "decoded content should contain a text operator ({} bytes)",
            content.len()
        );
    }

    #[test]
    fn opens_pdf_with_image_background() {
        // The "complex background" case: text drawn over an image.
        let doc = Document::open(&fixture("with-images.pdf")).unwrap();
        let content = doc.page_content(1).unwrap();
        // An image is painted with `Do`; if present, our inflate decoded it.
        assert!(
            has_op(&content, b"Do") || has_op(&content, b"Tj"),
            "expected drawing operators in decoded content ({} bytes)",
            content.len()
        );
    }

    #[test]
    fn reports_object_count() {
        let doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        assert!(doc.object_count() >= 3, "a PDF has several objects");
    }

    #[test]
    fn save_roundtrips_through_our_own_reader() {
        // Open a real (object-stream) PDF, re-serialize it with our writer, and
        // confirm the output re-opens with pages and decodable content intact.
        let doc = Document::open(&fixture("with-images.pdf")).unwrap();
        let saved = doc.save();

        let reopened = Document::open(&saved).unwrap();
        assert!(!reopened.page_ids().unwrap().is_empty(), "pages survived save");
        let content = reopened.page_content(1).unwrap();
        assert!(
            has_op(&content, b"Do") || has_op(&content, b"Tj"),
            "content survived save ({} bytes)",
            content.len()
        );
    }

    #[test]
    fn edits_text_in_place_and_persists_through_save() {
        // The full Word-like cycle on our own engine: open a real PDF, edit a
        // text run, save with our serializer, reopen, confirm the new text.
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        assert!(!doc.page_text_runs(1).unwrap().is_empty());

        doc.replace_text_run(1, 0, "Edited by gigapdf-engine").unwrap();
        let saved = doc.save();

        let reopened = Document::open(&saved).unwrap();
        let runs = reopened.page_text_runs(1).unwrap();
        assert!(
            runs.iter().any(|r| r.text.contains("Edited by gigapdf-engine")),
            "edited text should survive the save; got {:?}",
            runs.iter().map(|r| r.text.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn adds_a_frame_and_persists_through_save() {
        use crate::content::ElementKind;
        let paths = |doc: &Document| {
            doc.page_elements(1)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == ElementKind::Path)
                .count()
        };

        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let before = paths(&doc);
        doc.add_rectangle(1, 50.0, 50.0, 200.0, 100.0, Some([0.0, 0.0, 0.0]), None, 1.5)
            .unwrap();

        let reopened = Document::open(&doc.save()).unwrap();
        assert_eq!(paths(&reopened), before + 1, "one frame added and persisted");
    }

    #[test]
    fn adds_lists_and_persists_annotations() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let before = doc.page_annotations(1).unwrap().len();

        doc.add_square_annotation(1, [50.0, 50.0, 250.0, 150.0], Some([1.0, 0.0, 0.0]), None, 2.0)
            .unwrap();
        doc.add_highlight(1, [60.0, 200.0, 260.0, 220.0], [1.0, 1.0, 0.0])
            .unwrap();
        doc.add_free_text(1, [50.0, 300.0, 300.0, 340.0], "Note", 14.0, [0.0, 0.0, 1.0])
            .unwrap();

        let annots = Document::open(&doc.save()).unwrap().page_annotations(1).unwrap();
        assert_eq!(annots.len(), before + 3, "three annotations persisted");
        assert!(annots.iter().any(|a| a.subtype == "Square"));
        assert!(annots.iter().any(|a| a.subtype == "Highlight"));
        assert!(annots
            .iter()
            .any(|a| a.subtype == "FreeText" && a.contents == "Note"));
    }

    #[test]
    fn rotates_a_page() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        doc.rotate_page(1, 90).unwrap();
        let reopened = Document::open(&doc.save()).unwrap();
        let rotate = reopened
            .page_dict(1)
            .unwrap()
            .get(b"Rotate")
            .and_then(|o| o.as_i64());
        assert_eq!(rotate, Some(90));
    }

    #[test]
    fn sets_and_reads_metadata() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        doc.set_metadata("Title", "My Title").unwrap();
        doc.set_metadata("Author", "Rony").unwrap();
        let reopened = Document::open(&doc.save()).unwrap();
        assert_eq!(reopened.get_metadata("Title"), Some("My Title".to_string()));
        assert_eq!(reopened.get_metadata("Author"), Some("Rony".to_string()));
    }

    #[test]
    fn deletes_a_page() {
        let mut doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        let before = doc.page_count();
        assert!(before > 1, "fixture should have several pages");
        doc.delete_page(1).unwrap();
        let reopened = Document::open(&doc.save()).unwrap();
        assert_eq!(reopened.page_count(), before - 1);
    }

    #[test]
    fn moves_a_page() {
        let mut doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        let ids = doc.page_ids().unwrap();
        assert!(ids.len() >= 2);
        let first = ids[0];

        doc.move_page(1, ids.len() as u32).unwrap();
        let reordered = doc.page_ids().unwrap();
        assert_eq!(reordered.len(), ids.len());
        assert_eq!(reordered.last().copied(), Some(first), "page 1 moved to last");

        assert_eq!(Document::open(&doc.save()).unwrap().page_count(), ids.len());
    }

    #[test]
    fn extracts_a_single_page() {
        let doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        assert!(doc.page_count() >= 2);
        let extracted = doc.extract_pages(&[1]).unwrap();
        let reopened = Document::open(&extracted).unwrap();
        assert_eq!(reopened.page_count(), 1, "extracted exactly one page");
    }

    #[test]
    fn merges_pages_from_another_pdf() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let before = doc.page_count();
        let other = fixture("multi-page.pdf");
        let other_count = Document::open(&other).unwrap().page_count();

        doc.append_pages_from(&other).unwrap();
        let reopened = Document::open(&doc.save()).unwrap();
        assert_eq!(reopened.page_count(), before + other_count, "pages merged");
    }

    #[test]
    fn lists_form_fields() {
        let doc = Document::open(&fixture("with-forms.pdf")).unwrap();
        let fields = doc.form_fields().unwrap();
        eprintln!(
            "with-forms.pdf -> {} field(s): {:?}",
            fields.len(),
            fields
                .iter()
                .map(|f| (f.name.clone(), f.field_type.clone(), f.value.clone()))
                .collect::<Vec<_>>()
        );
        assert!(fields
            .iter()
            .any(|f| f.name == "name" && f.field_type == "Tx" && f.value == "John Doe"));
        assert!(fields
            .iter()
            .any(|f| f.name == "country" && f.field_type == "Ch" && f.value == "France"));
    }

    #[test]
    fn classifies_every_field_kind() {
        use crate::form::FieldKind;
        let doc = Document::open(&fixture("with-forms.pdf")).unwrap();
        let fields = doc.form_fields().unwrap();
        for f in &fields {
            eprintln!(
                "  {:<10} type={} kind={:?} flags={:#06x} opts={:?}",
                f.name, f.field_type, f.kind(), f.flags, f.options
            );
        }
        let by = |n: &str| fields.iter().find(|f| f.name == n).unwrap();
        assert_eq!(by("name").kind(), FieldKind::Text);
        assert_eq!(by("country").kind(), FieldKind::ComboBox);
        // `agree` and `gender` are buttons; at least one must be a checkbox/radio.
        assert!(matches!(
            by("gender").kind(),
            FieldKind::Radio | FieldKind::Checkbox
        ));
    }

    #[test]
    fn fills_text_checkbox_radio_and_choice() {
        let mut doc = Document::open(&fixture("with-forms.pdf")).unwrap();

        doc.set_text_field("name", "Jane Smith").unwrap();
        doc.set_text_field("email", "jane@example.com").unwrap();
        doc.set_checkbox("agree", true).unwrap();
        doc.set_choice_field("country", &["Germany"]).unwrap();

        // `gender` is a radio group: pick whichever export option it offers.
        let gender = doc
            .form_fields()
            .unwrap()
            .into_iter()
            .find(|f| f.name == "gender")
            .unwrap();
        if gender.kind() == crate::form::FieldKind::Radio {
            let option = gender.options.first().cloned().unwrap();
            doc.set_radio("gender", &option).unwrap();
        }

        let reopened = Document::open(&doc.save()).unwrap();
        let fields = reopened.form_fields().unwrap();
        let value = |n: &str| fields.iter().find(|f| f.name == n).unwrap().value.clone();

        assert_eq!(value("name"), "Jane Smith");
        assert_eq!(value("email"), "jane@example.com");
        assert_eq!(value("agree"), "Yes");
        assert_eq!(value("country"), "Germany");
    }

    #[test]
    fn rejects_unknown_choice_option() {
        let mut doc = Document::open(&fixture("with-forms.pdf")).unwrap();
        // `country` is a non-editable combo, so an off-list value must fail.
        let result = doc.set_choice_field("country", &["Atlantis"]);
        assert!(result.is_err(), "off-list value on a closed combo must error");
    }

    #[test]
    fn adds_and_reads_hyperlinks() {
        let mut doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        doc.add_uri_link(1, [72.0, 700.0, 300.0, 720.0], "https://giga-pdf.com")
            .unwrap();
        doc.add_goto_link(1, [72.0, 650.0, 300.0, 670.0], 2).unwrap();

        let reopened = Document::open(&doc.save()).unwrap();
        let links = reopened.page_links(1).unwrap();
        assert_eq!(links.len(), 2, "two links round-tripped");
        assert!(
            links
                .iter()
                .any(|l| l.target == LinkTarget::Uri("https://giga-pdf.com".to_string())),
            "external URI link survived"
        );
        assert!(
            links.iter().any(|l| l.target == LinkTarget::Page(2)),
            "internal go-to-page link resolved to page 2 after renumbering"
        );
    }

    #[test]
    fn builds_and_reads_outline() {
        let mut doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        let toc: Vec<(String, Option<u32>, usize)> = vec![
            ("Chapter 1".to_string(), Some(1), 0),
            ("Section 1.1".to_string(), Some(1), 1),
            ("Section 1.2".to_string(), Some(2), 1),
            ("Chapter 2".to_string(), Some(3), 0),
        ];
        doc.set_outline(&toc).unwrap();

        let reopened = Document::open(&doc.save()).unwrap();
        let items = reopened.outline_items();
        assert_eq!(items.len(), 4, "all outline items flattened");
        assert_eq!(items[0].title, "Chapter 1");
        assert_eq!(items[0].level, 0);
        assert_eq!(items[0].page, Some(1));
        assert_eq!(items[1].title, "Section 1.1");
        assert_eq!(items[1].level, 1, "nested under Chapter 1");
        assert_eq!(items[3].title, "Chapter 2");
        assert_eq!(items[3].level, 0);
        assert_eq!(items[3].page, Some(3), "dest page resolved after renumbering");
    }

    #[test]
    fn clears_the_outline() {
        let mut doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        doc.set_outline(&[("Only".to_string(), Some(1), 0)]).unwrap();
        doc.set_outline(&[]).unwrap();
        let reopened = Document::open(&doc.save()).unwrap();
        assert!(reopened.outline_items().is_empty(), "outline cleared");
    }

    #[test]
    fn adds_text_markup_and_ink_and_stamp() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let red = [1.0, 0.0, 0.0];
        doc.add_underline(1, [72.0, 700.0, 300.0, 712.0], red).unwrap();
        doc.add_strike_out(1, [72.0, 680.0, 300.0, 692.0], red).unwrap();
        doc.add_ink(1, &[vec![(100.0, 100.0), (130.0, 140.0), (160.0, 110.0)]], [0.0, 0.0, 1.0], 2.0)
            .unwrap();
        doc.add_stamp(1, [400.0, 700.0, 520.0, 740.0], "DRAFT", red)
            .unwrap();

        let reopened = Document::open(&doc.save()).unwrap();
        let subtypes: Vec<String> = reopened
            .page_annotations(1)
            .unwrap()
            .into_iter()
            .map(|a| a.subtype)
            .collect();
        for expected in ["Underline", "StrikeOut", "Ink", "Stamp"] {
            assert!(
                subtypes.iter().any(|s| s == expected),
                "{expected} annotation present, got {subtypes:?}"
            );
        }
    }

    #[test]
    fn flattens_annotations_into_content() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        doc.add_highlight(1, [72.0, 700.0, 300.0, 712.0], [1.0, 1.0, 0.0]).unwrap();
        doc.add_free_text(1, [72.0, 650.0, 300.0, 680.0], "Note", 12.0, [0.0, 0.0, 0.0])
            .unwrap();

        let baked = doc.flatten_annotations(1).unwrap();
        assert_eq!(baked, 2, "both annotations baked");

        let reopened = Document::open(&doc.save()).unwrap();
        assert!(
            reopened.page_annotations(1).unwrap().is_empty(),
            "markup removed after flatten"
        );
        // The appearances are now XObject draws in the page content.
        let images = reopened
            .page_elements(1)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == content::ElementKind::Image)
            .count();
        assert!(images >= 2, "baked appearances drawn as XObjects ({images})");
    }

    #[test]
    fn signs_a_document() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let randomness: Vec<u8> = (0..256).map(|i| (i * 53 + 7) as u8).collect();
        let signer = crate::sign::Signer::generate(
            "GigaPDF Tester", "260614000000Z", "360614000000Z", 512, &randomness,
        )
        .unwrap();

        let signed = doc
            .sign(&signer, "GigaPDF Tester", "Approval", "D:20260614120000Z")
            .unwrap();

        assert_eq!(&signed[0..5], b"%PDF-", "valid PDF header");
        // The fixed-width /ByteRange placeholders were patched with real offsets.
        assert!(
            !signed.windows(10).any(|w| w == b"9999999999"),
            "ByteRange placeholders patched"
        );
        let text = String::from_utf8_lossy(&signed);
        assert!(text.contains("adbe.pkcs7.detached"), "detached signature subfilter");
        assert!(text.contains("/ByteRange"), "byte range present");
        // The signed file still parses as a structurally valid PDF.
        let reopened = Document::open(&signed).unwrap();
        assert!(reopened.page_count() >= 1, "signed PDF re-opens");
    }

    #[test]
    fn redaction_removes_content_from_the_stream() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let before = doc.page_text_runs(1).unwrap().len();
        assert!(before > 0, "fixture has text");

        // No cover: pure stream deletion so a complex background would survive.
        let removed = doc.redact_region(1, 0.0, 0.0, 612.0, 792.0, None).unwrap();
        assert!(removed > 0, "elements were removed");

        // After save + reopen, the redacted text is gone from the stream — not
        // merely covered. (A cosmetic overlay would leave the runs intact.)
        let reopened = Document::open(&doc.save()).unwrap();
        let after = reopened.page_text_runs(1).unwrap().len();
        assert!(after < before, "text runs removed ({before} → {after})");
    }

    #[test]
    fn encrypts_and_decrypts_round_trip() {
        let original = Document::open(&fixture("simple-text.pdf")).unwrap();
        let want: String = original
            .page_text_runs(1)
            .unwrap()
            .iter()
            .map(|r| r.text.clone())
            .collect();
        assert!(!want.is_empty());

        let encrypted = original.save_encrypted(b"s3cret", b"file-id-bytes-01", -44);

        // Opening with the right password recovers the exact text.
        let opened = Document::open_with_password(&encrypted, b"s3cret").unwrap();
        let got: String = opened
            .page_text_runs(1)
            .unwrap()
            .iter()
            .map(|r| r.text.clone())
            .collect();
        assert_eq!(got, want, "decrypted text matches original");

        // The wrong (empty) password is rejected at open time.
        assert!(
            Document::open(&encrypted).is_err(),
            "wrong password must be rejected"
        );
    }

    #[test]
    fn renders_a_page_to_png() {
        // Add a vector rectangle so there is guaranteed ink, then rasterize.
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        doc.add_rectangle(1, 50.0, 50.0, 200.0, 100.0, None, Some([1.0, 0.0, 0.0]), 0.0)
            .unwrap();
        let png = doc.render_page(1, 1.0).unwrap();
        assert_eq!(&png[0..4], &[0x89, b'P', b'N', b'G'], "valid PNG header");
        assert!(png.len() > 1000, "non-trivial PNG ({} bytes)", png.len());
    }

    #[test]
    fn renders_embedded_font_glyphs() {
        // embedded-fonts.pdf uses a DejaVu TTF subset — glyphs must paint ink,
        // which only happens if the /FontFile2 program is parsed and filled.
        let doc = Document::open(&fixture("embedded-fonts.pdf")).unwrap();
        let png = doc.render_page(1, 2.0).unwrap();
        // Decode the (stored) zlib IDAT and count non-white pixels.
        let idat = png.windows(4).position(|w| w == b"IDAT").unwrap();
        let len = u32::from_be_bytes([
            png[idat - 4],
            png[idat - 3],
            png[idat - 2],
            png[idat - 1],
        ]) as usize;
        let zlib = &png[idat + 4..idat + 4 + len];
        let raw = crate::filters::inflate::inflate(&zlib[2..zlib.len() - 4]).unwrap();
        let dark = raw.iter().filter(|&&b| b < 200).count();
        assert!(dark > 500, "embedded-font glyphs painted ink ({dark} dark samples)");
    }

    #[test]
    fn extracts_text_without_tofu() {
        // Embedded TTF subsets with custom encodings only extract cleanly when
        // the font's /ToUnicode CMap is honoured — otherwise it's all tofu.
        for fixture_name in ["embedded-fonts.pdf", "mixed-fonts.pdf", "simple-text.pdf"] {
            let doc = Document::open(&fixture(fixture_name)).unwrap();
            let runs = doc.page_text_runs(1).unwrap();
            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
            assert!(!text.is_empty(), "{fixture_name}: extracted some text");
            let tofu = text.chars().filter(|&c| c == '\u{FFFD}').count();
            assert_eq!(tofu, 0, "{fixture_name}: no replacement chars, got {text:?}");
        }
    }
}
