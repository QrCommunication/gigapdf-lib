//! Per-format XML builders for the editable Office exporters.
//!
//! Each `to_*` takes already-normalized [`ConvPage`]s (top-down points) and
//! returns a complete `.odt`/`.docx`/`.pptx` byte stream via the [`super::zip`]
//! container. Content is **native and editable**: a PDF show-text run becomes a
//! placed text box, an image XObject a placed picture, a vector path a placed
//! rectangle — never a flattened page raster.

use super::zip::ZipWriter;
use super::style::{Generic, TextStyle};
use super::{ConvPage, PlacedImage};
use crate::content::num;

/// DOCX `<w:rPr>` run properties (fonts/bold/italic/colour/size) for a style.
fn docx_run_props(style: &TextStyle, half_pt: i64) -> String {
    let mut p = String::from("<w:rPr>");
    if !style.family.is_empty() {
        let mut fam = String::new();
        esc(&style.family, &mut fam);
        p.push_str(&format!("<w:rFonts w:ascii=\"{fam}\" w:hAnsi=\"{fam}\" w:cs=\"{fam}\"/>"));
    }
    if style.bold {
        p.push_str("<w:b/>");
    }
    if style.italic {
        p.push_str("<w:i/>");
    }
    if style.has_visible_color() {
        p.push_str(&format!("<w:color w:val=\"{}\"/>", style.hex_color()));
    }
    p.push_str(&format!("<w:sz w:val=\"{half_pt}\"/></w:rPr>"));
    p
}

/// ODF `<style:text-properties .../>` for a run style at `size_pt`.
fn odt_text_props(style: &TextStyle, size_pt: f64) -> String {
    let generic = match style.generic {
        Generic::Sans => "swiss",
        Generic::Serif => "roman",
        Generic::Mono => "modern",
    };
    let mut p = format!("<style:text-properties fo:font-size=\"{}pt\"", num(size_pt));
    if !style.family.is_empty() {
        let mut fam = String::new();
        esc(&style.family, &mut fam);
        p.push_str(&format!(
            " fo:font-family=\"{fam}\" style:font-family-generic=\"{generic}\""
        ));
    }
    if style.bold {
        p.push_str(" fo:font-weight=\"bold\"");
    }
    if style.italic {
        p.push_str(" fo:font-style=\"italic\"");
    }
    if style.has_visible_color() {
        p.push_str(&format!(" fo:color=\"#{}\"", style.hex_color()));
    }
    p.push_str("/>");
    p
}

/// PPTX `<a:rPr ...>...</a:rPr>` run properties at `sz` (hundredths of a point).
fn pptx_run_props(style: &TextStyle, sz: i64) -> String {
    let mut attrs = format!("lang=\"en-US\" sz=\"{sz}\"");
    if style.bold {
        attrs.push_str(" b=\"1\"");
    }
    if style.italic {
        attrs.push_str(" i=\"1\"");
    }
    let mut inner = String::new();
    if style.has_visible_color() {
        inner.push_str(&format!(
            "<a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill>",
            style.hex_color()
        ));
    }
    if !style.family.is_empty() {
        let mut fam = String::new();
        esc(&style.family, &mut fam);
        inner.push_str(&format!("<a:latin typeface=\"{fam}\"/>"));
    }
    if inner.is_empty() {
        format!("<a:rPr {attrs}/>")
    } else {
        format!("<a:rPr {attrs}>{inner}</a:rPr>")
    }
}

/// Append `text` with XML metacharacters escaped.
fn esc(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 forbids most C0 controls; drop them rather than emit
            // an invalid document.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
}

// ─────────────────────────────── ODT (ODF text) ───────────────────────────────

/// Export pages to an OpenDocument Text (`.odt`) document.
pub fn to_odt(pages: &[ConvPage]) -> Vec<u8> {
    let (pw, ph) = pages.first().map(|p| (p.width, p.height)).unwrap_or((612.0, 792.0));
    let mut zip = ZipWriter::new();

    // The mimetype entry must be first and stored uncompressed (ODF §3.3).
    zip.add_stored("mimetype", b"application/vnd.oasis.opendocument.text");

    let mut images: Vec<&PlacedImage> = Vec::new();
    let content = odt_content_xml(pages, &mut images);
    zip.add_deflated("content.xml", content.as_bytes());
    zip.add_deflated("styles.xml", odt_styles_xml(pw, ph).as_bytes());
    zip.add_deflated("META-INF/manifest.xml", odt_manifest_xml(images.len()).as_bytes());
    for (i, img) in images.iter().enumerate() {
        zip.add_deflated(&format!("Pictures/img{}.png", i + 1), &img.png);
    }
    zip.finish()
}

fn odt_styles_xml(pw: f64, ph: f64) -> String {
    let orient = if ph >= pw { "portrait" } else { "landscape" };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-styles xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" office:version=\"1.3\">\
<office:automatic-styles>\
<style:page-layout style:name=\"pm1\">\
<style:page-layout-properties fo:page-width=\"{w}pt\" fo:page-height=\"{h}pt\" \
fo:margin-top=\"0pt\" fo:margin-bottom=\"0pt\" fo:margin-left=\"0pt\" fo:margin-right=\"0pt\" \
style:print-orientation=\"{o}\"/></style:page-layout></office:automatic-styles>\
<office:master-styles>\
<style:master-page style:name=\"Standard\" style:page-layout-name=\"pm1\"/>\
</office:master-styles></office:document-styles>",
        w = num(pw),
        h = num(ph),
        o = orient
    )
}

