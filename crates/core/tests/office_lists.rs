//! List fidelity across the Office conversion pipeline.
//!
//! Reproduces the user-visible bug "bullet lists from a Word document render
//! without their bullets" and pins the whole list contract:
//!
//!  * docx → PDF: bullet (•) and decimal ("1.") markers are present in the
//!    extracted text AND painted on the raster (pixels in the marker gutter);
//!  * docx → model: `w:numPr` + `numbering.xml` lower to `List` blocks with the
//!    right `ListMarker`, level and ordered flag;
//!  * model → PDF: `pdf_from_model` paints the markers;
//!  * model → docx/odt/html/md round-trips keep marker type + nesting level.


use gigapdf_core::convert::zip::ZipWriter;
use gigapdf_core::Document;

// ─── fixtures ────────────────────────────────────────────────────────────────

/// `word/numbering.xml` defining two abstract numberings:
///  * `numId=1` → bullet list (level 0 "•" via Symbol \u{F0B7}, level 1 "o").
///  * `numId=2` → decimal "%1." at level 0, lowerLetter "%2)" at level 1.
///
/// The level-0 bullet uses the *Word default*: `w:lvlText w:val=""` (U+F0B7,
/// Private Use Area, Symbol font) — exactly what Word writes for its stock
/// "List Paragraph" bullets, so the fixture reproduces real-world documents.
fn numbering_xml() -> &'static str {
    r#"<?xml version="1.0"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:abstractNum w:abstractNumId="10">
    <w:lvl w:ilvl="0">
      <w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="&#xF0B7;"/>
      <w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr>
    </w:lvl>
    <w:lvl w:ilvl="1">
      <w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="o"/>
      <w:rPr><w:rFonts w:ascii="Courier New" w:hAnsi="Courier New"/></w:rPr>
    </w:lvl>
  </w:abstractNum>
  <w:abstractNum w:abstractNumId="20">
    <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
    <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2)"/></w:lvl>
  </w:abstractNum>
  <w:num w:numId="1"><w:abstractNumId w:val="10"/></w:num>
  <w:num w:numId="2"><w:abstractNumId w:val="20"/></w:num>
</w:numbering>"#
}

/// One `w:p` list item at `level` under `num_id`, with plain text `text`.
fn list_p(num_id: u32, level: u32, text: &str) -> String {
    format!(
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="{num_id}"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
    )
}

/// A .docx with a bullet list (2 levels) and a decimal list (2 levels), plus a
/// plain intro paragraph. This is the user-bug repro fixture.
fn docx_with_lists() -> Vec<u8> {
    let body = format!(
        "{intro}{b1}{b2}{b1b}{d1}{d2}{d1b}",
        intro = r#"<w:p><w:r><w:t>Intro paragraph</w:t></w:r></w:p>"#,
        b1 = list_p(1, 0, "Bullet one"),
        b2 = list_p(1, 1, "Bullet nested"),
        b1b = list_p(1, 0, "Bullet two"),
        d1 = list_p(2, 0, "Number one"),
        d2 = list_p(2, 1, "Number nested"),
        d1b = list_p(2, 0, "Number two"),
    );
    let document = format!(
        r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
    );
    let mut z = ZipWriter::new();
    z.add_stored("[Content_Types].xml", b"<Types/>");
    z.add_stored("word/document.xml", document.as_bytes());
    z.add_stored("word/numbering.xml", numbering_xml().as_bytes());
    z.finish()
}

// ─── raster helpers ──────────────────────────────────────────────────────────

/// Count "ink" pixels (any channel meaningfully darker than white) inside the
/// horizontal band `x0..x1` (fractions of the page width) over the full height.
fn ink_in_band(png: &[u8], x0: f64, x1: f64) -> usize {
    let img = gigapdf_core::raster::png_decode::decode_png(png).expect("engine PNG decodes");
    let (w, h, rgba) = (img.width, img.height, img.rgba);
    let (lo, hi) = (
        (w as f64 * x0).floor() as u32,
        (w as f64 * x1).ceil() as u32,
    );
    let mut n = 0usize;
    for y in 0..h {
        for x in lo..hi.min(w) {
            let i = ((y * w + x) * 4) as usize;
            let (r, g, b, a) = (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]);
            if a > 0 && (r < 200 || g < 200 || b < 200) {
                n += 1;
            }
        }
    }
    n
}

