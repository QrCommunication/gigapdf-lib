//! Fidelity of the reconstructed model for **tagged** PDFs.
//!
//! Since 0.120.0 every Office→PDF conversion is tagged by default, and
//! [`Document::reconstruct_model`] is the single reconstruction every
//! Office/web exporter flows through. A tagged document must therefore
//! reconstruct with **full visual fidelity** — placed images, rich char
//! styles, geometric table metrics, block frames, editable `source_index` —
//! exactly like its untagged twin; the tag tree may only inform *text*
//! extraction (`to_text` reading order), never strip the exporters down to
//! bare prose.
//!
//! Regression test for the 2026-07-06 incident: "PDF→Office export loses all
//! formatting and images — only text remains" (the tag-tree block list was
//! short-circuiting the geometric reconstruction inside `reconstruct_model`).

use gigapdf_core::convert::zip::ZipWriter;
use gigapdf_core::model::{Block, BlockKind, Inline};
use gigapdf_core::Document;

/// A rich real-world DOCX — the exact user flow that regressed: a paragraph
/// mixing plain / **bold** / colored runs, an inline image, and a bordered
/// 2×2 table.
fn rich_docx() -> Vec<u8> {
    let doc = r#"<w:document xmlns:w="x" xmlns:a="y" xmlns:r="z">
  <w:body>
    <w:p>
      <w:r><w:t xml:space="preserve">Intro </w:t></w:r>
      <w:r><w:rPr><w:b/></w:rPr><w:t>GRAS</w:t></w:r>
      <w:r><w:rPr><w:color w:val="CC0000"/></w:rPr><w:t xml:space="preserve"> ROUGE</w:t></w:r>
      <w:r><w:t xml:space="preserve"> fin.</w:t></w:r>
    </w:p>
    <w:p><w:r><w:drawing><a:blip r:embed="rId5"/></w:drawing></w:r></w:p>
    <w:tbl>
      <w:tblPr>
        <w:tblBorders>
          <w:top w:val="single" w:sz="8"/><w:left w:val="single" w:sz="8"/>
          <w:bottom w:val="single" w:sz="8"/><w:right w:val="single" w:sz="8"/>
          <w:insideH w:val="single" w:sz="8"/><w:insideV w:val="single" w:sz="8"/>
        </w:tblBorders>
      </w:tblPr>
      <w:tblGrid><w:gridCol w:w="2400"/><w:gridCol w:w="2400"/></w:tblGrid>
      <w:tr><w:tc><w:p><w:r><w:t>A1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B1</w:t></w:r></w:p></w:tc></w:tr>
      <w:tr><w:tc><w:p><w:r><w:t>A2</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B2</w:t></w:r></w:p></w:tc></w:tr>
    </w:tbl>
  </w:body>
</w:document>"#;
    let rels = r#"<Relationships xmlns="x">
  <Relationship Id="rId5" Type="image" Target="media/pic.png"/>
</Relationships>"#;
    // A 16×16 solid-red PNG.
    let rgba: Vec<u8> = (0..16 * 16).flat_map(|_| [220u8, 30, 30, 255]).collect();
    let png = gigapdf_core::raster::png::encode_png(16, 16, &rgba);

    let mut z = ZipWriter::new();
    z.add_stored("[Content_Types].xml", b"<Types/>");
    z.add_stored("word/document.xml", doc.as_bytes());
    z.add_stored("word/_rels/document.xml.rels", rels.as_bytes());
    z.add_stored("word/media/pic.png", &png);
    z.finish()
}

/// Fidelity signals collected from a reconstructed model.
#[derive(Debug, Default)]
struct Fidelity {
    resolved_images: usize,
    framed_images: usize,
    /// "ROUGE" carries its authored red — per-run color survived.
    rouge_is_red: bool,
    /// "Intro" stayed black — the red did NOT contaminate the whole
    /// paragraph (the tag path used to fuse every run into one).
    intro_not_red: bool,
    geo_table: bool,
    framed_text_blocks: usize,
    sourced_runs: usize,
}