fn odt_manifest_xml(image_count: usize) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\" manifest:version=\"1.3\">\
<manifest:file-entry manifest:full-path=\"/\" manifest:version=\"1.3\" manifest:media-type=\"application/vnd.oasis.opendocument.text\"/>\
<manifest:file-entry manifest:full-path=\"content.xml\" manifest:media-type=\"text/xml\"/>\
<manifest:file-entry manifest:full-path=\"styles.xml\" manifest:media-type=\"text/xml\"/>",
    );
    for i in 0..image_count {
        s.push_str(&format!(
            "<manifest:file-entry manifest:full-path=\"Pictures/img{}.png\" manifest:media-type=\"image/png\"/>",
            i + 1
        ));
    }
    s.push_str("</manifest:manifest>");
    s
}

/// Build content.xml. Images encountered are pushed to `images` (global order)
/// so the caller can write the matching `Pictures/imgN.png` parts.
fn odt_content_xml<'a>(pages: &'a [ConvPage], images: &mut Vec<&'a PlacedImage>) -> String {
    let mut auto = String::new(); // automatic styles (frames + per-run paragraphs)
    let mut body = String::new();

    // Shared frame/graphic styles: no wrap, positioned from the page edges.
    auto.push_str(
        "<style:style style:name=\"frT\" style:family=\"graphic\">\
<style:graphic-properties style:wrap=\"none\" style:horizontal-pos=\"from-left\" \
style:horizontal-rel=\"page\" style:vertical-pos=\"from-top\" style:vertical-rel=\"page\" \
draw:fill=\"none\" draw:stroke=\"none\" style:flow-with-text=\"false\"/></style:style>\
<style:style style:name=\"frI\" style:family=\"graphic\">\
<style:graphic-properties style:wrap=\"none\" style:horizontal-pos=\"from-left\" \
style:horizontal-rel=\"page\" style:vertical-pos=\"from-top\" style:vertical-rel=\"page\"/></style:style>\
<style:style style:name=\"frS\" style:family=\"graphic\">\
<style:graphic-properties style:wrap=\"none\" style:horizontal-pos=\"from-left\" \
style:horizontal-rel=\"page\" style:vertical-pos=\"from-top\" style:vertical-rel=\"page\" \
draw:fill=\"none\" draw:stroke=\"solid\" svg:stroke-width=\"0.5pt\" svg:stroke-color=\"#808080\"/></style:style>\
<style:style style:name=\"Pg\" style:family=\"paragraph\"/>\
<style:style style:name=\"PgB\" style:family=\"paragraph\">\
<style:paragraph-properties fo:break-before=\"page\"/></style:style>",
    );

    let mut style_id = 0usize; // per-run paragraph style counter (font size)
    let mut z = 0usize;

    for (pi, page) in pages.iter().enumerate() {
        let page_no = pi + 1;
        let para_style = if pi == 0 { "Pg" } else { "PgB" };
        body.push_str(&format!("<text:p text:style-name=\"{para_style}\">"));

        for t in &page.texts {
            // One paragraph autostyle per run, carrying its full text style.
            let size = t.height.max(1.0);
            auto.push_str(&format!(
                "<style:style style:name=\"T{style_id}\" style:family=\"paragraph\">{props}</style:style>",
                props = odt_text_props(&t.style, size)
            ));
            body.push_str(&format!(
                "<draw:frame draw:style-name=\"frT\" text:anchor-type=\"page\" \
text:anchor-page-number=\"{page_no}\" draw:z-index=\"{z}\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\">\
<draw:text-box><text:p text:style-name=\"T{style_id}\">",
                x = num(t.x),
                y = num(t.y),
                w = num(t.width.max(1.0)),
                h = num(t.height.max(1.0)),
            ));
            esc(&t.text, &mut body);
            body.push_str("</text:p></draw:text-box></draw:frame>");
            style_id += 1;
            z += 1;
        }

        for img in &page.images {
            images.push(img);
            let n = images.len();
            body.push_str(&format!(
                "<draw:frame draw:style-name=\"frI\" text:anchor-type=\"page\" \
text:anchor-page-number=\"{page_no}\" draw:z-index=\"{z}\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\">\
<draw:image xlink:href=\"Pictures/img{n}.png\" xlink:type=\"simple\" xlink:show=\"embed\" \
xlink:actuate=\"onLoad\"/></draw:frame>",
                x = num(img.x),
                y = num(img.y),
                w = num(img.width.max(1.0)),
                h = num(img.height.max(1.0)),
            ));
            z += 1;
        }

        for s in &page.shapes {
            body.push_str(&format!(
                "<draw:rect draw:style-name=\"frS\" text:anchor-type=\"page\" \
text:anchor-page-number=\"{page_no}\" draw:z-index=\"{z}\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\"/>",
                x = num(s.x),
                y = num(s.y),
                w = num(s.width.max(1.0)),
                h = num(s.height.max(1.0)),
            ));
            z += 1;
        }

        body.push_str("</text:p>");
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" \
xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" \
xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" \
xmlns:svg=\"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0\" \
xmlns:xlink=\"http://www.w3.org/1999/xlink\" office:version=\"1.3\">\
<office:automatic-styles>{auto}</office:automatic-styles>\
<office:body><office:text>{body}</office:text></office:body></office:document-content>"
    )
}

