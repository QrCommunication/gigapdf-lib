//! Structured importers: lower an input file into the unified editable
//! [`Document`](crate::model::Document) model.
//!
//! This is the model-producing counterpart of the `*_to_pdf` exporters: every
//! source format populates the format-neutral [`Document`] tree **directly**,
//! preserving structure (paragraphs, headings, lists, tables, typed spreadsheet
//! cells, slides) instead of flattening to HTML/PDF. The per-format walkers live
//! beside the rich HTML exporters in [`super::office_import`] (Office) and in
//! [`crate::html::model`] (HTML); this module is the small public dispatch
//! layer over them, plus the image and text/RTF entry points.
//!
//! These functions are additive — the existing `*_to_pdf` paths are unchanged
//! and remain the rendering fallback.

use crate::model::{
    Block, BlockKind, CharStyle, Document, ImageRef, ImageResource, Inline, InlineRun, Page,
    Paragraph, Rect, Section,
};
use std::collections::BTreeMap;

/// Auto-detect an Office container (OOXML/ODF, or legacy OLE2) and lower it to
/// the unified [`Document`] model, or `None` for an unrecognized archive. Mirror
/// of [`super::office_import::office_to_pdf`]'s magic-byte dispatch.
pub fn office_to_model(bytes: &[u8]) -> Option<Document> {
    super::office_import::office_to_model(bytes)
}

/// Parse an HTML string and lower it into the [`Document`] model: one section of
/// flow blocks (paragraphs, headings, lists, tables, inline runs/links/images).
/// CSS from `<style>` blocks is cascaded exactly as for the HTML→PDF renderer.
pub fn html_to_model(html: &str) -> Document {
    let nodes = crate::html::dom::parse(html);
    let sheet = crate::html::css::Stylesheet::new(&crate::html::css::collect_style_css(&nodes));
    crate::html::to_model(&nodes, &sheet)
}

/// Wrap a single raster image (PNG/JPEG/GIF/WebP) into a [`Document`]: one
/// section / one page holding a full-page [`Image`](BlockKind::Image) block,
/// sized to the image's own aspect ratio. The image header is decoded for its
/// pixel dimensions (no full decode); the bytes are stored in the document's
/// [`ResourceTable`](crate::model::ResourceTable). `None` if the bytes are not a
/// recognized image.
pub fn image_to_model(bytes: &[u8]) -> Option<Document> {
    let (w_px, h_px, format) = image_dimensions(bytes)?;
    let hash = fnv1a(bytes);

    // Fit the image inside an A4 portrait content area, preserving aspect.
    let geom = crate::model::PageGeometry::default();
    let avail_w = (geom.width - geom.margins.left - geom.margins.right).max(1.0);
    let avail_h = (geom.height - geom.margins.top - geom.margins.bottom).max(1.0);
    let (iw, ih) = (w_px.max(1) as f64, h_px.max(1) as f64);
    let scale = (avail_w / iw).min(avail_h / ih).min(1.0);
    let (draw_w, draw_h) = (iw * scale, ih * scale);
    let x = geom.margins.left + (avail_w - draw_w) / 2.0;
    let y = geom.margins.top + (avail_h - draw_h) / 2.0;

    let mut images = BTreeMap::new();
    images.insert(
        hash,
        ImageResource {
            bytes: bytes.to_vec(),
            format: format.to_string(),
        },
    );

    let block = Block {
        frame: Some(Rect::new(x, y, draw_w, draw_h)),
        kind: BlockKind::Image(ImageRef {
            resource: hash,
            alt: None,
        }),
        ..Block::default()
    };

    let mut doc = Document {
        sections: vec![Section {
            geometry: geom,
            header: None,
            footer: None,
            pages: vec![Page {
                blocks: vec![block],
                absolute: true,
            }],
        }],
        ..Document::default()
    };
    doc.resources.images = images;
    Some(doc)
}

/// Plain text → [`Document`]: one paragraph per line (blank lines kept as empty
/// paragraphs), in a single section/page of flow blocks.
pub fn txt_to_model(text: &str) -> Document {
    let blocks = text
        .lines()
        .map(|line| paragraph_block(line.trim_end()))
        .collect();
    flow_document(blocks)
}