// ─── docx → PDF (the user bug) ───────────────────────────────────────────────

/// The end-to-end user scenario: a Word file with bullet + numbered lists is
/// converted to PDF. The markers must be in the text layer (•, o, 1., a), 2.)
/// AND painted on the page raster.
#[test]
fn docx_lists_convert_with_visible_markers() {
    let pdf = gigapdf_core::convert::reverse::office_to_pdf(&docx_with_lists())
        .expect("docx converts to PDF");
    let doc = Document::open(&pdf).expect("engine reopens its own PDF");
    // `to_text` breaks lines per run; fold to spaces for phrase asserts.
    let text = doc.to_text().replace('\n', " ");

    // Bullet markers: the Word Symbol-font F0B7 maps to a real • (U+2022).
    assert!(
        text.contains('\u{2022}'),
        "bullet marker missing from extracted text: {text:?}"
    );
    assert!(
        !text.contains('\u{F0B7}'),
        "PUA bullet leaked un-mapped into the PDF: {text:?}"
    );
    // Numbered markers with their numbering.xml suffixes.
    for needle in ["1.", "2.", "a)"] {
        assert!(
            text.contains(needle),
            "numbered marker {needle:?} missing from extracted text: {text:?}"
        );
    }
    // Item text is intact.
    for needle in ["Bullet one", "Bullet nested", "Number two"] {
        assert!(text.contains(needle), "item text {needle:?} lost: {text:?}");
    }

    // Raster proof: the marker gutter (left ~18% of the page: margin + marker
    // indent) carries ink. Without markers the items' text starts further
    // right and this band is blank.
    let png = doc.render_page(1, 1.0).expect("page rasterizes");
    let gutter = ink_in_band(&png, 0.06, 0.18);
    assert!(
        gutter > 30,
        "expected list-marker ink in the left gutter, found {gutter} px"
    );
}

// ─── docx → model ────────────────────────────────────────────────────────────

/// `w:numPr` paragraphs lower to model `List` blocks: bullet lists unordered
/// with a real '•' marker, decimal lists ordered, and `w:ilvl` preserved.
#[test]
fn docx_lists_lower_to_model() {
    use gigapdf_core::model::{BlockKind, ListMarker};

    let zip = gigapdf_core::convert::zip::read_zip(&docx_with_lists());
    let model = gigapdf_core::convert::office_import::docx_to_model(&zip);
    let lists: Vec<_> = model
        .sections
        .iter()
        .flat_map(|s| s.pages.iter())
        .flat_map(|p| p.blocks.iter())
        .filter_map(|b| match &b.kind {
            BlockKind::List(l) => Some(l),
            _ => None,
        })
        .collect();
    assert_eq!(lists.len(), 6, "one single-item List per w:numPr paragraph");

    // Bullet items: unordered, the *real* per-level characters from
    // numbering.xml — Symbol F0B7 → `•` at level 0, the literal Courier "o"
    // at level 1 (Word's stock second-level bullet) — and `w:ilvl` preserved.
    for (i, lvl, marker) in [(0usize, 0u8, '\u{2022}'), (1, 1, 'o'), (2, 0, '\u{2022}')] {
        assert!(!lists[i].ordered, "bullet list item {i} must be unordered");
        assert!(
            matches!(lists[i].marker, ListMarker::Bullet(c) if c == marker),
            "bullet item {i} marker: {:?} (want {marker:?})",
            lists[i].marker
        );
        assert_eq!(lists[i].items[0].level, lvl, "bullet item {i} level");
    }
    // Numbered items: ordered, Decimal at level 0, LowerAlpha at level 1; the
    // "%2)" template's `)` suffix survives; each one-item list pins its real
    // running ordinal in `start` (1, then a fresh sub-level 1, then 2).
    assert!(lists[3].ordered && matches!(lists[3].marker, ListMarker::Decimal));
    assert_eq!(lists[3].marker_suffix, None, "'%1.' keeps the default '.'");
    assert_eq!(lists[3].start, 1);
    assert!(lists[4].ordered && matches!(lists[4].marker, ListMarker::LowerAlpha));
    assert_eq!(lists[4].items[0].level, 1);
    assert_eq!(lists[4].marker_suffix, Some(')'), "'%2)' carries its suffix");
    assert_eq!(lists[4].start, 1);
    assert!(lists[5].ordered && matches!(lists[5].marker, ListMarker::Decimal));
    assert_eq!(lists[5].start, 2, "second level-0 numbered paragraph is 2.");
}