// ─────────────────────────────── DOCX (OOXML) ───────────────────────────────

/// English Metric Units per point (914400 EMU/inch ÷ 72 pt/inch).
const EMU_PER_PT: f64 = 12700.0;

fn emu(points: f64) -> i64 {
    (points * EMU_PER_PT).round() as i64
}

fn twips(points: f64) -> i64 {
    (points * 20.0).round() as i64
}

/// Export pages to an editable Word document (`.docx`). Text runs become
/// absolutely-positioned `wps` text boxes, images become anchored pictures, and
/// each page is its own section sized to the page.
pub fn to_docx(pages: &[ConvPage]) -> Vec<u8> {
    let mut zip = ZipWriter::new();
    let mut images: Vec<&PlacedImage> = Vec::new();
    let document = docx_document_xml(pages, &mut images);

    zip.add_deflated("[Content_Types].xml", docx_content_types(images.len()).as_bytes());
    zip.add_deflated(
        "_rels/.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"word/document.xml\"/></Relationships>",
    );
    zip.add_deflated("word/document.xml", document.as_bytes());
    zip.add_deflated("word/_rels/document.xml.rels", docx_rels(images.len()).as_bytes());
    for (i, img) in images.iter().enumerate() {
        zip.add_deflated(&format!("word/media/image{}.png", i + 1), &img.png);
    }
    zip.finish()
}

fn docx_content_types(image_count: usize) -> String {
    let png = if image_count > 0 {
        "<Default Extension=\"png\" ContentType=\"image/png\"/>"
    } else {
        ""
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>{png}\
<Override PartName=\"/word/document.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>"
    )
}

fn docx_rels(image_count: usize) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for i in 0..image_count {
        s.push_str(&format!(
            "<Relationship Id=\"rId{id}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" \
Target=\"media/image{n}.png\"/>",
            id = 100 + i,
            n = i + 1
        ));
    }
    s.push_str("</Relationships>");
    s
}

/// A `<w:sectPr>` sizing one page (twips). `kind` is "" for the final body-level
/// section or " continuous-style" markup handled by the caller.
fn docx_sect_pr(width: f64, height: f64) -> String {
    let orient = if height >= width { "portrait" } else { "landscape" };
    format!(
        "<w:sectPr><w:pgSz w:w=\"{w}\" w:h=\"{h}\" w:orient=\"{o}\"/>\
<w:pgMar w:top=\"0\" w:right=\"0\" w:bottom=\"0\" w:left=\"0\" w:header=\"0\" w:footer=\"0\" w:gutter=\"0\"/></w:sectPr>",
        w = twips(width),
        h = twips(height),
        o = orient
    )
}

/// One absolutely-positioned drawing anchor (shared frame for text boxes,
/// pictures and shapes). `inner` is the `a:graphicData` child.
fn docx_anchor(id: usize, x: f64, y: f64, w: f64, h: f64, inner: &str) -> String {
    format!(
        "<w:r><w:drawing><wp:anchor distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\" \
simplePos=\"0\" relativeHeight=\"{id}\" behindDoc=\"0\" locked=\"0\" layoutInCell=\"1\" allowOverlap=\"1\">\
<wp:simplePos x=\"0\" y=\"0\"/>\
<wp:positionH relativeFrom=\"page\"><wp:posOffset>{x}</wp:posOffset></wp:positionH>\
<wp:positionV relativeFrom=\"page\"><wp:posOffset>{y}</wp:posOffset></wp:positionV>\
<wp:extent cx=\"{w}\" cy=\"{h}\"/><wp:effectExtent l=\"0\" t=\"0\" r=\"0\" b=\"0\"/><wp:wrapNone/>\
<wp:docPr id=\"{id}\" name=\"obj{id}\"/><wp:cNvGraphicFramePr/>\
<a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">{inner}</a:graphic>\
</wp:anchor></w:drawing></w:r>",
        x = emu(x),
        y = emu(y),
        w = emu(w.max(1.0)),
        h = emu(h.max(1.0)),
    )
}