/// RTF → [`Document`]: routed through the **rich** RTF parser
/// ([`super::rtf::rtf_to_model`]), which recovers run-level character styling
/// (bold/italic/underline/strike, colour, size, font family), tables, `\pict`
/// images (bytes interned into the resource table) and `\field` hyperlinks —
/// not just flat text.
pub fn rtf_to_model(rtf: &str) -> Document {
    super::rtf::rtf_to_model(rtf)
}

/// Markdown → [`Document`]: a CommonMark-ish parse producing real structure —
/// headings, paragraphs, lists, GFM tables, fenced code and inline emphasis/
/// links (see [`super::md_import`]).
pub fn md_to_model(md: &str) -> Document {
    super::md_import::md_to_model(md)
}

/// CSV → [`Document`]: an RFC 4180 parse (quoting, auto-detected delimiter)
/// lowered to a single editable table (see [`super::csv_import`]). `None` for
/// input with no parseable fields.
pub fn csv_to_model(bytes: &[u8]) -> Option<Document> {
    super::csv_import::csv_to_model(bytes)
}

/// A one-section, one-page [`Document`] of flow blocks (A4 default geometry).
fn flow_document(blocks: Vec<Block>) -> Document {
    Document {
        sections: vec![Section {
            geometry: crate::model::PageGeometry::default(),
            header: None,
            footer: None,
            pages: vec![Page {
                blocks,
                absolute: false,
            }],
        }],
        ..Document::default()
    }
}

/// A plain-text [`Paragraph`] block carrying one default-styled run (empty text
/// yields an empty paragraph, preserving blank-line spacing).
fn paragraph_block(text: &str) -> Block {
    let runs = if text.is_empty() {
        Vec::new()
    } else {
        vec![Inline::Run(InlineRun {
            text: text.to_string(),
            style: CharStyle::default(),
            source_index: None,
        })]
    };
    Block {
        kind: BlockKind::Paragraph(Paragraph {
            runs,
            ..Paragraph::default()
        }),
        ..Block::default()
    }
}

/// Read just the pixel dimensions (and a format tag) from a raster image's
/// header — PNG, JPEG, GIF, WebP (VP8/VP8L/VP8X), or AVIF — without decoding
/// pixels. Returns `(width, height, format)` or `None` for an unrecognized
/// container.
pub(crate) fn image_dimensions(b: &[u8]) -> Option<(u32, u32, &'static str)> {
    png_dims(b)
        .map(|(w, h)| (w, h, "png"))
        .or_else(|| jpeg_dims(b).map(|(w, h)| (w, h, "jpeg")))
        .or_else(|| gif_dims(b).map(|(w, h)| (w, h, "gif")))
        .or_else(|| webp_dims(b).map(|(w, h)| (w, h, "webp")))
        .or_else(|| avif_dims(b).map(|(w, h)| (w, h, "avif")))
}

/// PNG: signature + IHDR width/height (big-endian) from the first chunk.
fn png_dims(b: &[u8]) -> Option<(u32, u32)> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if b.get(..8)? != SIG {
        return None;
    }
    // IHDR is the first chunk: [len(4)][type(4)="IHDR"][width(4)][height(4)]…
    if b.get(12..16)? != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(b.get(16..20)?.try_into().ok()?);
    let h = u32::from_be_bytes(b.get(20..24)?.try_into().ok()?);
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// JPEG: scan segment markers for the first SOF (`0xC0..=0xCF`, excluding the
/// non-SOF markers C4/C8/CC) and read its 16-bit height then width.
fn jpeg_dims(b: &[u8]) -> Option<(u32, u32)> {
    if b.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2usize;
    while i + 1 < b.len() {
        // Markers are 0xFF followed by a non-0x00, non-0xFF marker byte.
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let mut marker = b[i + 1];
        // Skip fill bytes (0xFF runs).
        let mut j = i + 1;
        while marker == 0xFF && j + 1 < b.len() {
            j += 1;
            marker = b[j];
        }
        i = j + 1;
        // Standalone markers (no length): RSTn, SOI, EOI, TEM.
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            continue;
        }
        let len = u16::from_be_bytes(*b.get(i..i + 2)?.first_chunk::<2>()?) as usize;
        if len < 2 {
            return None;
        }
        let is_sof =
            (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC;
        if is_sof {
            // Segment: [len(2)][precision(1)][height(2)][width(2)]…
            let h = u16::from_be_bytes(*b.get(i + 3..i + 5)?.first_chunk::<2>()?) as u32;
            let w = u16::from_be_bytes(*b.get(i + 5..i + 7)?.first_chunk::<2>()?) as u32;
            if w == 0 || h == 0 {
                return None;
            }
            return Some((w, h));
        }
        i += len; // skip this segment's payload
    }
    None
}