// ─── model → PDF ─────────────────────────────────────────────────────────────

/// A model with a bullet + numbered list prints its markers into the PDF text.
#[test]
fn model_lists_render_markers_to_pdf() {
    use gigapdf_core::model::{
        Block, BlockKind, CharStyle, Document as Model, Inline, InlineRun, List, ListItem, Page,
        PageGeometry, Paragraph, Section,
    };

    fn item(text: &str, level: u8) -> ListItem {
        ListItem {
            level,
            label: None,
            blocks: vec![Block {
                kind: BlockKind::Paragraph(Paragraph {
                    runs: vec![Inline::Run(InlineRun {
                        text: text.to_string(),
                        style: CharStyle::default(),
                        ..Default::default()
                    })],
                    ..Default::default()
                }),
                ..Default::default()
            }],
        }
    }

    let mut model = Model::default();
    model.sections.push(Section {
        geometry: PageGeometry::default(),
        pages: vec![Page {
            blocks: vec![
                Block {
                    kind: BlockKind::List(List {
                        ordered: false,
                        marker: gigapdf_core::model::ListMarker::Bullet('\u{2022}'),
                        items: vec![item("Alpha", 0), item("Beta", 1)],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Block {
                    kind: BlockKind::List(List {
                        ordered: true,
                        marker: gigapdf_core::model::ListMarker::Decimal,
                        items: vec![item("First", 0), item("Second", 0)],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    });

    let pdf = gigapdf_core::convert::project::pdf_from_model(&model);
    let doc = Document::open(&pdf).expect("model PDF reopens");
    let text = doc.to_text();
    assert!(text.contains('\u{2022}'), "bullet marker painted: {text:?}");
    assert!(text.contains("1."), "decimal marker painted: {text:?}");
    assert!(text.contains("2."), "decimal ordinal advances: {text:?}");
}

// ─── odt → model / PDF ───────────────────────────────────────────────────────

/// An .odt with a two-level bullet list (declared `text:bullet-char`s) and a
/// numbered list whose level 2 uses `style:num-suffix=")"`.
fn odt_with_lists() -> Vec<u8> {
    let content = r#"<?xml version="1.0"?>
<office:document-content xmlns:office="o" xmlns:text="t" xmlns:style="s" xmlns:fo="f">
  <office:automatic-styles>
    <text:list-style style:name="LB">
      <text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/>
      <text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/>
    </text:list-style>
    <text:list-style style:name="LN">
      <text:list-level-style-number text:level="1" style:num-format="1" style:num-suffix="."/>
      <text:list-level-style-number text:level="2" style:num-format="a" style:num-suffix=")"/>
    </text:list-style>
  </office:automatic-styles>
  <office:body><office:text>
    <text:list text:style-name="LB">
      <text:list-item><text:p>alpha</text:p>
        <text:list><text:list-item><text:p>nested</text:p></text:list-item></text:list>
      </text:list-item>
      <text:list-item><text:p>beta</text:p></text:list-item>
    </text:list>
    <text:list text:style-name="LN">
      <text:list-item><text:p>one</text:p>
        <text:list><text:list-item><text:p>deep</text:p></text:list-item></text:list>
      </text:list-item>
      <text:list-item><text:p>two</text:p></text:list-item>
    </text:list>
  </office:text></office:body>
</office:document-content>"#;
    let mut z = ZipWriter::new();
    z.add_stored("mimetype", b"application/vnd.oasis.opendocument.text");
    z.add_stored("content.xml", content.as_bytes());
    z.finish()
}

/// Collect every `List` block of a model, in document order.
fn model_lists(
    model: &gigapdf_core::model::Document,
) -> Vec<&gigapdf_core::model::List> {
    use gigapdf_core::model::BlockKind;
    model
        .sections
        .iter()
        .flat_map(|s| s.pages.iter())
        .flat_map(|p| p.blocks.iter())
        .filter_map(|b| match &b.kind {
            BlockKind::List(l) => Some(l),
            _ => None,
        })
        .collect()
}

/// `text:list` + `text:list-style` lower with real bullet chars per level,
/// number formats + suffixes, running ordinals in `start`, and nesting levels.
#[test]
fn odt_lists_lower_to_model() {
    use gigapdf_core::model::ListMarker;

    let model =
        gigapdf_core::convert::office_to_model(&odt_with_lists()).expect("odt lowers to a model");
    let lists = model_lists(&model);
    assert_eq!(lists.len(), 6, "one single-item List per list paragraph");

    // Bullets: the declared per-level chars (• then ◦), levels 0/1/0.
    assert!(matches!(lists[0].marker, ListMarker::Bullet('\u{2022}')));
    assert_eq!(lists[0].items[0].level, 0);
    assert!(matches!(lists[1].marker, ListMarker::Bullet('\u{25E6}')));
    assert_eq!(lists[1].items[0].level, 1);
    assert!(matches!(lists[2].marker, ListMarker::Bullet('\u{2022}')));

    // Numbered: decimal "." at level 0, lowerLetter ")" at level 1; the
    // second level-0 item pins ordinal 2.
    assert!(lists[3].ordered && matches!(lists[3].marker, ListMarker::Decimal));
    assert_eq!((lists[3].start, lists[3].marker_suffix), (1, None));
    assert!(lists[4].ordered && matches!(lists[4].marker, ListMarker::LowerAlpha));
    assert_eq!(lists[4].marker_suffix, Some(')'), "num-suffix carried");
    assert_eq!(lists[4].items[0].level, 1);
    assert!(lists[5].ordered && matches!(lists[5].marker, ListMarker::Decimal));
    assert_eq!(lists[5].start, 2, "second numbered item keeps its ordinal");
}

/// The direct odt → PDF path paints the real per-level markers: `•`/`◦`
/// bullets, and `1.`/`a)`/`2.` ordinals (per-item, restarting sub-levels).
#[test]
fn odt_lists_convert_with_visible_markers() {
    let pdf = gigapdf_core::convert::reverse::office_to_pdf(&odt_with_lists())
        .expect("odt converts to PDF");
    let doc = Document::open(&pdf).expect("engine reopens its own PDF");
    let text = doc.to_text().replace('\n', " ");
    for needle in ["\u{2022} alpha", "\u{25E6} nested", "\u{2022} beta", "1. one", "a) deep", "2. two"] {
        assert!(text.contains(needle), "missing {needle:?} in: {text:?}");
    }
}

// ─── model round-trips (docx / odt / html / md) ──────────────────────────────

/// A reference model: one bullet list (levels 0/1/0) + one `4)`-seeded decimal
/// list, exercised through every exporter that must keep type + level.
fn reference_model() -> gigapdf_core::model::Document {
    // Build via Markdown (simplest structured source), then adjust start/suffix.
    let mut model =
        gigapdf_core::convert::md_to_model("- alpha\n  - nested\n- beta\n\n1. one\n2. two\n");
    for section in &mut model.sections {
        for page in &mut section.pages {
            for block in &mut page.blocks {
                if let gigapdf_core::model::BlockKind::List(l) = &mut block.kind {
                    if l.ordered {
                        l.start = 4;
                        l.marker_suffix = Some(')');
                    }
                }
            }
        }
    }
    model
}

/// model → docx → model: ordered flag, nesting level, `)` suffix and the
/// start ordinal all survive the OOXML round-trip.
#[test]
fn model_docx_roundtrip_keeps_list_shape() {
    let model = reference_model();
    let docx = gigapdf_core::convert::export_model::docx_from_model(&model);
    let back = gigapdf_core::convert::office_to_model(&docx).expect("docx re-imports");
    let lists = model_lists(&back);
    let bullets: Vec<_> = lists.iter().filter(|l| !l.ordered).collect();
    let numbered: Vec<_> = lists.iter().filter(|l| l.ordered).collect();
    assert_eq!(
        bullets.iter().flat_map(|l| l.items.iter().map(|i| i.level)).collect::<Vec<_>>(),
        vec![0, 1, 0],
        "bullet nesting levels survive"
    );
    assert_eq!(numbered.len(), 2, "two numbered paragraphs");
    for l in &numbered {
        assert_eq!(l.marker_suffix, Some(')'), "'%1)' template round-trips");
    }
    assert_eq!(
        numbered.iter().map(|l| l.start).collect::<Vec<_>>(),
        vec![4, 5],
        "start=4 seeds the ordinals"
    );
}

/// model → odt → model: same contract through OpenDocument.
#[test]
fn model_odt_roundtrip_keeps_list_shape() {
    let model = reference_model();
    let odt = gigapdf_core::convert::export_model::odt_from_model(&model);
    let back = gigapdf_core::convert::office_to_model(&odt).expect("odt re-imports");
    let lists = model_lists(&back);
    let bullets: Vec<_> = lists.iter().filter(|l| !l.ordered).collect();
    let numbered: Vec<_> = lists.iter().filter(|l| l.ordered).collect();
    assert_eq!(
        bullets.iter().flat_map(|l| l.items.iter().map(|i| i.level)).collect::<Vec<_>>(),
        vec![0, 1, 0],
        "bullet nesting levels survive"
    );
    for l in &numbered {
        assert_eq!(l.marker_suffix, Some(')'), "style:num-suffix round-trips");
    }
}

/// model → html: nesting levels become real nested <ul>/<ol> sub-lists.
#[test]
fn model_html_export_nests_levels() {
    let html = gigapdf_core::convert::web::html_from_model(&reference_model());
    let ul = html.find("<ul>").expect("outer <ul>");
    let nested = html[ul + 4..].find("<ul>").expect("nested <ul> for the level-1 item");
    let close = html[ul + 4..].find("</ul>").expect("some </ul>");
    assert!(
        nested < close,
        "the nested <ul> opens inside the outer list: {html}"
    );
    assert!(html.contains("start=\"4\""), "ol start survives: {html}");
}

/// model → markdown: level-1 items indent, ordinals honour start + `)` suffix.
#[test]
fn model_md_export_keeps_levels_and_ordinals() {
    let md = gigapdf_core::convert::export_model::markdown_from_model(&reference_model());
    assert!(md.contains("- alpha\n    - nested\n- beta"), "nesting indents: {md:?}");
    assert!(md.contains("4) one"), "start + ')' suffix: {md:?}");
    assert!(md.contains("5) two"), "ordinal advances: {md:?}");
}

/// The additive `marker_suffix` field survives the model JSON round-trip and
/// stays absent (retro-compatible) when unset.
#[test]
fn marker_suffix_json_roundtrip() {
    let model = reference_model();
    let json = model.to_json();
    assert!(json.contains("\"marker_suffix\":\")\""), "suffix serialized");
    let back = gigapdf_core::model::Document::from_json(&json).expect("model JSON re-parses");
    let lists = model_lists(&back);
    let ordered: Vec<_> = lists.iter().filter(|l| l.ordered).collect();
    assert!(ordered.iter().all(|l| l.marker_suffix == Some(')')));
    let bullets: Vec<_> = lists.iter().filter(|l| !l.ordered).collect();
    assert!(bullets.iter().all(|l| l.marker_suffix.is_none()));
    // A list without a custom suffix serializes without the key at all.
    let plain = gigapdf_core::convert::md_to_model("1. a\n");
    let plain_json = plain.to_json();
    assert!(!plain_json.contains("marker_suffix"), "{plain_json}");
}