fn docx_document_xml<'a>(pages: &'a [ConvPage], images: &mut Vec<&'a PlacedImage>) -> String {
    let mut body = String::new();
    let mut id = 1usize;
    let page_count = pages.len();

    for (pi, page) in pages.iter().enumerate() {
        let mut para = String::from("<w:p>");
        for t in &page.texts {
            let half_pt = (t.height.max(1.0) * 2.0).round().max(1.0) as i64;
            let mut run_text = String::new();
            esc(&t.text, &mut run_text);
            let inner = format!(
                "<a:graphicData uri=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\">\
<wps:wsp xmlns:wps=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\">\
<wps:cNvSpPr txBox=\"1\"/><wps:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/>\
<a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/></wps:spPr>\
<wps:txbx><w:txbxContent><w:p><w:pPr><w:spacing w:after=\"0\" w:line=\"240\" w:lineRule=\"auto\"/></w:pPr>\
<w:r>{rpr}<w:t xml:space=\"preserve\">{text}</w:t></w:r></w:p></w:txbxContent></wps:txbx>\
<wps:bodyPr rot=\"0\" wrap=\"none\" lIns=\"0\" tIns=\"0\" rIns=\"0\" bIns=\"0\"><a:noAutofit/></wps:bodyPr></wps:wsp></a:graphicData>",
                w = emu(t.width.max(1.0)),
                h = emu(t.height.max(1.0)),
                rpr = docx_run_props(&t.style, half_pt),
                text = run_text,
            );
            para.push_str(&docx_anchor(id, t.x, t.y, t.width, t.height, &inner));
            id += 1;
        }
        for img in &page.images {
            images.push(img);
            let rid = 100 + images.len() - 1;
            let inner = format!(
                "<a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:pic xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:nvPicPr><pic:cNvPr id=\"{id}\" name=\"img{id}\"/><pic:cNvPicPr/></pic:nvPicPr>\
<pic:blipFill><a:blip xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" r:embed=\"rId{rid}\"/>\
<a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm>\
<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData>",
                id = id,
                rid = rid,
                w = emu(img.width.max(1.0)),
                h = emu(img.height.max(1.0)),
            );
            para.push_str(&docx_anchor(id, img.x, img.y, img.width, img.height, &inner));
            id += 1;
        }
        for s in &page.shapes {
            let inner = format!(
                "<a:graphicData uri=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\">\
<wps:wsp xmlns:wps=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\">\
<wps:cNvSpPr/><wps:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm>\
<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/>\
<a:ln w=\"6350\"><a:solidFill><a:srgbClr val=\"808080\"/></a:solidFill></a:ln></wps:spPr>\
<wps:bodyPr/></wps:wsp></a:graphicData>",
                w = emu(s.width.max(1.0)),
                h = emu(s.height.max(1.0)),
            );
            para.push_str(&docx_anchor(id, s.x, s.y, s.width, s.height, &inner));
            id += 1;
        }

        // Section break: a per-page sectPr lives in the LAST paragraph of a
        // page (except the final page, whose sectPr is a body-level child).
        if pi + 1 < page_count {
            para.push_str(&format!("<w:pPr>{}</w:pPr>", docx_sect_pr(page.width, page.height)));
        }
        para.push_str("</w:p>");
        body.push_str(&para);
    }

    let final_sect = pages
        .last()
        .map(|p| docx_sect_pr(p.width, p.height))
        .unwrap_or_else(|| docx_sect_pr(612.0, 792.0));

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">\
<w:body>{body}{final_sect}</w:body></w:document>"
    )
}

// ─────────────────────────────── PPTX (OOXML) ───────────────────────────────

/// A minimal, valid Office theme (required by the slide-master chain).
const PPTX_THEME: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<a:theme xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" name=\"Office Theme\">\
<a:themeElements><a:clrScheme name=\"Office\">\
<a:dk1><a:sysClr val=\"windowText\" lastClr=\"000000\"/></a:dk1>\
<a:lt1><a:sysClr val=\"window\" lastClr=\"FFFFFF\"/></a:lt1>\
<a:dk2><a:srgbClr val=\"44546A\"/></a:dk2><a:lt2><a:srgbClr val=\"E7E6E6\"/></a:lt2>\
<a:accent1><a:srgbClr val=\"4472C4\"/></a:accent1><a:accent2><a:srgbClr val=\"ED7D31\"/></a:accent2>\
<a:accent3><a:srgbClr val=\"A5A5A5\"/></a:accent3><a:accent4><a:srgbClr val=\"FFC000\"/></a:accent4>\
<a:accent5><a:srgbClr val=\"5B9BD5\"/></a:accent5><a:accent6><a:srgbClr val=\"70AD47\"/></a:accent6>\
<a:hlink><a:srgbClr val=\"0563C1\"/></a:hlink><a:folHlink><a:srgbClr val=\"954F72\"/></a:folHlink>\
</a:clrScheme><a:fontScheme name=\"Office\">\
<a:majorFont><a:latin typeface=\"Calibri Light\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:majorFont>\
<a:minorFont><a:latin typeface=\"Calibri\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:minorFont>\
</a:fontScheme><a:fmtScheme name=\"Office\">\
<a:fillStyleLst><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:fillStyleLst>\
<a:lnStyleLst><a:ln w=\"6350\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln>\
<a:ln w=\"12700\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln>\
<a:ln w=\"19050\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln></a:lnStyleLst>\
<a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle>\
<a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst>\
<a:bgFillStyleLst><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:bgFillStyleLst>\
</a:fmtScheme></a:themeElements></a:theme>";

const PPTX_MASTER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sldMaster xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/>\
<a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld>\
<p:clrMap bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" \
accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/>\
<p:sldLayoutIdLst><p:sldLayoutId id=\"2147483649\" r:id=\"rId1\"/></p:sldLayoutIdLst></p:sldMaster>";