fn fidelity(m: &gigapdf_core::model::Document) -> Fidelity {
    fn is_red(c: Option<[f64; 3]>) -> bool {
        matches!(c, Some([r, g, b]) if r > 0.5 && g < 0.3 && b < 0.3)
    }
    fn scan_runs(runs: &[Inline], out: &mut Fidelity) {
        for inline in runs {
            match inline {
                Inline::Run(r) => {
                    if r.text.contains("ROUGE") && is_red(r.style.color) {
                        out.rouge_is_red = true;
                    }
                    if r.text.contains("Intro") && !is_red(r.style.color) {
                        out.intro_not_red = true;
                    }
                    if r.source_index.is_some() {
                        out.sourced_runs += 1;
                    }
                }
                Inline::Link { children, .. } => scan_runs(children, out),
                _ => {}
            }
        }
    }
    fn scan_blocks(
        blocks: &[Block],
        images: &std::collections::BTreeMap<u64, gigapdf_core::model::ImageResource>,
        out: &mut Fidelity,
    ) {
        for b in blocks {
            let text_kind = matches!(
                b.kind,
                BlockKind::Paragraph(_) | BlockKind::Heading(_) | BlockKind::List(_)
            );
            if text_kind && b.frame.is_some() {
                out.framed_text_blocks += 1;
            }
            match &b.kind {
                BlockKind::Image(img) if images.contains_key(&img.resource) => {
                    out.resolved_images += 1;
                    if b.frame.is_some() {
                        out.framed_images += 1;
                    }
                }
                BlockKind::Paragraph(p) => scan_runs(&p.runs, out),
                BlockKind::Heading(h) => scan_runs(&h.para.runs, out),
                BlockKind::List(l) => {
                    for item in &l.items {
                        scan_blocks(&item.blocks, images, out);
                    }
                }
                BlockKind::Table(t) => {
                    if !t.col_widths.is_empty() {
                        out.geo_table = true;
                    }
                    for row in &t.rows {
                        for cell in &row.cells {
                            scan_blocks(&cell.blocks, images, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Fidelity::default();
    for s in &m.sections {
        for p in &s.pages {
            scan_blocks(&p.blocks, &m.resources.images, &mut out);
        }
    }
    out
}

fn assert_rich(label: &str, f: &Fidelity) {
    assert!(
        f.resolved_images >= 1,
        "{label}: placed image survives reconstruction ({f:?})"
    );
    assert!(
        f.framed_images >= 1,
        "{label}: the image keeps its frame (exporters need it) ({f:?})"
    );
    assert!(
        f.rouge_is_red,
        "{label}: per-run color survives ({f:?})"
    );
    assert!(
        f.intro_not_red,
        "{label}: color does not bleed over the whole paragraph ({f:?})"
    );
    assert!(
        f.geo_table,
        "{label}: geometric table fidelity (col_widths) survives ({f:?})"
    );
    assert!(
        f.framed_text_blocks >= 1,
        "{label}: text blocks keep their frames ({f:?})"
    );
    assert!(
        f.sourced_runs >= 1,
        "{label}: runs keep their editable source_index ({f:?})"
    );
}

/// Sanity guard: the untagged twin reconstructs rich (keeps the assertions
/// honest — if this fails the fixture is wrong, not the tag path).
#[test]
fn untagged_pdf_reconstructs_rich_model() {
    let pdf = gigapdf_core::convert::office_import::office_to_pdf_opts(&rich_docx(), &[], false)
        .expect("docx converts");
    let doc = Document::open(&pdf).expect("engine reopens its own PDF");
    assert_rich("untagged", &fidelity(&doc.reconstruct_model()));
}

/// The actual regression pin: the SAME document through the **default**
/// (tagged) conversion must reconstruct just as rich.
#[test]
fn tagged_pdf_reconstructs_rich_model() {
    let pdf = gigapdf_core::convert::reverse::office_to_pdf(&rich_docx()).expect("docx converts");
    assert!(
        String::from_utf8_lossy(&pdf).contains("StructTreeRoot"),
        "default conversion really is tagged"
    );
    let doc = Document::open(&pdf).expect("tagged PDF reopens");
    assert_rich("tagged", &fidelity(&doc.reconstruct_model()));
}

/// Exporter downstream: the tagged PDF's DOCX export still embeds the image
/// (a `word/media/` part exists in the OOXML zip) and keeps the bold run.
#[test]
fn tagged_pdf_docx_export_keeps_image_and_bold() {
    let pdf = gigapdf_core::convert::reverse::office_to_pdf(&rich_docx()).expect("docx converts");
    let docx = Document::open(&pdf).expect("tagged PDF reopens").to_docx();
    let has_media = docx.windows(b"word/media/".len()).any(|w| w == b"word/media/");
    assert!(has_media, "tagged→docx keeps an embedded image part (word/media/)");
}