/// GIF: `GIF87a`/`GIF89a` header → logical-screen width/height (little-endian).
fn gif_dims(b: &[u8]) -> Option<(u32, u32)> {
    let sig = b.get(..6)?;
    if sig != b"GIF87a" && sig != b"GIF89a" {
        return None;
    }
    let w = u16::from_le_bytes(*b.get(6..8)?.first_chunk::<2>()?) as u32;
    let h = u16::from_le_bytes(*b.get(8..10)?.first_chunk::<2>()?) as u32;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// WebP (RIFF): dispatch on the chunk fourCC — `VP8 ` (lossy), `VP8L`
/// (lossless), or `VP8X` (extended) — and read the canvas dimensions.
fn webp_dims(b: &[u8]) -> Option<(u32, u32)> {
    if b.get(..4)? != b"RIFF" || b.get(8..12)? != b"WEBP" {
        return None;
    }
    let fourcc = b.get(12..16)?;
    match fourcc {
        b"VP8 " => {
            // Lossy: after the 10-byte frame tag the keyframe start code
            // 0x9D 0x01 0x2A precedes 14-bit width/height (little-endian).
            let sc = b.get(23..26)?;
            if sc != [0x9D, 0x01, 0x2A] {
                return None;
            }
            let w = (u16::from_le_bytes(*b.get(26..28)?.first_chunk::<2>()?) & 0x3FFF) as u32;
            let h = (u16::from_le_bytes(*b.get(28..30)?.first_chunk::<2>()?) & 0x3FFF) as u32;
            (w != 0 && h != 0).then_some((w, h))
        }
        b"VP8L" => {
            // Lossless: 0x2F signature, then 14-bit (w-1) and (h-1) packed.
            if b.get(20)? != &0x2F {
                return None;
            }
            let bits = u32::from_le_bytes(*b.get(21..25)?.first_chunk::<4>()?);
            let w = (bits & 0x3FFF) + 1;
            let h = ((bits >> 14) & 0x3FFF) + 1;
            Some((w, h))
        }
        b"VP8X" => {
            // Extended: 24-bit (w-1) and (h-1), little-endian, at offset 24.
            let wb = b.get(24..27)?;
            let hb = b.get(27..30)?;
            let w = u32::from_le_bytes([wb[0], wb[1], wb[2], 0]) + 1;
            let h = u32::from_le_bytes([hb[0], hb[1], hb[2], 0]) + 1;
            Some((w, h))
        }
        _ => None,
    }
}

/// AVIF (ISOBMFF): confirm the `ftyp` brand is an AVIF/HEIF still, then read the
/// primary item's canvas size from the `meta → iprp → ipco → ispe` box. The
/// `ispe` ("image spatial extents") FullBox carries width/height as big-endian
/// `u32`s right after its 4-byte version/flags — a cheap header probe, no AV1
/// decode. Falls back to a full [`decode_avif`](crate::raster::avif::decode_avif)
/// only if the `ispe` walk fails (handles unusual box orderings).
fn avif_dims(b: &[u8]) -> Option<(u32, u32)> {
    use crate::raster::avif::find_box;

    // `ftyp` at offset 4, with an AVIF/HEIF-still brand (major or compatible).
    if b.len() < 16 || b.get(4..8)? != b"ftyp" {
        return None;
    }
    let (ftyp_s, ftyp_e) = find_box(b, 0, b.len(), b"ftyp")?;
    let mut is_avif = false;
    // Brands are 4-byte tags: major_brand at ftyp_s, then minor_version (4),
    // then the compatible_brands list to the end of the box.
    let mut o = ftyp_s;
    if let Some(major) = b.get(o..o + 4) {
        is_avif |= AVIF_BRANDS.contains(&major);
    }
    o += 8; // skip major_brand + minor_version
    while o + 4 <= ftyp_e {
        if let Some(brand) = b.get(o..o + 4) {
            is_avif |= AVIF_BRANDS.contains(&brand);
        }
        o += 4;
    }
    if !is_avif {
        return None;
    }

    // meta (FullBox: +4 version/flags) → iprp → ipco → ispe.
    let dims_from_ispe = (|| {
        let (meta_s, meta_e) = find_box(b, 0, b.len(), b"meta")?;
        let (iprp_s, iprp_e) = find_box(b, meta_s + 4, meta_e, b"iprp")?;
        let (ipco_s, ipco_e) = find_box(b, iprp_s, iprp_e, b"ipco")?;
        let (ispe_s, _) = find_box(b, ipco_s, ipco_e, b"ispe")?;
        // ispe is a FullBox: skip 4 bytes version/flags, then width(4)+height(4).
        let w = u32::from_be_bytes(b.get(ispe_s + 4..ispe_s + 8)?.try_into().ok()?);
        let h = u32::from_be_bytes(b.get(ispe_s + 8..ispe_s + 12)?.try_into().ok()?);
        (w != 0 && h != 0).then_some((w, h))
    })();

    dims_from_ispe.or_else(|| crate::raster::avif::decode_avif(b).map(|(w, h, _)| (w, h)))
}

/// `ftyp` brands that mark an AVIF / HEIF-still container we accept.
const AVIF_BRANDS: [&[u8]; 5] = [b"avif", b"avis", b"mif1", b"miaf", b"MA1B"];

/// 64-bit FNV-1a content hash — a stable, dependency-free resource key.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::zip::ZipWriter;
    use crate::model::{BlockKind, CellValue, DocMeta, PlaceholderRole};

    /// A tiny valid PNG (3×2 red) for the image-import fixture.
    fn red_png(w: u32, h: u32) -> Vec<u8> {
        let rgba = [255u8, 0, 0, 255].repeat((w * h) as usize);
        crate::raster::png::encode_png(w, h, &rgba)
    }

    fn blocks(doc: &Document) -> &[Block] {
        &doc.sections[0].pages[0].blocks
    }

    // ── DOCX → model (heading + paragraph + 2×2 table with a colspan) ──

    #[test]
    fn docx_model_heading_paragraph_and_spanning_table() {
        let doc_xml = r#"<w:document xmlns:w="x">
  <w:body>
    <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Title</w:t></w:r></w:p>
    <w:p><w:r><w:t>Body text</w:t></w:r></w:p>
    <w:tbl>
      <w:tr>
        <w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>Spanning</w:t></w:r></w:p></w:tc>
      </w:tr>
      <w:tr>
        <w:tc><w:p><w:r><w:t>R2C1</w:t></w:r></w:p></w:tc>
        <w:tc><w:p><w:r><w:t>R2C2</w:t></w:r></w:p></w:tc>
      </w:tr>
    </w:tbl>
  </w:body>
</w:document>"#;
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored("word/document.xml", doc_xml.as_bytes());
        let docx = z.finish();

        let doc = office_to_model(&docx).expect("docx → model");
        let b = blocks(&doc);

        // Block 0: a level-1 heading carrying "Title".
        match &b[0].kind {
            BlockKind::Heading(h) => {
                assert_eq!(h.level, 1);
                assert_eq!(para_text(&h.para), "Title");
            }
            other => panic!("expected heading, got {other:?}"),
        }
        // Block 1: a paragraph "Body text".
        match &b[1].kind {
            BlockKind::Paragraph(p) => assert_eq!(para_text(p), "Body text"),
            other => panic!("expected paragraph, got {other:?}"),
        }
        // Block 2: a 2-row table; first row's single cell spans 2 columns.
        match &b[2].kind {
            BlockKind::Table(t) => {
                assert_eq!(t.rows.len(), 2, "two rows");
                assert_eq!(t.rows[0].cells.len(), 1, "merged first row");
                assert_eq!(t.rows[0].cells[0].col_span, 2, "gridSpan=2");
                assert_eq!(t.rows[1].cells.len(), 2, "second row has two cells");
                assert_eq!(cell_text(&t.rows[1].cells[1]), "R2C2");
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    // ── XLSX → model (numeric cell + a merge) ──

    #[test]
    fn xlsx_model_sheet_number_and_merge() {
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored(
            "xl/workbook.xml",
            br#"<workbook><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        );
        z.add_stored(
            "xl/sharedStrings.xml",
            br#"<sst><si><t>Label</t></si></sst>"#,
        );
        z.add_stored(
            "xl/worksheets/sheet1.xml",
            br#"<worksheet>
              <mergeCells><mergeCell ref="A1:B1"/></mergeCells>
              <sheetData>
                <row r="1"><c r="A1" t="s"><v>0</v></c></row>
                <row r="2"><c r="A2"><v>42.5</v></c></row>
              </sheetData>
            </worksheet>"#,
        );
        let xlsx = z.finish();

        let doc = office_to_model(&xlsx).expect("xlsx → model");
        let sheet = match &blocks(&doc)[0].kind {
            BlockKind::Sheet(s) => &s.sheets[0],
            other => panic!("expected sheet block, got {other:?}"),
        };
        assert_eq!(sheet.name, "Data");
        // A2 is a typed number.
        assert_eq!(sheet.rows[1].cells[0].value, CellValue::Number(42.5));
        // A1 resolved its shared string.
        assert_eq!(
            sheet.rows[0].cells[0].value,
            CellValue::Text("Label".into())
        );
        // The A1:B1 merge survived.
        assert_eq!(sheet.merges.len(), 1);
        let m = sheet.merges[0];
        assert_eq!((m.r0, m.c0, m.r1, m.c1), (0, 0, 0, 1));
    }

    // ── PPTX → model (a text placeholder) ──

    #[test]
    fn pptx_model_slide_text_placeholder() {
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored("ppt/presentation.xml", b"<p:presentation/>");
        z.add_stored(
            "ppt/slides/slide1.xml",
            br#"<p:sld xmlns:a="y" xmlns:p="z">
              <p:cSld><p:spTree>
                <p:sp>
                  <p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
                  <p:txBody><a:p><a:r><a:t>Slide Title</a:t></a:r></a:p></p:txBody>
                </p:sp>
              </p:spTree></p:cSld>
            </p:sld>"#,
        );
        let pptx = z.finish();

        let doc = office_to_model(&pptx).expect("pptx → model");
        let slide = match &blocks(&doc)[0].kind {
            BlockKind::Slide(s) => &s.slides[0],
            other => panic!("expected slide block, got {other:?}"),
        };
        let ph = slide
            .placeholders
            .iter()
            .find(|p| matches!(p.role, PlaceholderRole::Title))
            .expect("a title placeholder");
        match &ph.block.kind {
            BlockKind::TextBox(tb) => {
                let text = tb
                    .blocks
                    .iter()
                    .filter_map(|b| match &b.kind {
                        BlockKind::Paragraph(p) => Some(para_text(p)),
                        _ => None,
                    })
                    .collect::<String>();
                assert_eq!(text, "Slide Title");
            }
            other => panic!("expected text box, got {other:?}"),
        }
    }

    // ── HTML → model ──

    #[test]
    fn html_model_heading_paragraph_list() {
        let doc = html_to_model("<h1>T</h1><p>body</p><ul><li>a</li><li>b</li></ul>");
        let b = blocks(&doc);
        assert_eq!(b.len(), 3);
        assert!(matches!(&b[0].kind, BlockKind::Heading(h) if h.level == 1));
        assert!(matches!(&b[1].kind, BlockKind::Paragraph(_)));
        match &b[2].kind {
            BlockKind::List(l) => assert_eq!(l.items.len(), 2),
            other => panic!("expected list, got {other:?}"),
        }
    }

    // ── image → model ──

    #[test]
    fn image_model_full_page_block() {
        let png = red_png(3, 2);
        let doc = image_to_model(&png).expect("png → model");
        let page = &doc.sections[0].pages[0];
        assert!(page.absolute, "image page is absolutely positioned");
        let b = &page.blocks[0];
        let frame = b.frame.expect("image block has a frame");
        assert!(frame.w > frame.h, "3×2 image keeps landscape aspect");
        let img = match &b.kind {
            BlockKind::Image(i) => i,
            other => panic!("expected image, got {other:?}"),
        };
        // The bytes are stored in the resource table under the block's key.
        assert!(doc.resources.images.contains_key(&img.resource));
        assert_eq!(doc.resources.images[&img.resource].format, "png");
        // Garbage is rejected.
        assert!(image_to_model(b"not an image").is_none());
    }

    #[test]
    fn image_dimensions_from_headers() {
        let png = red_png(7, 4);
        assert_eq!(image_dimensions(&png), Some((7, 4, "png")));
        // GIF89a 5×3 header (logical screen size little-endian).
        let gif = b"GIF89a\x05\x00\x03\x00\x00\x00\x00";
        assert_eq!(image_dimensions(gif), Some((5, 3, "gif")));
    }

    #[test]
    fn image_dimensions_probe_avif_ispe() {
        // The 32×32 AVIF still fixture: dimensions read from its `ispe` box
        // header (no AV1 decode), dispatched as the "avif" format.
        let avif = include_bytes!("../raster/fixtures/av1test.avif");
        assert_eq!(image_dimensions(avif), Some((32, 32, "avif")));
        // image_to_model wraps it into a full-page image document, storing the
        // AVIF bytes verbatim under the "avif" format tag.
        let doc = image_to_model(avif).expect("avif → model");
        let img_key = match &doc.sections[0].pages[0].blocks[0].kind {
            BlockKind::Image(i) => i.resource,
            other => panic!("expected image, got {other:?}"),
        };
        assert_eq!(doc.resources.images[&img_key].format, "avif");
    }

    // ── text / RTF → model ──

    #[test]
    fn txt_and_rtf_to_model() {
        let doc = txt_to_model("line one\nline two");
        let b = blocks(&doc);
        assert_eq!(para_text_at(b, 0), "line one");
        assert_eq!(para_text_at(b, 1), "line two");

        let rtf = r"{\rtf1\ansi Hello\par World\par}";
        let rdoc = rtf_to_model(rtf);
        let rb = blocks(&rdoc);
        let joined: String = rb
            .iter()
            .filter_map(|blk| match &blk.kind {
                BlockKind::Paragraph(p) => Some(para_text(p)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("Hello") && joined.contains("World"));
    }

    // ── document metadata → DocMeta (issue #29) ──

    /// A DOCX `docProps/core.xml` + `app.xml` populate the full model `DocMeta`:
    /// the core five (title/author/subject/keywords/lang) **and** the extended
    /// properties (description, created/modified dates, last-modified-by,
    /// revision, application, company).
    #[test]
    fn docx_model_reads_core_xml_metadata() {
        let core = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
 xmlns:dc="http://purl.org/dc/elements/1.1/"
 xmlns:dcterms="http://purl.org/dc/terms/"
 xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <dc:title>Quarterly Report</dc:title>
  <dc:creator>Ada Lovelace</dc:creator>
  <dc:subject>Finance</dc:subject>
  <dc:description>Q3 financial summary</dc:description>
  <cp:keywords>budget, q3, revenue</cp:keywords>
  <dc:language>en-GB</dc:language>
  <cp:lastModifiedBy>Charles Babbage</cp:lastModifiedBy>
  <cp:revision>4</cp:revision>
  <dcterms:created xsi:type="dcterms:W3CDTF">2020-01-01T09:00:00Z</dcterms:created>
  <dcterms:modified xsi:type="dcterms:W3CDTF">2020-06-15T14:30:00Z</dcterms:modified>
</cp:coreProperties>"#;
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored(
            "word/document.xml",
            br#"<w:document xmlns:w="x"><w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body></w:document>"#,
        );
        z.add_stored("docProps/core.xml", core.as_bytes());
        z.add_stored(
            "docProps/app.xml",
            br#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties">
              <Application>Acme Word</Application><Company>Analytical Engines Ltd</Company></Properties>"#,
        );
        let docx = z.finish();

        let doc = office_to_model(&docx).expect("docx → model");
        let m = &doc.meta;
        assert_eq!(m.title.as_deref(), Some("Quarterly Report"));
        assert_eq!(m.author.as_deref(), Some("Ada Lovelace"));
        assert_eq!(m.subject.as_deref(), Some("Finance"));
        assert_eq!(m.lang.as_deref(), Some("en-GB"));
        assert_eq!(m.keywords, vec!["budget", "q3", "revenue"]);
        assert_eq!(m.description, "Q3 financial summary");
        assert_eq!(m.created, "2020-01-01T09:00:00Z");
        assert_eq!(m.modified, "2020-06-15T14:30:00Z");
        assert_eq!(m.last_modified_by, "Charles Babbage");
        assert_eq!(m.revision, "4");
        assert_eq!(m.application, "Acme Word");
        assert_eq!(m.company, "Analytical Engines Ltd");
    }

    /// An ODT `meta.xml` populates the full model `DocMeta`: the Dublin-Core
    /// fields, each repeated `meta:keyword`, **and** the extended properties
    /// (description, creation date → created, `dc:date` → modified, generator,
    /// editing-cycles).
    #[test]
    fn odt_model_reads_meta_xml_metadata() {
        let meta = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0"
 xmlns:dc="http://purl.org/dc/elements/1.1/">
  <office:meta>
    <meta:generator>Acme Writer/1.0</meta:generator>
    <dc:title>Field Notes</dc:title>
    <dc:creator>Grace Hopper</dc:creator>
    <dc:subject>Research</dc:subject>
    <dc:description>Lab observations</dc:description>
    <dc:language>fr-FR</dc:language>
    <meta:keyword>alpha</meta:keyword>
    <meta:keyword>beta</meta:keyword>
    <meta:creation-date>2020-01-01T00:00:00</meta:creation-date>
    <dc:date>2020-03-04T11:22:33</dc:date>
    <meta:editing-cycles>7</meta:editing-cycles>
  </office:meta>
</office:document-meta>"#;
        let mut z = ZipWriter::new();
        // mimetype must be first/stored for ODF dispatch.
        z.add_stored("mimetype", b"application/vnd.oasis.opendocument.text");
        z.add_stored(
            "content.xml",
            br#"<office:document-content xmlns:office="o" xmlns:text="t">
              <office:body><office:text><text:p>Body</text:p></office:text></office:body>
            </office:document-content>"#,
        );
        z.add_stored("meta.xml", meta.as_bytes());
        let odt = z.finish();

        let doc = office_to_model(&odt).expect("odt → model");
        let m = &doc.meta;
        assert_eq!(m.title.as_deref(), Some("Field Notes"));
        assert_eq!(m.author.as_deref(), Some("Grace Hopper"));
        assert_eq!(m.subject.as_deref(), Some("Research"));
        assert_eq!(m.lang.as_deref(), Some("fr-FR"));
        assert_eq!(m.keywords, vec!["alpha", "beta"]);
        assert_eq!(m.description, "Lab observations");
        assert_eq!(m.created, "2020-01-01T00:00:00");
        assert_eq!(m.modified, "2020-03-04T11:22:33");
        assert_eq!(m.generator, "Acme Writer/1.0");
        assert_eq!(m.editing_cycles, "7");
    }

    /// No metadata part (or an empty one) ⇒ a default `DocMeta`, no panic — for
    /// both an OOXML and an ODF package.
    #[test]
    fn office_model_missing_metadata_yields_empty_docmeta() {
        // DOCX with no docProps/core.xml at all.
        let mut z = ZipWriter::new();
        z.add_stored("[Content_Types].xml", b"<Types/>");
        z.add_stored(
            "word/document.xml",
            br#"<w:document xmlns:w="x"><w:body><w:p><w:r><w:t>Hi</w:t></w:r></w:p></w:body></w:document>"#,
        );
        let docx = z.finish();
        let doc = office_to_model(&docx).expect("docx → model");
        assert_eq!(doc.meta, DocMeta::default());

        // ODT whose meta.xml carries no DocMeta-mapped field at all (only an
        // unrelated statistic element) ⇒ default DocMeta.
        let mut z = ZipWriter::new();
        z.add_stored("mimetype", b"application/vnd.oasis.opendocument.text");
        z.add_stored(
            "content.xml",
            br#"<office:document-content xmlns:office="o" xmlns:text="t">
              <office:body><office:text><text:p>Hi</text:p></office:text></office:body>
            </office:document-content>"#,
        );
        z.add_stored(
            "meta.xml",
            br#"<office:document-meta xmlns:office="o" xmlns:meta="m">
              <office:meta><meta:document-statistic meta:page-count="1"/></office:meta>
            </office:document-meta>"#,
        );
        let odt = z.finish();
        let doc = office_to_model(&odt).expect("odt → model");
        assert_eq!(doc.meta, DocMeta::default());
    }

    // ── helpers ──

    fn para_text(p: &Paragraph) -> String {
        let mut s = String::new();
        for inline in &p.runs {
            match inline {
                Inline::Run(r) => s.push_str(&r.text),
                Inline::Link { children, .. } => {
                    for c in children {
                        if let Inline::Run(r) = c {
                            s.push_str(&r.text);
                        }
                    }
                }
                _ => {}
            }
        }
        s.trim().to_string()
    }

    fn para_text_at(b: &[Block], i: usize) -> String {
        match &b[i].kind {
            BlockKind::Paragraph(p) => para_text(p),
            other => panic!("block {i} is not a paragraph: {other:?}"),
        }
    }

    fn cell_text(c: &crate::model::Cell) -> String {
        c.blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BlockKind::Paragraph(p) => Some(para_text(p)),
                _ => None,
            })
            .collect::<String>()
    }
}