const PPTX_LAYOUT: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sldLayout xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" type=\"blank\" preserve=\"1\">\
<p:cSld name=\"Blank\"><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/>\
<a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld>\
<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>";

/// Export pages to an editable PowerPoint presentation (`.pptx`): one slide per
/// page, each text run a positioned text box, each image a placed picture.
pub fn to_pptx(pages: &[ConvPage]) -> Vec<u8> {
    let (sw, sh) = pages.first().map(|p| (p.width, p.height)).unwrap_or((612.0, 792.0));
    let mut zip = ZipWriter::new();

    // Build slides; collect a flat media list (global imageN.png) plus, per
    // slide, the local rId → global-index mapping for that slide's rels.
    let mut media: Vec<&PlacedImage> = Vec::new();
    let mut slides: Vec<String> = Vec::new();
    let mut slide_rels: Vec<Vec<usize>> = Vec::new(); // per slide: global media indices used
    for page in pages {
        let mut used: Vec<usize> = Vec::new();
        let xml = pptx_slide_xml(page, &mut media, &mut used);
        slides.push(xml);
        slide_rels.push(used);
    }

    zip.add_deflated("[Content_Types].xml", pptx_content_types(slides.len(), !media.is_empty()).as_bytes());
    zip.add_deflated(
        "_rels/.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"ppt/presentation.xml\"/></Relationships>",
    );
    zip.add_deflated("ppt/presentation.xml", pptx_presentation_xml(slides.len(), sw, sh).as_bytes());
    zip.add_deflated("ppt/_rels/presentation.xml.rels", pptx_presentation_rels(slides.len()).as_bytes());
    zip.add_deflated("ppt/theme/theme1.xml", PPTX_THEME.as_bytes());
    zip.add_deflated("ppt/slideMasters/slideMaster1.xml", PPTX_MASTER.as_bytes());
    zip.add_deflated(
        "ppt/slideMasters/_rels/slideMaster1.xml.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" \
Target=\"../slideLayouts/slideLayout1.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" \
Target=\"../theme/theme1.xml\"/></Relationships>",
    );
    zip.add_deflated("ppt/slideLayouts/slideLayout1.xml", PPTX_LAYOUT.as_bytes());
    zip.add_deflated(
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" \
Target=\"../slideMasters/slideMaster1.xml\"/></Relationships>",
    );
    for (i, slide) in slides.iter().enumerate() {
        zip.add_deflated(&format!("ppt/slides/slide{}.xml", i + 1), slide.as_bytes());
        zip.add_deflated(
            &format!("ppt/slides/_rels/slide{}.xml.rels", i + 1),
            pptx_slide_rels(&slide_rels[i]).as_bytes(),
        );
    }
    for (i, img) in media.iter().enumerate() {
        zip.add_deflated(&format!("ppt/media/image{}.png", i + 1), &img.png);
    }
    zip.finish()
}

fn pptx_content_types(slide_count: usize, has_media: bool) -> String {
    let png = if has_media {
        "<Default Extension=\"png\" ContentType=\"image/png\"/>"
    } else {
        ""
    };
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>{png}\
<Override PartName=\"/ppt/presentation.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/>\
<Override PartName=\"/ppt/slideMasters/slideMaster1.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml\"/>\
<Override PartName=\"/ppt/slideLayouts/slideLayout1.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml\"/>\
<Override PartName=\"/ppt/theme/theme1.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/>"
    );
    for i in 0..slide_count {
        s.push_str(&format!(
            "<Override PartName=\"/ppt/slides/slide{}.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>",
            i + 1
        ));
    }
    s.push_str("</Types>");
    s
}

fn pptx_presentation_xml(slide_count: usize, sw: f64, sh: f64) -> String {
    let mut ids = String::new();
    for i in 0..slide_count {
        ids.push_str(&format!("<p:sldId id=\"{}\" r:id=\"rId{}\"/>", 256 + i, 2 + i));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:presentation xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId1\"/></p:sldMasterIdLst>\
<p:sldIdLst>{ids}</p:sldIdLst>\
<p:sldSz cx=\"{cx}\" cy=\"{cy}\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/></p:presentation>",
        cx = emu(sw),
        cy = emu(sh),
    )
}

fn pptx_presentation_rels(slide_count: usize) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" \
Target=\"slideMasters/slideMaster1.xml\"/>",
    );
    for i in 0..slide_count {
        s.push_str(&format!(
            "<Relationship Id=\"rId{}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" \
Target=\"slides/slide{}.xml\"/>",
            2 + i,
            i + 1
        ));
    }
    s.push_str("</Relationships>");
    s
}

fn pptx_slide_rels(media_indices: &[usize]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for (local, &global) in media_indices.iter().enumerate() {
        s.push_str(&format!(
            "<Relationship Id=\"rId{}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" \
Target=\"../media/image{}.png\"/>",
            local + 1,
            global + 1
        ));
    }
    s.push_str("</Relationships>");
    s
}

/// Build one slide. Images are appended to `media` (global) and their global
/// index recorded in `used` (the slide's local rId order).
fn pptx_slide_xml<'a>(page: &'a ConvPage, media: &mut Vec<&'a PlacedImage>, used: &mut Vec<usize>) -> String {
    let mut tree = String::new();
    let mut id = 2usize; // ids 1 is the group shape

    for t in &page.texts {
        let hundredths = (t.height.max(1.0) * 100.0).round().max(100.0) as i64;
        let mut run = String::new();
        esc(&t.text, &mut run);
        tree.push_str(&format!(
            "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"t{id}\"/><p:cNvSpPr txBox=\"1\"/><p:nvPr/></p:nvSpPr>\
<p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm>\
<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/></p:spPr>\
<p:txBody><a:bodyPr wrap=\"none\" lIns=\"0\" tIns=\"0\" rIns=\"0\" bIns=\"0\"><a:noAutofit/></a:bodyPr>\
<a:p><a:r>{rpr}<a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>",
            x = emu(t.x),
            y = emu(t.y),
            w = emu(t.width.max(1.0)),
            h = emu(t.height.max(1.0)),
            rpr = pptx_run_props(&t.style, hundredths),
            text = run,
        ));
        id += 1;
    }

    for img in &page.images {
        media.push(img);
        used.push(media.len() - 1);
        let rid = used.len(); // local rId within this slide
        tree.push_str(&format!(
            "<p:pic><p:nvPicPr><p:cNvPr id=\"{id}\" name=\"img{id}\"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr>\
<p:blipFill><a:blip r:embed=\"rId{rid}\"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>\
<p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm>\
<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr></p:pic>",
            x = emu(img.x),
            y = emu(img.y),
            w = emu(img.width.max(1.0)),
            h = emu(img.height.max(1.0)),
        ));
        id += 1;
    }

    for s in &page.shapes {
        tree.push_str(&format!(
            "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"s{id}\"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>\
<p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm>\
<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/>\
<a:ln w=\"6350\"><a:solidFill><a:srgbClr val=\"808080\"/></a:solidFill></a:ln></p:spPr>\
<p:txBody><a:bodyPr/><a:p/></p:txBody></p:sp>",
            x = emu(s.x),
            y = emu(s.y),
            w = emu(s.width.max(1.0)),
            h = emu(s.height.max(1.0)),
        ));
        id += 1;
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sld xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/>\
<a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>{tree}</p:spTree></p:cSld></p:sld>"
    )
}

// ─────────────────────────────── XLSX (OOXML) ───────────────────────────────

/// Spreadsheet column letters for a 0-based index: 0→A, 25→Z, 26→AA …
fn col_letter(mut index: usize) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    letters.reverse();
    String::from_utf8(letters).unwrap()
}

/// Export reconstructed tables (one grid per page) to an `.xlsx` workbook — one
/// sheet per page, cell text carried inline. Use when the PDF is tabular.
pub fn to_xlsx(grids: &[Vec<Vec<String>>]) -> Vec<u8> {
    let sheet_count = grids.len().max(1);
    let mut zip = ZipWriter::new();

    zip.add_deflated("[Content_Types].xml", xlsx_content_types(sheet_count).as_bytes());
    zip.add_deflated(
        "_rels/.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"xl/workbook.xml\"/></Relationships>",
    );
    zip.add_deflated("xl/workbook.xml", xlsx_workbook_xml(sheet_count).as_bytes());
    zip.add_deflated("xl/_rels/workbook.xml.rels", xlsx_workbook_rels(sheet_count).as_bytes());

    if grids.is_empty() {
        zip.add_deflated("xl/worksheets/sheet1.xml", xlsx_sheet_xml(&[]).as_bytes());
    } else {
        for (i, grid) in grids.iter().enumerate() {
            zip.add_deflated(&format!("xl/worksheets/sheet{}.xml", i + 1), xlsx_sheet_xml(grid).as_bytes());
        }
    }
    zip.finish()
}

fn xlsx_content_types(sheet_count: usize) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/xl/workbook.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>",
    );
    for i in 0..sheet_count {
        s.push_str(&format!(
            "<Override PartName=\"/xl/worksheets/sheet{}.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>",
            i + 1
        ));
    }
    s.push_str("</Types>");
    s
}

fn xlsx_workbook_xml(sheet_count: usize) -> String {
    let mut sheets = String::new();
    for i in 0..sheet_count {
        sheets.push_str(&format!(
            "<sheet name=\"Page {n}\" sheetId=\"{n}\" r:id=\"rId{n}\"/>",
            n = i + 1
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
<sheets>{sheets}</sheets></workbook>"
    )
}

fn xlsx_workbook_rels(sheet_count: usize) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for i in 0..sheet_count {
        s.push_str(&format!(
            "<Relationship Id=\"rId{n}\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" \
Target=\"worksheets/sheet{n}.xml\"/>",
            n = i + 1
        ));
    }
    s.push_str("</Relationships>");
    s
}

fn xlsx_sheet_xml(grid: &[Vec<String>]) -> String {
    let mut data = String::new();
    for (r, row) in grid.iter().enumerate() {
        let mut cells = String::new();
        for (c, value) in row.iter().enumerate() {
            if value.is_empty() {
                continue;
            }
            let mut text = String::new();
            esc(value, &mut text);
            cells.push_str(&format!(
                "<c r=\"{col}{row}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{text}</t></is></c>",
                col = col_letter(c),
                row = r + 1,
            ));
        }
        if !cells.is_empty() {
            data.push_str(&format!("<row r=\"{}\">{cells}</row>", r + 1));
        }
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
<sheetData>{data}</sheetData></worksheet>"
    )
}

// ─────────────────────────────── ODS (ODF sheet) ──────────────────────────────

/// Export reconstructed tables to an OpenDocument Spreadsheet (`.ods`), one
/// `table:table` per page. The ODF counterpart of [`to_xlsx`].
pub fn to_ods(grids: &[Vec<Vec<String>>]) -> Vec<u8> {
    let mut zip = ZipWriter::new();
    zip.add_stored("mimetype", b"application/vnd.oasis.opendocument.spreadsheet");

    let mut content = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" \
xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" office:version=\"1.3\">\
<office:body><office:spreadsheet>",
    );
    let sheets = grids.len().max(1);
    for s in 0..sheets {
        content.push_str(&format!("<table:table table:name=\"Page {}\">", s + 1));
        let grid = grids.get(s).map(Vec::as_slice).unwrap_or(&[]);
        if grid.is_empty() {
            content.push_str("<table:table-row><table:table-cell/></table:table-row>");
        }
        for row in grid {
            content.push_str("<table:table-row>");
            for value in row {
                if value.is_empty() {
                    content.push_str("<table:table-cell/>");
                } else {
                    let mut text = String::new();
                    esc(value, &mut text);
                    content.push_str(&format!(
                        "<table:table-cell office:value-type=\"string\"><text:p>{text}</text:p></table:table-cell>"
                    ));
                }
            }
            content.push_str("</table:table-row>");
        }
        content.push_str("</table:table>");
    }
    content.push_str("</office:spreadsheet></office:body></office:document-content>");

    zip.add_deflated("content.xml", content.as_bytes());
    zip.add_deflated(
        "styles.xml",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-styles xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
office:version=\"1.3\"></office:document-styles>",
    );
    zip.add_deflated(
        "META-INF/manifest.xml",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\" manifest:version=\"1.3\">\
<manifest:file-entry manifest:full-path=\"/\" manifest:version=\"1.3\" manifest:media-type=\"application/vnd.oasis.opendocument.spreadsheet\"/>\
<manifest:file-entry manifest:full-path=\"content.xml\" manifest:media-type=\"text/xml\"/>\
<manifest:file-entry manifest:full-path=\"styles.xml\" manifest:media-type=\"text/xml\"/>\
</manifest:manifest>",
    );
    zip.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{PlacedShape, PlacedText};
    use crate::filters::inflate::inflate;

    fn sample_pages() -> Vec<ConvPage> {
        vec![ConvPage {
            width: 612.0,
            height: 792.0,
            texts: vec![PlacedText {
                text: "Hello <World> & co".to_string(),
                x: 72.0,
                y: 100.0,
                width: 200.0,
                height: 12.0,
                style: TextStyle {
                    family: "Helvetica".to_string(),
                    generic: Generic::Sans,
                    bold: true,
                    italic: false,
                    color: Some([1.0, 0.0, 0.0]),
                },
            }],
            images: Vec::new(),
            shapes: vec![PlacedShape { x: 50.0, y: 50.0, width: 100.0, height: 80.0 }],
        }]
    }

    /// Find a stored/deflated entry by name in our own ZIP and return its bytes.
    fn entry(zip: &[u8], name: &str) -> Option<Vec<u8>> {
        let mut i = 0;
        while i + 30 <= zip.len() && zip[i..i + 4] == [0x50, 0x4b, 0x03, 0x04] {
            let method = u16::from_le_bytes([zip[i + 8], zip[i + 9]]);
            let comp = u32::from_le_bytes(zip[i + 18..i + 22].try_into().unwrap()) as usize;
            let nlen = u16::from_le_bytes([zip[i + 26], zip[i + 27]]) as usize;
            let elen = u16::from_le_bytes([zip[i + 28], zip[i + 29]]) as usize;
            let ename = String::from_utf8_lossy(&zip[i + 30..i + 30 + nlen]).to_string();
            let ds = i + 30 + nlen + elen;
            let payload = &zip[ds..ds + comp];
            if ename == name {
                return Some(if method == 8 { inflate(payload).unwrap() } else { payload.to_vec() });
            }
            i = ds + comp;
        }
        None
    }

    #[test]
    fn odt_has_mimetype_first_and_stored() {
        let zip = to_odt(&sample_pages());
        // First local entry name is "mimetype", stored (method 0).
        let nlen = u16::from_le_bytes([zip[26], zip[27]]) as usize;
        let name = String::from_utf8_lossy(&zip[30..30 + nlen]);
        assert_eq!(name, "mimetype");
        assert_eq!(u16::from_le_bytes([zip[8], zip[9]]), 0);
    }

    #[test]
    fn odt_content_has_real_escaped_text() {
        let zip = to_odt(&sample_pages());
        let content = String::from_utf8(entry(&zip, "content.xml").unwrap()).unwrap();
        assert!(content.contains("draw:text-box"), "text is a real text box");
        assert!(content.contains("Hello &lt;World&gt; &amp; co"), "escaped run text present");
        assert!(content.contains("draw:rect"), "shape becomes a rectangle");
        assert!(!content.contains("draw:image"), "no image when none provided");
        // style fidelity
        assert!(content.contains("fo:font-weight=\"bold\""), "bold emitted");
        assert!(content.contains("fo:font-family=\"Helvetica\""), "family emitted");
        assert!(content.contains("fo:color=\"#FF0000\""), "colour emitted");
    }

    #[test]
    fn docx_has_real_text_box_and_section() {
        let zip = to_docx(&sample_pages());
        let doc = String::from_utf8(entry(&zip, "word/document.xml").unwrap()).unwrap();
        assert!(doc.contains("wps:txbx"), "text is a real Word text box");
        assert!(doc.contains("Hello &lt;World&gt; &amp; co"), "escaped run text present");
        assert!(doc.contains("<w:t xml:space=\"preserve\">"), "editable run, not image");
        assert!(doc.contains("w:sectPr"), "page becomes a section");
        assert!(doc.contains("<w:b/>"), "bold emitted");
        assert!(doc.contains("w:rFonts w:ascii=\"Helvetica\""), "font family emitted");
        assert!(doc.contains("<w:color w:val=\"FF0000\"/>"), "colour emitted");
        // [Content_Types].xml must be a parseable, well-formed part.
        let ct = String::from_utf8(entry(&zip, "[Content_Types].xml").unwrap()).unwrap();
        assert!(ct.contains("wordprocessingml.document.main+xml"));
    }

    #[test]
    fn docx_multipage_emits_one_section_per_page() {
        let pages = vec![sample_pages().remove(0), sample_pages().remove(0)];
        let zip = to_docx(&pages);
        let doc = String::from_utf8(entry(&zip, "word/document.xml").unwrap()).unwrap();
        // Two pages → two sectPr (one in-paragraph break + one body-level final).
        assert_eq!(doc.matches("<w:sectPr>").count(), 2);
    }

    #[test]
    fn col_letters_match_spreadsheet_convention() {
        assert_eq!(col_letter(0), "A");
        assert_eq!(col_letter(25), "Z");
        assert_eq!(col_letter(26), "AA");
        assert_eq!(col_letter(27), "AB");
        assert_eq!(col_letter(701), "ZZ");
        assert_eq!(col_letter(702), "AAA");
    }

    #[test]
    fn xlsx_writes_inline_cells_and_a_sheet_per_page() {
        let grids = vec![
            vec![
                vec!["Name".to_string(), "Age".to_string()],
                vec!["Alice & Bob".to_string(), "30".to_string()],
            ],
            vec![vec!["Page two".to_string()]],
        ];
        let zip = to_xlsx(&grids);
        let s1 = String::from_utf8(entry(&zip, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(s1.contains("t=\"inlineStr\""), "cells carry text inline");
        assert!(s1.contains("<c r=\"B2\""), "B2 addressed");
        assert!(s1.contains("Alice &amp; Bob"), "escaped cell text");
        let wb = String::from_utf8(entry(&zip, "xl/workbook.xml").unwrap()).unwrap();
        assert_eq!(wb.matches("<sheet ").count(), 2, "one sheet per page");
        assert!(entry(&zip, "xl/worksheets/sheet2.xml").is_some());
    }

    #[test]
    fn ods_writes_table_cells_per_page() {
        let grids = vec![vec![
            vec!["A".to_string(), "B".to_string()],
            vec!["1".to_string(), "2".to_string()],
        ]];
        let zip = to_ods(&grids);
        // mimetype first + stored.
        let nlen = u16::from_le_bytes([zip[26], zip[27]]) as usize;
        assert_eq!(&zip[30..30 + nlen], b"mimetype");
        let content = String::from_utf8(entry(&zip, "content.xml").unwrap()).unwrap();
        assert!(content.contains("table:table table:name=\"Page 1\""));
        assert!(content.contains("office:value-type=\"string\"><text:p>1</text:p>"));
    }

    #[test]
    fn pptx_has_slide_chain_and_real_text() {
        let zip = to_pptx(&sample_pages());
        // The master/layout/theme chain parts must all be present.
        for part in [
            "ppt/presentation.xml",
            "ppt/slideMasters/slideMaster1.xml",
            "ppt/slideLayouts/slideLayout1.xml",
            "ppt/theme/theme1.xml",
            "ppt/slides/slide1.xml",
        ] {
            assert!(entry(&zip, part).is_some(), "missing {part}");
        }
        let slide = String::from_utf8(entry(&zip, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(slide.contains("<p:sp>"), "text is a real positioned shape");
        assert!(slide.contains("<a:t>Hello &lt;World&gt; &amp; co</a:t>"), "escaped run text");
        assert!(slide.contains(" b=\"1\""), "bold emitted");
        assert!(slide.contains("<a:latin typeface=\"Helvetica\"/>"), "font family emitted");
        assert!(slide.contains("<a:srgbClr val=\"FF0000\"/>"), "colour emitted");
        let pres = String::from_utf8(entry(&zip, "ppt/presentation.xml").unwrap()).unwrap();
        assert!(pres.contains("p:sldSz"), "slide size set from page");
        assert_eq!(pres.matches("<p:sldId ").count(), 1, "one slide per page");
    }
}
