//! Per-format XML builders for the editable Office exporters.
//!
//! Each `to_*` takes already-normalized [`ConvPage`]s (top-down points) and
//! returns a complete `.odt`/`.docx`/`.pptx` byte stream via the [`super::zip`]
//! container. Content is **native and editable**: a PDF show-text run becomes a
//! placed text box, an image XObject a placed picture, a vector path a real
//! coloured shape (rectangle or custom path) — never a flattened page raster.

use super::style::{Generic, TextStyle};
use super::zip::ZipWriter;
use super::{ConvPage, PlacedImage, PlacedShape};
use crate::content::num;
use crate::content::vector::PathSeg;

/// DOCX `<w:rPr>` run properties (fonts/bold/italic/colour/size) for a style.
fn docx_run_props(style: &TextStyle, half_pt: i64) -> String {
    let mut p = String::from("<w:rPr>");
    if !style.family.is_empty() {
        let mut fam = String::new();
        esc(&style.family, &mut fam);
        p.push_str(&format!(
            "<w:rFonts w:ascii=\"{fam}\" w:hAnsi=\"{fam}\" w:cs=\"{fam}\"/>"
        ));
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
pub(super) fn esc(text: &str, out: &mut String) {
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

// ───────────────────────────── shape paint helpers ─────────────────────────────

/// An RGB triple (`0..=1`) as an upper-case `RRGGBB` hex string.
pub(super) fn shape_hex(rgb: [f64; 3]) -> String {
    let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("{:02X}{:02X}{:02X}", q(rgb[0]), q(rgb[1]), q(rgb[2]))
}

/// Alpha (`0..=1`) as an ODF percentage string, e.g. `0.5` → `"50%"`.
fn odf_opacity(a: f64) -> String {
    format!("{}%", (a.clamp(0.0, 1.0) * 100.0).round() as i64)
}

/// Alpha (`0..=1`) as DrawingML thousandths-of-a-percent, e.g. `0.5` → `50000`.
fn dml_alpha(a: f64) -> i64 {
    (a.clamp(0.0, 1.0) * 100_000.0).round() as i64
}

/// True when the path is a single axis-aligned rectangle (the common case —
/// `re`, or `m`/`l`×3/`h`). Such shapes are emitted as a plain rectangle (with
/// the real colours) rather than a custom path.
pub(super) fn shape_is_rect(shape: &PlacedShape) -> bool {
    let segs = &shape.segments;
    if segs.is_empty() {
        return true; // no geometry → fall back to the bounding rectangle
    }
    let core = if matches!(segs.last(), Some(PathSeg::Close)) {
        &segs[..segs.len() - 1]
    } else {
        &segs[..]
    };
    if core.len() != 4 {
        return false;
    }
    // First op a Move, the other three straight Lines, four corner points total.
    let mut pts = [(0.0f64, 0.0f64); 4];
    for (i, seg) in core.iter().enumerate() {
        match (*seg, i) {
            (PathSeg::Move(x, y), 0) => pts[0] = (x, y),
            (PathSeg::Line(x, y), i) if i > 0 => pts[i] = (x, y),
            _ => return false,
        }
    }
    // Every edge is horizontal or vertical ⇒ axis-aligned rectangle.
    (0..4).all(|i| {
        let (ax, ay) = pts[i];
        let (bx, by) = pts[(i + 1) % 4];
        (ax - bx).abs() < 1e-3 || (ay - by).abs() < 1e-3
    })
}

/// ODF `svg:d` path data (absolute, in points) for a top-down shape path.
pub(super) fn odf_path_d(segments: &[PathSeg]) -> String {
    let mut d = String::new();
    for seg in segments {
        match *seg {
            PathSeg::Move(x, y) => d.push_str(&format!("M {} {} ", num(x), num(y))),
            PathSeg::Line(x, y) => d.push_str(&format!("L {} {} ", num(x), num(y))),
            PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => d.push_str(&format!(
                "C {} {} {} {} {} {} ",
                num(x1),
                num(y1),
                num(x2),
                num(y2),
                num(x3),
                num(y3)
            )),
            PathSeg::Close => d.push_str("Z "),
        }
    }
    d.trim_end().to_string()
}

/// Points → centimetres (1 pt = 1/72", 1" = 2.54 cm), for ODF dash lengths.
fn pt_to_cm(points: f64) -> f64 {
    points / 28.3465
}

/// The `draw:stroke-dash` name for a shape style `name` (e.g. `S5` → `sdS5`),
/// referenced from its `<style:graphic-properties draw:stroke-dash=…>`.
fn odf_dash_name(name: &str) -> String {
    format!("sd{name}")
}

/// An ODF `<draw:stroke-dash>` definition for a shape's `dash` pattern, lengths
/// converted from points to centimetres. The PDF dash array is cyclic and may be
/// odd-length, read here as on/off pairs (final element duplicated when odd).
/// `draw:style="rect"`, `draw:dots1` count + `draw:dots1-length` give the dash
/// segments; `draw:distance` the gap. Empty for a solid line. Lives in
/// automatic-styles and is referenced via `draw:stroke-dash="sd<name>"`.
fn odf_stroke_dash(name: &str, dash: &[f64]) -> String {
    if dash.is_empty() {
        return String::new();
    }
    let on = dash[0];
    let off = *dash.get(1).unwrap_or(&on);
    format!(
        "<draw:stroke-dash draw:name=\"{dn}\" draw:display-name=\"{dn}\" draw:style=\"rect\" \
draw:dots1=\"1\" draw:dots1-length=\"{onl}cm\" draw:distance=\"{offl}cm\"/>",
        dn = odf_dash_name(name),
        onl = num(pt_to_cm(on.max(0.0))),
        offl = num(pt_to_cm(off.max(0.0))),
    )
}

/// Inline ODF `<style:style>` graphic properties for a shape's paint state.
/// `name` is the autostyle id; the element references it via `draw:style-name`.
/// When the shape is dashed, a sibling [`odf_stroke_dash`] definition is
/// prepended and referenced via `draw:stroke-dash`.
pub(super) fn odf_shape_style(name: &str, shape: &PlacedShape) -> String {
    let mut p = String::from(
        "<style:graphic-properties style:wrap=\"none\" style:horizontal-pos=\"from-left\" \
style:horizontal-rel=\"page\" style:vertical-pos=\"from-top\" style:vertical-rel=\"page\" \
style:flow-with-text=\"false\"",
    );
    match shape.fill {
        Some(rgb) => {
            p.push_str(&format!(
                " draw:fill=\"solid\" draw:fill-color=\"#{}\"",
                shape_hex(rgb)
            ));
            if shape.fill_alpha < 0.999 {
                p.push_str(&format!(
                    " draw:opacity=\"{}\"",
                    odf_opacity(shape.fill_alpha)
                ));
            }
        }
        None => p.push_str(" draw:fill=\"none\""),
    }
    match shape.stroke {
        Some(rgb) => {
            // Dashed strokes reference a `<draw:stroke-dash>`; solid otherwise.
            if shape.dash.is_empty() {
                p.push_str(" draw:stroke=\"solid\"");
            } else {
                p.push_str(&format!(
                    " draw:stroke=\"dash\" draw:stroke-dash=\"{}\"",
                    odf_dash_name(name)
                ));
            }
            p.push_str(&format!(
                " svg:stroke-width=\"{}pt\" svg:stroke-color=\"#{}\"",
                num(shape.stroke_width.max(0.0)),
                shape_hex(rgb)
            ));
            if shape.stroke_alpha < 0.999 {
                p.push_str(&format!(
                    " svg:stroke-opacity=\"{}\"",
                    odf_opacity(shape.stroke_alpha)
                ));
            }
        }
        None => p.push_str(" draw:stroke=\"none\""),
    }
    p.push_str("/>");
    // The dash definition (if any) precedes the style that references it.
    let dash_def = match shape.stroke {
        Some(_) => odf_stroke_dash(name, &shape.dash),
        None => String::new(),
    };
    format!(
        "{dash_def}<style:style style:name=\"{name}\" style:family=\"graphic\">{p}</style:style>"
    )
}

/// DrawingML `<a:custGeom>` for a shape path, with coordinates in EMU **relative
/// to the shape's bounding box** (origin = the box's top-left). `w`/`h` are the
/// box size in points (the geometry guide space). Used by DOCX and PPTX.
pub(super) fn dml_cust_geom(shape: &PlacedShape, w_pt: f64, h_pt: f64) -> String {
    let cx = emu(w_pt.max(1.0));
    let cy = emu(h_pt.max(1.0));
    let mut path = String::new();
    let ex = |x: f64| emu(x - shape.x);
    let ey = |y: f64| emu(y - shape.y);
    for seg in &shape.segments {
        match *seg {
            PathSeg::Move(x, y) => path.push_str(&format!(
                "<a:moveTo><a:pt x=\"{}\" y=\"{}\"/></a:moveTo>",
                ex(x),
                ey(y)
            )),
            PathSeg::Line(x, y) => path.push_str(&format!(
                "<a:lnTo><a:pt x=\"{}\" y=\"{}\"/></a:lnTo>",
                ex(x),
                ey(y)
            )),
            PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => path.push_str(&format!(
                "<a:cubicBezTo><a:pt x=\"{}\" y=\"{}\"/><a:pt x=\"{}\" y=\"{}\"/>\
<a:pt x=\"{}\" y=\"{}\"/></a:cubicBezTo>",
                ex(x1),
                ey(y1),
                ex(x2),
                ey(y2),
                ex(x3),
                ey(y3)
            )),
            PathSeg::Close => path.push_str("<a:close/>"),
        }
    }
    format!(
        "<a:custGeom><a:avLst/><a:gdLst/><a:ahLst/><a:cxnLst/>\
<a:rect l=\"0\" t=\"0\" r=\"{cx}\" b=\"{cy}\"/>\
<a:pathLst><a:path w=\"{cx}\" h=\"{cy}\">{path}</a:path></a:pathLst></a:custGeom>"
    )
}

/// DrawingML `<a:solidFill>`/`<a:noFill>` for a shape's fill.
pub(super) fn dml_fill(shape: &PlacedShape) -> String {
    match shape.fill {
        Some(rgb) => format!(
            "<a:solidFill><a:srgbClr val=\"{}\"><a:alpha val=\"{}\"/></a:srgbClr></a:solidFill>",
            shape_hex(rgb),
            dml_alpha(shape.fill_alpha)
        ),
        None => "<a:noFill/>".to_string(),
    }
}

/// DrawingML dash element for a shape's `dash` pattern. DrawingML expresses dash
/// lengths as a **percentage of the line width** in thousandths of a percent
/// (`ST_PositivePercentage`), so each PDF on/off length (points) becomes
/// `round(len / stroke_width * 100000)`. The PDF dash array is cyclic and may be
/// odd-length (e.g. `[3]` = 3 on / 3 off), so it is read as on/off pairs, the
/// final element duplicated when the count is odd. Returns `<a:prstDash>` (a
/// generic dash) when the width is non-positive — there is no width to scale by.
fn dml_dash(shape: &PlacedShape) -> String {
    if shape.dash.is_empty() {
        return String::new();
    }
    let w = shape.stroke_width;
    if w <= 0.0 {
        return "<a:prstDash val=\"dash\"/>".to_string();
    }
    let pct = |len: f64| ((len.max(0.0) / w) * 100_000.0).round().max(1.0) as i64;
    let d = &shape.dash;
    let mut stops = String::new();
    let mut i = 0;
    while i < d.len() {
        let on = d[i];
        // Odd-length array: the missing "off" repeats the on length (PDF treats
        // an odd array as itself concatenated, which yields on==off here).
        let off = *d.get(i + 1).unwrap_or(&on);
        stops.push_str(&format!("<a:ds d=\"{}\" sp=\"{}\"/>", pct(on), pct(off)));
        i += 2;
    }
    format!("<a:custDash>{stops}</a:custDash>")
}

/// DrawingML `<a:ln>` (outline) for a shape's stroke.
pub(super) fn dml_line(shape: &PlacedShape) -> String {
    match shape.stroke {
        Some(rgb) => {
            format!(
                "<a:ln w=\"{}\"><a:solidFill><a:srgbClr val=\"{}\"><a:alpha val=\"{}\"/>\
</a:srgbClr></a:solidFill>{dash}</a:ln>",
                emu(shape.stroke_width.max(0.0)),
                shape_hex(rgb),
                dml_alpha(shape.stroke_alpha),
                dash = dml_dash(shape),
            )
        }
        None => "<a:ln><a:noFill/></a:ln>".to_string(),
    }
}

// ─────────────────────────────── ODT (ODF text) ───────────────────────────────

/// Export pages to an OpenDocument Text (`.odt`) document.
pub fn to_odt(pages: &[ConvPage]) -> Vec<u8> {
    let (pw, ph) = pages
        .first()
        .map(|p| (p.width, p.height))
        .unwrap_or((612.0, 792.0));
    let mut zip = ZipWriter::new();

    // The mimetype entry must be first and stored uncompressed (ODF §3.3).
    zip.add_stored("mimetype", b"application/vnd.oasis.opendocument.text");

    let mut images: Vec<&PlacedImage> = Vec::new();
    let content = odt_content_xml(pages, &mut images);
    zip.add_deflated("content.xml", content.as_bytes());
    zip.add_deflated("styles.xml", odt_styles_xml(pw, ph).as_bytes());
    zip.add_deflated(
        "META-INF/manifest.xml",
        odt_manifest_xml(images.len()).as_bytes(),
    );
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
            let style = format!("S{style_id}");
            auto.push_str(&odf_shape_style(&style, s));
            let (x, y) = (num(s.x), num(s.y));
            let (w, h) = (num(s.width.max(1.0)), num(s.height.max(1.0)));
            if shape_is_rect(s) {
                body.push_str(&format!(
                    "<draw:rect draw:style-name=\"{style}\" text:anchor-type=\"page\" \
text:anchor-page-number=\"{page_no}\" draw:z-index=\"{z}\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\"/>"
                ));
            } else {
                // `svg:d` is in absolute top-down points; the viewBox is offset by
                // the box origin so those coordinates map straight onto the frame.
                body.push_str(&format!(
                    "<draw:path draw:style-name=\"{style}\" text:anchor-type=\"page\" \
text:anchor-page-number=\"{page_no}\" draw:z-index=\"{z}\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\" \
svg:viewBox=\"{vbx} {vby} {vbw} {vbh}\" svg:d=\"{d}\"/>",
                    vbx = num(s.x),
                    vby = num(s.y),
                    vbw = num(s.width.max(1.0)),
                    vbh = num(s.height.max(1.0)),
                    d = odf_path_d(&s.segments),
                ));
            }
            style_id += 1;
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

/// Export pages to an editable OpenDocument Presentation (`.odp`): one slide per
/// page, each text run a positioned text box, each image a picture, each shape a
/// rectangle — real editable objects (never a page raster).
pub fn to_odp(pages: &[ConvPage]) -> Vec<u8> {
    let (pw, ph) = pages
        .first()
        .map(|p| (p.width, p.height))
        .unwrap_or((792.0, 612.0));
    let mut zip = ZipWriter::new();

    // The mimetype entry must be first and stored uncompressed (ODF §3.3).
    zip.add_stored(
        "mimetype",
        b"application/vnd.oasis.opendocument.presentation",
    );

    let mut images: Vec<&PlacedImage> = Vec::new();
    let content = odp_content_xml(pages, &mut images);
    zip.add_deflated("content.xml", content.as_bytes());
    zip.add_deflated("styles.xml", odp_styles_xml(pw, ph).as_bytes());
    zip.add_deflated(
        "META-INF/manifest.xml",
        odp_manifest_xml(images.len()).as_bytes(),
    );
    for (i, img) in images.iter().enumerate() {
        zip.add_deflated(&format!("Pictures/img{}.png", i + 1), &img.png);
    }
    zip.finish()
}

fn odp_styles_xml(pw: f64, ph: f64) -> String {
    let orient = if ph >= pw { "portrait" } else { "landscape" };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-styles xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" \
xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" office:version=\"1.3\">\
<office:automatic-styles>\
<style:page-layout style:name=\"pm1\">\
<style:page-layout-properties fo:page-width=\"{w}pt\" fo:page-height=\"{h}pt\" \
fo:margin-top=\"0pt\" fo:margin-bottom=\"0pt\" fo:margin-left=\"0pt\" fo:margin-right=\"0pt\" \
style:print-orientation=\"{o}\"/></style:page-layout></office:automatic-styles>\
<office:master-styles>\
<style:master-page style:name=\"Default\" style:page-layout-name=\"pm1\"/>\
</office:master-styles></office:document-styles>",
        w = num(pw),
        h = num(ph),
        o = orient
    )
}

fn odp_manifest_xml(image_count: usize) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\" manifest:version=\"1.3\">\
<manifest:file-entry manifest:full-path=\"/\" manifest:version=\"1.3\" manifest:media-type=\"application/vnd.oasis.opendocument.presentation\"/>\
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

/// Build an ODP content.xml: one `<draw:page>` per page, each carrying the
/// positioned text/image/shape frames. Mirrors `odt_content_xml` but the frames
/// are direct children of the slide (no text anchor).
fn odp_content_xml<'a>(pages: &'a [ConvPage], images: &mut Vec<&'a PlacedImage>) -> String {
    let mut auto = String::new();
    let mut body = String::new();

    auto.push_str(
        "<style:style style:name=\"dp1\" style:family=\"drawing-page\">\
<style:drawing-page-properties draw:fill=\"none\" draw:background-size=\"border\"/></style:style>\
<style:style style:name=\"frT\" style:family=\"graphic\">\
<style:graphic-properties draw:fill=\"none\" draw:stroke=\"none\" \
draw:auto-grow-width=\"false\" draw:auto-grow-height=\"false\" fo:padding=\"0pt\" \
draw:textarea-vertical-align=\"top\"/></style:style>\
<style:style style:name=\"frI\" style:family=\"graphic\">\
<style:graphic-properties draw:fill=\"none\" draw:stroke=\"none\"/></style:style>",
    );

    let mut style_id = 0usize;
    for page in pages {
        body.push_str("<draw:page draw:style-name=\"dp1\" draw:master-page-name=\"Default\">");

        for t in &page.texts {
            let size = t.height.max(1.0);
            auto.push_str(&format!(
                "<style:style style:name=\"T{style_id}\" style:family=\"paragraph\">{props}</style:style>",
                props = odt_text_props(&t.style, size)
            ));
            body.push_str(&format!(
                "<draw:frame draw:style-name=\"frT\" draw:layer=\"layout\" \
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
        }

        for img in &page.images {
            images.push(img);
            let n = images.len();
            body.push_str(&format!(
                "<draw:frame draw:style-name=\"frI\" draw:layer=\"layout\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\">\
<draw:image xlink:href=\"Pictures/img{n}.png\" xlink:type=\"simple\" xlink:show=\"embed\" \
xlink:actuate=\"onLoad\"/></draw:frame>",
                x = num(img.x),
                y = num(img.y),
                w = num(img.width.max(1.0)),
                h = num(img.height.max(1.0)),
            ));
        }

        for s in &page.shapes {
            let style = format!("S{style_id}");
            auto.push_str(&odf_shape_style(&style, s));
            let (x, y) = (num(s.x), num(s.y));
            let (w, h) = (num(s.width.max(1.0)), num(s.height.max(1.0)));
            if shape_is_rect(s) {
                body.push_str(&format!(
                    "<draw:rect draw:style-name=\"{style}\" draw:layer=\"layout\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\"/>"
                ));
            } else {
                body.push_str(&format!(
                    "<draw:path draw:style-name=\"{style}\" draw:layer=\"layout\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\" \
svg:viewBox=\"{vbx} {vby} {vbw} {vbh}\" svg:d=\"{d}\"/>",
                    vbx = num(s.x),
                    vby = num(s.y),
                    vbw = num(s.width.max(1.0)),
                    vbh = num(s.height.max(1.0)),
                    d = odf_path_d(&s.segments),
                ));
            }
            style_id += 1;
        }

        body.push_str("</draw:page>");
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" \
xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" \
xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" \
xmlns:svg=\"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0\" \
xmlns:presentation=\"urn:oasis:names:tc:opendocument:xmlns:presentation:1.0\" \
xmlns:xlink=\"http://www.w3.org/1999/xlink\" office:version=\"1.3\">\
<office:automatic-styles>{auto}</office:automatic-styles>\
<office:body><office:presentation>{body}</office:presentation></office:body></office:document-content>"
    )
}

// ─────────────────────────────── DOCX (OOXML) ───────────────────────────────

/// English Metric Units per point (914400 EMU/inch ÷ 72 pt/inch).
const EMU_PER_PT: f64 = 12700.0;

pub(super) fn emu(points: f64) -> i64 {
    (points * EMU_PER_PT).round() as i64
}

pub(super) fn twips(points: f64) -> i64 {
    (points * 20.0).round() as i64
}

/// Export pages to an editable Word document (`.docx`). Text runs become
/// absolutely-positioned `wps` text boxes, images become anchored pictures, and
/// each page is its own section sized to the page.
pub fn to_docx(pages: &[ConvPage]) -> Vec<u8> {
    let mut zip = ZipWriter::new();
    let mut images: Vec<&PlacedImage> = Vec::new();
    let document = docx_document_xml(pages, &mut images);

    zip.add_deflated(
        "[Content_Types].xml",
        docx_content_types(images.len()).as_bytes(),
    );
    zip.add_deflated(
        "_rels/.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"word/document.xml\"/></Relationships>",
    );
    zip.add_deflated("word/document.xml", document.as_bytes());
    zip.add_deflated(
        "word/_rels/document.xml.rels",
        docx_rels(images.len()).as_bytes(),
    );
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
    let orient = if height >= width {
        "portrait"
    } else {
        "landscape"
    };
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
            para.push_str(&docx_anchor(
                id, img.x, img.y, img.width, img.height, &inner,
            ));
            id += 1;
        }
        for s in &page.shapes {
            let geom = if shape_is_rect(s) {
                "<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom>".to_string()
            } else {
                dml_cust_geom(s, s.width, s.height)
            };
            let sp_pr = format!(
                "<wps:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm>{geom}{fill}{ln}</wps:spPr>",
                w = emu(s.width.max(1.0)),
                h = emu(s.height.max(1.0)),
                fill = dml_fill(s),
                ln = dml_line(s),
            );
            let inner = format!(
                "<a:graphicData uri=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\">\
<wps:wsp xmlns:wps=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\">\
<wps:cNvSpPr/>{sp_pr}<wps:bodyPr/></wps:wsp></a:graphicData>"
            );
            para.push_str(&docx_anchor(id, s.x, s.y, s.width, s.height, &inner));
            id += 1;
        }

        // Section break: a per-page sectPr lives in the LAST paragraph of a
        // page (except the final page, whose sectPr is a body-level child).
        if pi + 1 < page_count {
            para.push_str(&format!(
                "<w:pPr>{}</w:pPr>",
                docx_sect_pr(page.width, page.height)
            ));
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
pub(super) const PPTX_THEME: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
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

pub(super) const PPTX_MASTER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sldMaster xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/>\
<a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld>\
<p:clrMap bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" \
accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/>\
<p:sldLayoutIdLst><p:sldLayoutId id=\"2147483649\" r:id=\"rId1\"/></p:sldLayoutIdLst></p:sldMaster>";

pub(super) const PPTX_LAYOUT: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
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
    let (sw, sh) = pages
        .first()
        .map(|p| (p.width, p.height))
        .unwrap_or((612.0, 792.0));
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

    zip.add_deflated(
        "[Content_Types].xml",
        pptx_content_types(slides.len(), !media.is_empty()).as_bytes(),
    );
    zip.add_deflated(
        "_rels/.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"ppt/presentation.xml\"/></Relationships>",
    );
    zip.add_deflated(
        "ppt/presentation.xml",
        pptx_presentation_xml(slides.len(), sw, sh).as_bytes(),
    );
    zip.add_deflated(
        "ppt/_rels/presentation.xml.rels",
        pptx_presentation_rels(slides.len()).as_bytes(),
    );
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
        ids.push_str(&format!(
            "<p:sldId id=\"{}\" r:id=\"rId{}\"/>",
            256 + i,
            2 + i
        ));
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
fn pptx_slide_xml<'a>(
    page: &'a ConvPage,
    media: &mut Vec<&'a PlacedImage>,
    used: &mut Vec<usize>,
) -> String {
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
        let geom = if shape_is_rect(s) {
            "<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom>".to_string()
        } else {
            dml_cust_geom(s, s.width, s.height)
        };
        let sp_pr = format!(
            "<p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm>{geom}{fill}{ln}</p:spPr>",
            x = emu(s.x),
            y = emu(s.y),
            w = emu(s.width.max(1.0)),
            h = emu(s.height.max(1.0)),
            fill = dml_fill(s),
            ln = dml_line(s),
        );
        tree.push_str(&format!(
            "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"s{id}\"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>{sp_pr}<p:txBody><a:bodyPr/><a:p/></p:txBody></p:sp>"
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
pub(super) fn col_letter(mut index: usize) -> String {
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

/// The display name for sheet `i`: the caller-supplied `names[i]` when present
/// and non-empty, else the default `Page <i+1>`.
fn sheet_name(names: &[String], i: usize) -> String {
    names
        .get(i)
        .filter(|n| !n.is_empty())
        .cloned()
        .unwrap_or_else(|| format!("Page {}", i + 1))
}

/// Export reconstructed tables (one grid per page) to an `.xlsx` workbook — one
/// sheet per page, cell text carried inline. Use when the PDF is tabular.
/// Sheets are named `Page <n>`; use [`to_xlsx_named`] for custom names.
pub fn to_xlsx(grids: &[Vec<Vec<String>>]) -> Vec<u8> {
    to_xlsx_named(grids, &[])
}

/// As [`to_xlsx`] but with explicit per-sheet names (index-aligned to `grids`); a
/// missing or empty name falls back to `Page <n>`. Lets a host preserve its own
/// sheet titles (e.g. a single concatenated `Sheet1`) when reusing this writer.
pub fn to_xlsx_named(grids: &[Vec<Vec<String>>], names: &[String]) -> Vec<u8> {
    to_xlsx_with_shapes(grids, names, &[])
}

/// As [`to_xlsx_named`] but also lays out each sheet's floating vector shapes
/// (`shapes_per_sheet`, index-aligned to `grids`). A sheet with shapes gains a
/// DrawingML drawing part (`xl/drawings/drawingN.xml`) wired to the worksheet via
/// an `<drawing>` reference + a worksheet rels part; the `drawing` content-type
/// and the `drawings/drawingN.xml` overrides are registered in
/// `[Content_Types].xml`. Each shape is an `xdr:absoluteAnchor` placed in EMU
/// (1 pt = 12700 EMU), drawn with the same `a:custGeom`/`prstGeom` + fill/line/
/// dash helpers as the DOCX/PPTX exporters. Sheets without shapes are unchanged.
pub fn to_xlsx_with_shapes(
    grids: &[Vec<Vec<String>>],
    names: &[String],
    shapes_per_sheet: &[Vec<PlacedShape>],
) -> Vec<u8> {
    let sheet_count = grids.len().max(1);
    let mut zip = ZipWriter::new();

    // Which sheets carry shapes (and so need a drawing part). The Nth such sheet
    // maps to drawing{N} (1-based), shared by the content-type override, the
    // worksheet `<drawing r:id>` and the worksheet rels.
    let shapes_for =
        |i: usize| -> &[PlacedShape] { shapes_per_sheet.get(i).map(Vec::as_slice).unwrap_or(&[]) };
    let mut drawing_no = vec![0usize; sheet_count]; // sheet → drawing number (0 = none)
    let mut next = 0usize;
    for (slot, n) in drawing_no.iter_mut().enumerate() {
        if !shapes_for(slot).is_empty() {
            next += 1;
            *n = next;
        }
    }

    zip.add_deflated(
        "[Content_Types].xml",
        xlsx_content_types(sheet_count, &drawing_no).as_bytes(),
    );
    zip.add_deflated(
        "_rels/.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
Target=\"xl/workbook.xml\"/></Relationships>",
    );
    zip.add_deflated(
        "xl/workbook.xml",
        xlsx_workbook_xml(sheet_count, names).as_bytes(),
    );
    zip.add_deflated(
        "xl/_rels/workbook.xml.rels",
        xlsx_workbook_rels(sheet_count).as_bytes(),
    );

    if grids.is_empty() {
        zip.add_deflated(
            "xl/worksheets/sheet1.xml",
            xlsx_sheet_xml(&[], drawing_no[0] != 0).as_bytes(),
        );
    } else {
        for (i, grid) in grids.iter().enumerate() {
            zip.add_deflated(
                &format!("xl/worksheets/sheet{}.xml", i + 1),
                xlsx_sheet_xml(grid, drawing_no[i] != 0).as_bytes(),
            );
        }
    }

    // Per-sheet drawing parts + their rels (worksheet → drawing).
    for (i, &n) in drawing_no.iter().enumerate() {
        if n == 0 {
            continue;
        }
        zip.add_deflated(
            &format!("xl/drawings/drawing{n}.xml"),
            xlsx_drawing_xml(shapes_for(i)).as_bytes(),
        );
        zip.add_deflated(
            &format!("xl/worksheets/_rels/sheet{}.xml.rels", i + 1),
            xlsx_sheet_rels(n).as_bytes(),
        );
    }
    zip.finish()
}

fn xlsx_content_types(sheet_count: usize, drawing_no: &[usize]) -> String {
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
    // One drawing override per sheet that has shapes.
    for &n in drawing_no {
        if n != 0 {
            s.push_str(&format!(
                "<Override PartName=\"/xl/drawings/drawing{n}.xml\" \
ContentType=\"application/vnd.openxmlformats-officedocument.drawing+xml\"/>"
            ));
        }
    }
    s.push_str("</Types>");
    s
}

fn xlsx_workbook_xml(sheet_count: usize, names: &[String]) -> String {
    let mut sheets = String::new();
    for i in 0..sheet_count {
        let mut nm = String::new();
        esc(&sheet_name(names, i), &mut nm);
        sheets.push_str(&format!(
            "<sheet name=\"{nm}\" sheetId=\"{n}\" r:id=\"rId{n}\"/>",
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

fn xlsx_sheet_xml(grid: &[Vec<String>], has_drawing: bool) -> String {
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
    // The `<drawing>` reference must follow `<sheetData>` (schema order). The
    // single relationship in the sheet's rels is always rId1.
    let (r_ns, drawing) = if has_drawing {
        (
            " xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"",
            "<drawing r:id=\"rId1\"/>",
        )
    } else {
        ("", "")
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"{r_ns}>\
<sheetData>{data}</sheetData>{drawing}</worksheet>"
    )
}

/// The worksheet → drawing relationship part (`xl/worksheets/_rels/sheetK.xml.rels`).
/// `drawing_no` is the global drawing index for this sheet; the relationship id is
/// always `rId1` (one drawing per sheet).
fn xlsx_sheet_rels(drawing_no: usize) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing\" \
Target=\"../drawings/drawing{drawing_no}.xml\"/></Relationships>"
    )
}

/// A spreadsheet DrawingML part (`xl/drawings/drawingN.xml`): one
/// `<xdr:absoluteAnchor>` per shape, positioned in EMU (1 pt = 12700 EMU). Each
/// anchor holds an `<xdr:sp>` whose `<xdr:spPr>` reuses the same custom-geometry
/// (or `prstGeom rect`) + fill/line/dash helpers as the DOCX/PPTX shape exporters,
/// so fills, strokes, alpha and exact dash patterns carry through identically.
fn xlsx_drawing_xml(shapes: &[PlacedShape]) -> String {
    let mut anchors = String::new();
    for (i, s) in shapes.iter().enumerate() {
        let geom = if shape_is_rect(s) {
            "<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom>".to_string()
        } else {
            dml_cust_geom(s, s.width, s.height)
        };
        let id = i + 2; // 1 is reserved for the group shape by convention
        anchors.push_str(&format!(
            "<xdr:absoluteAnchor>\
<xdr:pos x=\"{x}\" y=\"{y}\"/><xdr:ext cx=\"{w}\" cy=\"{h}\"/>\
<xdr:sp macro=\"\" textlink=\"\"><xdr:nvSpPr>\
<xdr:cNvPr id=\"{id}\" name=\"Shape {id}\"/><xdr:cNvSpPr/></xdr:nvSpPr>\
<xdr:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm>\
{geom}{fill}{ln}</xdr:spPr>\
<xdr:txBody><a:bodyPr/><a:p/></xdr:txBody></xdr:sp>\
<xdr:clientData/></xdr:absoluteAnchor>",
            x = emu(s.x),
            y = emu(s.y),
            w = emu(s.width.max(1.0)),
            h = emu(s.height.max(1.0)),
            fill = dml_fill(s),
            ln = dml_line(s),
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<xdr:wsDr xmlns:xdr=\"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing\" \
xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">{anchors}</xdr:wsDr>"
    )
}

// ─────────────────────────────── ODS (ODF sheet) ──────────────────────────────

/// Export reconstructed tables to an OpenDocument Spreadsheet (`.ods`), one
/// `table:table` per page. The ODF counterpart of [`to_xlsx`]. Sheets are named
/// `Page <n>`; use [`to_ods_named`] for custom names.
pub fn to_ods(grids: &[Vec<Vec<String>>]) -> Vec<u8> {
    to_ods_named(grids, &[])
}

/// As [`to_ods`] but with explicit per-sheet names (index-aligned to `grids`); a
/// missing or empty name falls back to `Page <n>`.
pub fn to_ods_named(grids: &[Vec<Vec<String>>], names: &[String]) -> Vec<u8> {
    to_ods_with_shapes(grids, names, &[])
}

/// As [`to_ods_named`] but also lays out each sheet's floating vector shapes
/// (`shapes_per_sheet`, index-aligned to `grids`) inside that sheet's
/// `table:table`, mirroring the [`to_odp`] shape rendering: a `draw:rect` for an
/// axis-aligned box, a `draw:path` (absolute `svg:d` + `svg:viewBox`) otherwise,
/// each referencing an automatic `<style:style>` carrying its real fill/stroke/
/// dash via [`odf_shape_style`]. Sheets without shapes are byte-identical to the
/// plain table output.
pub fn to_ods_with_shapes(
    grids: &[Vec<Vec<String>>],
    names: &[String],
    shapes_per_sheet: &[Vec<PlacedShape>],
) -> Vec<u8> {
    let mut zip = ZipWriter::new();
    zip.add_stored(
        "mimetype",
        b"application/vnd.oasis.opendocument.spreadsheet",
    );

    // Shape paint styles go in automatic-styles; the shapes themselves inside the
    // owning table. Build both, then assemble content.xml.
    let mut auto = String::new();
    let mut body = String::new();
    let sheets = grids.len().max(1);
    let mut style_id = 0usize;
    for s in 0..sheets {
        let mut nm = String::new();
        esc(&sheet_name(names, s), &mut nm);
        body.push_str(&format!("<table:table table:name=\"{nm}\">"));
        let grid = grids.get(s).map(Vec::as_slice).unwrap_or(&[]);
        if grid.is_empty() {
            body.push_str("<table:table-row><table:table-cell/></table:table-row>");
        }
        for row in grid {
            body.push_str("<table:table-row>");
            for value in row {
                if value.is_empty() {
                    body.push_str("<table:table-cell/>");
                } else {
                    let mut text = String::new();
                    esc(value, &mut text);
                    body.push_str(&format!(
                        "<table:table-cell office:value-type=\"string\"><text:p>{text}</text:p></table:table-cell>"
                    ));
                }
            }
            body.push_str("</table:table-row>");
        }
        // Floating shapes are sheet-level drawing objects: emitted after the rows
        // but still inside the `table:table` (ODF §9.2.5 allows `draw:*` there).
        for shape in shapes_per_sheet.get(s).map(Vec::as_slice).unwrap_or(&[]) {
            let style = format!("S{style_id}");
            auto.push_str(&odf_shape_style(&style, shape));
            let (x, y) = (num(shape.x), num(shape.y));
            let (w, h) = (num(shape.width.max(1.0)), num(shape.height.max(1.0)));
            if shape_is_rect(shape) {
                body.push_str(&format!(
                    "<draw:rect draw:style-name=\"{style}\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\"/>"
                ));
            } else {
                // `svg:d` is absolute top-down points; the viewBox is offset by the
                // box origin so those coordinates map straight onto the frame.
                body.push_str(&format!(
                    "<draw:path draw:style-name=\"{style}\" \
svg:x=\"{x}pt\" svg:y=\"{y}pt\" svg:width=\"{w}pt\" svg:height=\"{h}pt\" \
svg:viewBox=\"{vbx} {vby} {vbw} {vbh}\" svg:d=\"{d}\"/>",
                    vbx = num(shape.x),
                    vby = num(shape.y),
                    vbw = num(shape.width.max(1.0)),
                    vbh = num(shape.height.max(1.0)),
                    d = odf_path_d(&shape.segments),
                ));
            }
            style_id += 1;
        }
        body.push_str("</table:table>");
    }

    // The drawing namespaces and the automatic-styles block are only emitted when
    // shapes are present, so a shape-less workbook stays byte-identical to the
    // historical plain-table output.
    let (draw_ns, styles) = if auto.is_empty() {
        ("", String::new())
    } else {
        (
            " xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" \
xmlns:svg=\"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0\"",
            format!("<office:automatic-styles>{auto}</office:automatic-styles>"),
        )
    };
    let content = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" \
xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\"{draw_ns} office:version=\"1.3\">\
{styles}\
<office:body><office:spreadsheet>{body}</office:spreadsheet></office:body></office:document-content>"
    );

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

// ─────────────────────────── XLSX reader (inverse) ────────────────────────────

/// Read an `.xlsx` workbook back into per-sheet `(name, rows)` grids — the
/// inverse of [`to_xlsx`]/[`to_xlsx_named`]. Each `rows[r][c]` is the cell text
/// (empty string for blank cells); sheets come in `<sheets>` order. Handles both
/// **inline strings** (this engine's own output) and **shared strings**
/// (`sharedStrings.xml`, as Excel and most libraries emit), plus plain numeric /
/// `str` cells. Non-xlsx or unreadable input yields an empty `Vec`. Pure std.
pub fn xlsx_to_grids(bytes: &[u8]) -> Vec<(String, Vec<Vec<String>>)> {
    let zip = crate::convert::zip::read_zip(bytes);
    let names = {
        let xml = zip
            .get("xl/workbook.xml")
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        workbook_sheet_names(&xml)
    };
    let worksheet_count = zip
        .keys()
        .filter(|k| k.starts_with("xl/worksheets/sheet") && k.ends_with(".xml"))
        .count();
    let count = names.len().max(worksheet_count);
    let shared = {
        let xml = zip
            .get("xl/sharedStrings.xml")
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        shared_strings(&xml)
    };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let name = names
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("Page {}", i + 1));
        let grid = zip
            .get(&format!("xl/worksheets/sheet{}.xml", i + 1))
            .map(|b| sheet_grid(&String::from_utf8_lossy(b), &shared))
            .unwrap_or_default();
        out.push((name, grid));
    }
    out
}

/// The value of attribute `attr` in an opening-tag fragment (`tag` excludes the
/// surrounding `<` `>`), or `None`.
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let key = format!("{attr}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

/// Sheet display names from `xl/workbook.xml`, in `<sheets>` order (unescaped).
fn workbook_sheet_names(xml: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut i = 0;
    while let Some(rel) = xml[i..].find("<sheet ") {
        let start = i + rel + 7;
        let end = xml[start..].find('>').map_or(xml.len(), |e| start + e);
        if let Some(name) = attr_value(&xml[start..end], "name") {
            names.push(crate::convert::reverse::unescape(&name));
        }
        i = end;
    }
    names
}

/// The shared-string table from `xl/sharedStrings.xml`: one entry per `<si>`,
/// concatenating its `<t>` runs (unescaped).
fn shared_strings(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = xml[i..].find("<si>") {
        let start = i + rel + 4;
        let end = xml[start..].find("</si>").map_or(xml.len(), |e| start + e);
        out.push(collect_t_runs(&xml[start..end]));
        i = end;
    }
    out
}

/// Concatenate every `<t…>text</t>` run in `s`, unescaping each.
fn collect_t_runs(s: &str) -> String {
    let mut text = String::new();
    let mut i = 0;
    while let Some(rel) = s[i..].find("<t") {
        let tag = i + rel;
        let Some(grel) = s[tag..].find('>') else {
            break;
        };
        let body_start = tag + grel + 1;
        let Some(erel) = s[body_start..].find("</t>") else {
            break;
        };
        let end = body_start + erel;
        text.push_str(&crate::convert::reverse::unescape(&s[body_start..end]));
        i = end + 4;
    }
    text
}

/// Parse a worksheet's `<sheetData>` into a dense row/column grid.
fn sheet_grid(xml: &str, shared: &[String]) -> Vec<Vec<String>> {
    let mut cells: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0;
    while let Some(rel) = xml[i..].find("<c ") {
        let cstart = i + rel;
        let Some(grel) = xml[cstart..].find('>') else {
            break;
        };
        let gt = cstart + grel;
        let open = &xml[cstart..gt];
        let self_closing = xml.as_bytes().get(gt.wrapping_sub(1)) == Some(&b'/');
        let r_attr = attr_value(open, "r").unwrap_or_default();
        let t_attr = attr_value(open, "t").unwrap_or_default();
        let text = if self_closing {
            i = gt + 1;
            String::new()
        } else {
            let body_start = gt + 1;
            let Some(erel) = xml[body_start..].find("</c>") else {
                break;
            };
            let end = body_start + erel;
            let value = decode_cell(&xml[body_start..end], &t_attr, shared);
            i = end + 4;
            value
        };
        if let (Some(row), Some(col)) = cell_ref(&r_attr) {
            cells.push((row, col, text));
        }
    }
    let Some(max_row) = cells.iter().map(|(r, _, _)| *r).max() else {
        return Vec::new();
    };
    let max_col = cells.iter().map(|(_, c, _)| *c).max().unwrap_or(0);
    let mut grid = vec![vec![String::new(); max_col + 1]; max_row + 1];
    for (r, c, t) in cells {
        grid[r][c] = t;
    }
    grid
}

/// Resolve an A1-style cell reference to `(row, col)` 0-based indices.
fn cell_ref(r: &str) -> (Option<usize>, Option<usize>) {
    let bytes = r.as_bytes();
    let mut col = 0usize;
    let mut seen = false;
    let mut idx = 0;
    while idx < bytes.len() && bytes[idx].is_ascii_alphabetic() {
        col = col * 26 + (bytes[idx].to_ascii_uppercase() - b'A' + 1) as usize;
        seen = true;
        idx += 1;
    }
    let row = r[idx..]
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1));
    (row, seen.then(|| col - 1))
}

/// The text of one `<c>` body, per its `t` attribute: inline string, shared
/// string (index into `shared`), or a plain `<v>` value.
fn decode_cell(body: &str, t: &str, shared: &[String]) -> String {
    match t {
        "inlineStr" => collect_t_runs(body),
        "s" => inner_v(body)
            .parse::<usize>()
            .ok()
            .and_then(|n| shared.get(n).cloned())
            .unwrap_or_default(),
        _ => crate::convert::reverse::unescape(&inner_v(body)),
    }
}

/// The text between the first `<v…>` and `</v>` in a cell body.
fn inner_v(body: &str) -> String {
    let Some(p) = body.find("<v") else {
        return String::new();
    };
    let after = &body[p..];
    let Some(gt) = after.find('>') else {
        return String::new();
    };
    let rest = &after[gt + 1..];
    rest.find("</v>")
        .map(|e| rest[..e].to_string())
        .unwrap_or_default()
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
            // Axis-aligned rectangle (no explicit segments → rect fallback),
            // green fill + blue stroke, to exercise the paint plumbing.
            shapes: vec![PlacedShape {
                x: 50.0,
                y: 50.0,
                width: 100.0,
                height: 80.0,
                fill: Some([0.0, 1.0, 0.0]),
                stroke: Some([0.0, 0.0, 1.0]),
                stroke_width: 2.0,
                ..PlacedShape::default()
            }],
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
                return Some(if method == 8 {
                    inflate(payload).unwrap()
                } else {
                    payload.to_vec()
                });
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
        assert!(
            content.contains("Hello &lt;World&gt; &amp; co"),
            "escaped run text present"
        );
        assert!(content.contains("draw:rect"), "shape becomes a rectangle");
        assert!(
            !content.contains("draw:image"),
            "no image when none provided"
        );
        // style fidelity
        assert!(content.contains("fo:font-weight=\"bold\""), "bold emitted");
        assert!(
            content.contains("fo:font-family=\"Helvetica\""),
            "family emitted"
        );
        assert!(content.contains("fo:color=\"#FF0000\""), "colour emitted");
    }

    #[test]
    fn docx_has_real_text_box_and_section() {
        let zip = to_docx(&sample_pages());
        let doc = String::from_utf8(entry(&zip, "word/document.xml").unwrap()).unwrap();
        assert!(doc.contains("wps:txbx"), "text is a real Word text box");
        assert!(
            doc.contains("Hello &lt;World&gt; &amp; co"),
            "escaped run text present"
        );
        assert!(
            doc.contains("<w:t xml:space=\"preserve\">"),
            "editable run, not image"
        );
        assert!(doc.contains("w:sectPr"), "page becomes a section");
        assert!(doc.contains("<w:b/>"), "bold emitted");
        assert!(
            doc.contains("w:rFonts w:ascii=\"Helvetica\""),
            "font family emitted"
        );
        assert!(
            doc.contains("<w:color w:val=\"FF0000\"/>"),
            "colour emitted"
        );
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
        // Default names are Page N.
        assert!(wb.contains("name=\"Page 1\"") && wb.contains("name=\"Page 2\""));
    }

    #[test]
    fn xlsx_and_ods_honor_custom_sheet_names() {
        let grids = vec![vec![vec!["only".to_string()]]];
        // Provided name overrides the Page <n> default and is XML-escaped.
        let names = vec!["Sheet1 & <Summary>".to_string()];
        let wb =
            String::from_utf8(entry(&to_xlsx_named(&grids, &names), "xl/workbook.xml").unwrap())
                .unwrap();
        assert!(
            wb.contains("name=\"Sheet1 &amp; &lt;Summary&gt;\""),
            "custom xlsx sheet name, escaped: {wb}"
        );
        let content =
            String::from_utf8(entry(&to_ods_named(&grids, &names), "content.xml").unwrap())
                .unwrap();
        assert!(
            content.contains("table:name=\"Sheet1 &amp; &lt;Summary&gt;\""),
            "custom ods sheet name, escaped"
        );
        // An empty name falls back to the Page <n> default.
        let wb2 = String::from_utf8(
            entry(&to_xlsx_named(&grids, &[String::new()]), "xl/workbook.xml").unwrap(),
        )
        .unwrap();
        assert!(wb2.contains("name=\"Page 1\""), "empty name → default");
    }

    #[test]
    fn xlsx_round_trips_through_grids() {
        // Write a workbook, then read it back natively — names, cells (incl.
        // escaped text) and the blank-row gap survive the round-trip.
        let grids = vec![
            vec![
                vec!["Name".to_string(), "Age".to_string()],
                vec!["Alice & <b>".to_string(), "30".to_string()],
                vec![], // blank separator row
                vec!["Bob".to_string(), "".to_string()],
            ],
            vec![vec!["café".to_string()]],
        ];
        let names = vec!["People".to_string(), "Notes".to_string()];
        let read = xlsx_to_grids(&to_xlsx_named(&grids, &names));

        assert_eq!(read.len(), 2, "two sheets");
        assert_eq!(read[0].0, "People");
        assert_eq!(read[1].0, "Notes");
        let s0 = &read[0].1;
        assert_eq!(s0[0], vec!["Name".to_string(), "Age".to_string()]);
        assert_eq!(s0[1][0], "Alice & <b>", "escaped cell text decoded");
        // The blank separator row survives as an all-empty row (dense to max col).
        assert_eq!(s0.len(), 4, "row-index gap preserves the blank row");
        assert!(s0[2].iter().all(String::is_empty), "blank row is empty");
        assert_eq!(s0[3][0], "Bob");
        assert_eq!(read[1].1[0][0], "café", "utf-8 cell preserved");
    }

    #[test]
    fn xlsx_reader_decodes_shared_strings() {
        // A hand-built workbook using a shared-string table (t="s"), as Excel and
        // most libraries emit — the reader must resolve the indices.
        use crate::convert::zip::ZipWriter;
        let mut zip = ZipWriter::new();
        zip.add_deflated(
            "xl/workbook.xml",
            b"<workbook><sheets><sheet name=\"S\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>",
        );
        zip.add_deflated(
            "xl/sharedStrings.xml",
            b"<sst><si><t>Hello</t></si><si><t>World</t></si></sst>",
        );
        zip.add_deflated(
            "xl/worksheets/sheet1.xml",
            b"<worksheet><sheetData><row r=\"1\">\
<c r=\"A1\" t=\"s\"><v>0</v></c><c r=\"B1\" t=\"s\"><v>1</v></c>\
</row></sheetData></worksheet>",
        );
        let read = xlsx_to_grids(&zip.finish());
        assert_eq!(read[0].0, "S");
        assert_eq!(read[0].1[0], vec!["Hello".to_string(), "World".to_string()]);
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
        assert!(
            slide.contains("<a:t>Hello &lt;World&gt; &amp; co</a:t>"),
            "escaped run text"
        );
        assert!(slide.contains(" b=\"1\""), "bold emitted");
        assert!(
            slide.contains("<a:latin typeface=\"Helvetica\"/>"),
            "font family emitted"
        );
        assert!(
            slide.contains("<a:srgbClr val=\"FF0000\"/>"),
            "colour emitted"
        );
        let pres = String::from_utf8(entry(&zip, "ppt/presentation.xml").unwrap()).unwrap();
        assert!(pres.contains("p:sldSz"), "slide size set from page");
        assert_eq!(pres.matches("<p:sldId ").count(), 1, "one slide per page");
    }

    #[test]
    fn to_odp_is_a_presentation_with_slides() {
        let zip = to_odp(&sample_pages());
        assert_eq!(
            entry(&zip, "mimetype").unwrap(),
            b"application/vnd.oasis.opendocument.presentation"
        );
        for part in ["content.xml", "styles.xml", "META-INF/manifest.xml"] {
            assert!(entry(&zip, part).is_some(), "missing {part}");
        }
        let content = String::from_utf8(entry(&zip, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("<office:presentation>"),
            "presentation body"
        );
        assert!(content.contains("<draw:page "), "one draw:page per slide");
        assert!(
            content.contains("<draw:text-box>"),
            "text as a positioned box"
        );
        assert!(
            content.contains("Hello &lt;World&gt; &amp; co"),
            "escaped run text"
        );
        let styles = String::from_utf8(entry(&zip, "styles.xml").unwrap()).unwrap();
        assert!(styles.contains("style:master-page"), "master page present");
    }

    // ── shape geometry + colour fidelity (PDF→Office vectors) ──────────────────

    /// A red-filled, blue-stroked axis-aligned rectangle (segments-less → rect
    /// fallback), the common frame/table-rule case.
    fn rect_shape_pages() -> Vec<ConvPage> {
        vec![ConvPage {
            width: 612.0,
            height: 792.0,
            shapes: vec![PlacedShape {
                x: 10.0,
                y: 10.0,
                width: 100.0,
                height: 60.0,
                fill: Some([1.0, 0.0, 0.0]),
                stroke: Some([0.0, 0.0, 1.0]),
                stroke_width: 1.5,
                ..PlacedShape::default()
            }],
            ..ConvPage::default()
        }]
    }

    /// A non-rectangular path (triangle whose hypotenuse is a cubic), red fill,
    /// to exercise the custom-geometry / `draw:path` branch.
    fn path_shape_pages() -> Vec<ConvPage> {
        vec![ConvPage {
            width: 612.0,
            height: 792.0,
            shapes: vec![PlacedShape {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
                segments: vec![
                    PathSeg::Move(0.0, 100.0),
                    PathSeg::Line(100.0, 100.0),
                    PathSeg::Cubic(80.0, 60.0, 40.0, 20.0, 0.0, 0.0),
                    PathSeg::Close,
                ],
                fill: Some([1.0, 0.0, 0.0]),
                stroke: Some([0.0, 0.0, 1.0]),
                stroke_width: 2.0,
                ..PlacedShape::default()
            }],
            ..ConvPage::default()
        }]
    }

    #[test]
    fn odt_rect_shape_carries_fill_and_stroke_colours() {
        let zip = to_odt(&rect_shape_pages());
        let content = String::from_utf8(entry(&zip, "content.xml").unwrap()).unwrap();
        assert!(content.contains("<draw:rect "), "rect emitted, not a path");
        assert!(
            content.contains("draw:fill=\"solid\" draw:fill-color=\"#FF0000\""),
            "red fill"
        );
        assert!(
            content.contains("svg:stroke-color=\"#0000FF\""),
            "blue stroke"
        );
        assert!(!content.contains("#808080"), "no hardcoded grey");
    }

    #[test]
    fn odt_non_rect_shape_becomes_a_path() {
        let zip = to_odt(&path_shape_pages());
        let content = String::from_utf8(entry(&zip, "content.xml").unwrap()).unwrap();
        assert!(content.contains("<draw:path "), "non-rect → draw:path");
        assert!(content.contains("svg:d=\""), "path data present");
        assert!(
            content.contains("draw:fill-color=\"#FF0000\""),
            "fill colour carried onto the path"
        );
    }

    #[test]
    fn docx_rect_shape_uses_prstgeom_with_colours() {
        let zip = to_docx(&rect_shape_pages());
        let doc = String::from_utf8(entry(&zip, "word/document.xml").unwrap()).unwrap();
        assert!(
            doc.contains("<a:prstGeom prst=\"rect\">"),
            "rect preset geom"
        );
        assert!(
            doc.contains("<a:solidFill><a:srgbClr val=\"FF0000\">"),
            "red fill"
        );
        assert!(
            doc.contains("<a:srgbClr val=\"0000FF\">"),
            "blue stroke colour"
        );
        assert!(!doc.contains("808080"), "no hardcoded grey");
    }

    #[test]
    fn docx_non_rect_shape_emits_custom_geometry() {
        let zip = to_docx(&path_shape_pages());
        let doc = String::from_utf8(entry(&zip, "word/document.xml").unwrap()).unwrap();
        assert!(doc.contains("<a:custGeom>"), "non-rect → custGeom");
        assert!(doc.contains("<a:cubicBezTo>"), "cubic segment emitted");
        assert!(doc.contains("<a:moveTo>") && doc.contains("<a:lnTo>"));
        assert!(
            doc.contains("<a:srgbClr val=\"FF0000\">"),
            "fill colour carried"
        );
    }

    #[test]
    fn pptx_rect_shape_uses_prstgeom_with_colours() {
        let zip = to_pptx(&rect_shape_pages());
        let slide = String::from_utf8(entry(&zip, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(
            slide.contains("<a:prstGeom prst=\"rect\">"),
            "rect preset geom"
        );
        assert!(
            slide.contains("<a:solidFill><a:srgbClr val=\"FF0000\">"),
            "red fill"
        );
        assert!(slide.contains("<a:srgbClr val=\"0000FF\">"), "blue stroke");
        assert!(!slide.contains("808080"), "no hardcoded grey");
    }

    #[test]
    fn pptx_non_rect_shape_emits_custom_geometry() {
        let zip = to_pptx(&path_shape_pages());
        let slide = String::from_utf8(entry(&zip, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(slide.contains("<a:custGeom>"), "non-rect → custGeom");
        assert!(slide.contains("<a:cubicBezTo>"), "cubic segment emitted");
        assert!(
            slide.contains("<a:srgbClr val=\"FF0000\">"),
            "fill colour carried"
        );
    }

    #[test]
    fn odp_shape_carries_colours() {
        let zip = to_odp(&rect_shape_pages());
        let content = String::from_utf8(entry(&zip, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("draw:fill-color=\"#FF0000\""),
            "red fill on the slide shape"
        );
        assert!(
            content.contains("svg:stroke-color=\"#0000FF\""),
            "blue stroke"
        );
        assert!(!content.contains("#808080"), "no hardcoded grey");
    }

    // ── exact dash patterns (PDF→Office P2/P3) ─────────────────────────────────

    /// A dashed-stroke rectangle: 4 pt line, dash `[6, 3]` (6 pt on / 3 pt off).
    /// Exercises the exact-pattern path (`<a:custDash>` / `<draw:stroke-dash>`).
    fn dashed_shape_pages() -> Vec<ConvPage> {
        vec![ConvPage {
            width: 612.0,
            height: 792.0,
            shapes: vec![PlacedShape {
                x: 10.0,
                y: 10.0,
                width: 100.0,
                height: 60.0,
                stroke: Some([0.0, 0.0, 0.0]),
                stroke_width: 4.0,
                dash: vec![6.0, 3.0],
                ..PlacedShape::default()
            }],
            ..ConvPage::default()
        }]
    }

    #[test]
    fn dml_dash_emits_exact_custom_pattern() {
        // dash [3,2] at width 4 → on=3/4=75000, off=2/4=50000 thousandths-%.
        let shape = PlacedShape {
            stroke: Some([0.0, 0.0, 0.0]),
            stroke_width: 4.0,
            dash: vec![3.0, 2.0],
            ..PlacedShape::default()
        };
        let ln = dml_line(&shape);
        assert!(
            ln.contains("<a:custDash>"),
            "exact dash, not a preset: {ln}"
        );
        assert!(
            ln.contains("<a:ds d=\"75000\" sp=\"50000\"/>"),
            "on/off scaled to % of width: {ln}"
        );
        assert!(!ln.contains("prstDash"), "no generic preset when scalable");
    }

    #[test]
    fn dml_dash_falls_back_to_preset_when_width_zero() {
        // No width to scale by → generic preset rather than a bogus 0-scaled dash.
        let shape = PlacedShape {
            stroke: Some([0.0, 0.0, 0.0]),
            stroke_width: 0.0,
            dash: vec![3.0, 2.0],
            ..PlacedShape::default()
        };
        let ln = dml_line(&shape);
        assert!(
            ln.contains("<a:prstDash val=\"dash\"/>"),
            "preset fallback: {ln}"
        );
        assert!(!ln.contains("custDash"));
    }

    #[test]
    fn dml_dash_pairs_odd_length_array() {
        // [4] (cyclic 4-on/4-off) at width 2 → one ds with d==sp==200000.
        let shape = PlacedShape {
            stroke: Some([0.0, 0.0, 0.0]),
            stroke_width: 2.0,
            dash: vec![4.0],
            ..PlacedShape::default()
        };
        let ln = dml_line(&shape);
        assert!(
            ln.contains("<a:ds d=\"200000\" sp=\"200000\"/>"),
            "odd array → on==off: {ln}"
        );
    }

    #[test]
    fn docx_dashed_shape_uses_custom_dash() {
        let zip = to_docx(&dashed_shape_pages());
        let doc = String::from_utf8(entry(&zip, "word/document.xml").unwrap()).unwrap();
        // dash [6,3] at width 4 → on=150000, off=75000.
        assert!(doc.contains("<a:custDash>"), "DOCX exact dash");
        assert!(
            doc.contains("<a:ds d=\"150000\" sp=\"75000\"/>"),
            "DOCX dash stops scaled: {doc}"
        );
    }

    #[test]
    fn pptx_dashed_shape_uses_custom_dash() {
        let zip = to_pptx(&dashed_shape_pages());
        let slide = String::from_utf8(entry(&zip, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(slide.contains("<a:custDash>"), "PPTX exact dash");
        assert!(slide.contains("<a:ds d=\"150000\" sp=\"75000\"/>"));
    }

    #[test]
    fn odt_dashed_shape_emits_stroke_dash_definition() {
        let zip = to_odt(&dashed_shape_pages());
        let content = String::from_utf8(entry(&zip, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("<draw:stroke-dash "),
            "ODT defines a dash style"
        );
        assert!(
            content.contains("draw:stroke=\"dash\" draw:stroke-dash=\""),
            "the shape style references the dash: {content}"
        );
        // 6 pt → 0.212 cm, 3 pt → 0.106 cm (pt/28.3465, num() rounds to 3 dp).
        assert!(
            content.contains("draw:dots1-length=\"0.212cm\""),
            "on length cm: {content}"
        );
        assert!(
            content.contains("draw:distance=\"0.106cm\""),
            "off length cm"
        );
    }

    #[test]
    fn odp_dashed_shape_emits_stroke_dash_definition() {
        let zip = to_odp(&dashed_shape_pages());
        let content = String::from_utf8(entry(&zip, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("<draw:stroke-dash "),
            "ODP defines a dash style"
        );
        assert!(content.contains("draw:stroke=\"dash\""), "stroke is dashed");
    }

    #[test]
    fn solid_stroke_shape_has_no_dash_artifacts() {
        // The non-dashed rect must stay solid: no custDash / stroke-dash anywhere.
        let docx =
            String::from_utf8(entry(&to_docx(&rect_shape_pages()), "word/document.xml").unwrap())
                .unwrap();
        assert!(!docx.contains("custDash") && !docx.contains("prstDash"));
        let odt =
            String::from_utf8(entry(&to_odt(&rect_shape_pages()), "content.xml").unwrap()).unwrap();
        assert!(!odt.contains("stroke-dash"));
        assert!(odt.contains("draw:stroke=\"solid\""), "still solid");
    }

    // ── spreadsheet shapes (XLSX/ODS drawing layer) ───────────────────────────

    /// A red-filled, blue-stroked, dashed non-rectangular path — exercises the
    /// custom-geometry + exact-dash branch in both spreadsheet exporters.
    fn sheet_shape() -> PlacedShape {
        PlacedShape {
            x: 12.0,
            y: 24.0,
            width: 80.0,
            height: 40.0,
            segments: vec![
                PathSeg::Move(12.0, 24.0),
                PathSeg::Line(92.0, 24.0),
                PathSeg::Cubic(80.0, 50.0, 40.0, 60.0, 12.0, 64.0),
                PathSeg::Close,
            ],
            fill: Some([1.0, 0.0, 0.0]),
            stroke: Some([0.0, 0.0, 1.0]),
            stroke_width: 4.0,
            dash: vec![6.0, 3.0],
            ..PlacedShape::default()
        }
    }

    #[test]
    fn xlsx_with_shapes_embeds_a_drawing_part() {
        let grids = vec![vec![vec!["A1".to_string()]]];
        let shapes = vec![vec![sheet_shape()]];
        let zip = to_xlsx_with_shapes(&grids, &[], &shapes);

        // Valid zip: the cell sheet and the drawing part are both readable.
        let s1 = String::from_utf8(entry(&zip, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(s1.contains("inlineStr"), "cells still written");
        assert!(
            s1.contains("<drawing r:id=\"rId1\"/>"),
            "worksheet references its drawing: {s1}"
        );

        let d1 = String::from_utf8(entry(&zip, "xl/drawings/drawing1.xml").unwrap()).unwrap();
        assert!(d1.contains("<xdr:wsDr "), "spreadsheet drawing root");
        assert!(d1.contains("<xdr:absoluteAnchor>"), "absolute anchor");
        // 12 pt → 152400 EMU on x; the box is positioned in EMU.
        assert!(d1.contains("<xdr:pos x=\"152400\""), "EMU position: {d1}");
        assert!(d1.contains("<a:path "), "custom path geometry present");
        assert!(d1.contains("<a:cubicBezTo>"), "cubic segment carried");
        assert!(
            d1.contains("<a:srgbClr val=\"FF0000\">"),
            "red fill colour: {d1}"
        );
        assert!(
            d1.contains("<a:srgbClr val=\"0000FF\">"),
            "blue stroke colour"
        );
        // dash [6,3] at width 4 → on=150000, off=75000 (% of width, thousandths).
        assert!(
            d1.contains("<a:custDash>"),
            "exact dash in the spreadsheet drawing"
        );
        assert!(
            d1.contains("<a:ds d=\"150000\" sp=\"75000\"/>"),
            "dash stops scaled"
        );

        // The drawing content-type override + the worksheet→drawing rel are wired.
        let ct = String::from_utf8(entry(&zip, "[Content_Types].xml").unwrap()).unwrap();
        assert!(
            ct.contains("PartName=\"/xl/drawings/drawing1.xml\"")
                && ct.contains("officedocument.drawing+xml"),
            "drawing registered in content types: {ct}"
        );
        let rels =
            String::from_utf8(entry(&zip, "xl/worksheets/_rels/sheet1.xml.rels").unwrap()).unwrap();
        assert!(
            rels.contains("Target=\"../drawings/drawing1.xml\""),
            "sheet rels point at the drawing: {rels}"
        );
    }

    #[test]
    fn xlsx_rect_shape_uses_prstgeom() {
        let grids = vec![vec![vec!["x".to_string()]]];
        let shapes = vec![vec![PlacedShape {
            x: 0.0,
            y: 0.0,
            width: 50.0,
            height: 50.0,
            fill: Some([0.0, 1.0, 0.0]),
            ..PlacedShape::default()
        }]];
        let zip = to_xlsx_with_shapes(&grids, &[], &shapes);
        let d1 = String::from_utf8(entry(&zip, "xl/drawings/drawing1.xml").unwrap()).unwrap();
        assert!(
            d1.contains("<a:prstGeom prst=\"rect\">"),
            "rect → preset geom"
        );
        assert!(
            d1.contains("<a:srgbClr val=\"00FF00\">"),
            "green fill: {d1}"
        );
    }

    #[test]
    fn xlsx_without_shapes_is_unchanged() {
        // A shape-less workbook must not gain any drawing parts/refs and stays
        // byte-identical to the plain writer.
        let grids = vec![vec![vec!["only".to_string()]]];
        let plain = to_xlsx(&grids);
        let with_empty = to_xlsx_with_shapes(&grids, &[], &[Vec::new()]);
        assert_eq!(plain, with_empty, "no shapes ⇒ identical output");
        let s1 = String::from_utf8(entry(&plain, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(!s1.contains("<drawing "), "no drawing reference");
        assert!(entry(&plain, "xl/drawings/drawing1.xml").is_none());
    }

    #[test]
    fn xlsx_shapes_only_on_sheets_that_have_them() {
        // Sheet 1 has no shapes, sheet 2 does → the only drawing is drawing1,
        // wired to sheet2 (drawings are numbered over shape-bearing sheets).
        let grids = vec![vec![vec!["one".to_string()]], vec![vec!["two".to_string()]]];
        let shapes = vec![Vec::new(), vec![sheet_shape()]];
        let zip = to_xlsx_with_shapes(&grids, &[], &shapes);
        assert!(entry(&zip, "xl/drawings/drawing1.xml").is_some());
        assert!(entry(&zip, "xl/drawings/drawing2.xml").is_none());
        let s1 = String::from_utf8(entry(&zip, "xl/worksheets/sheet1.xml").unwrap()).unwrap();
        assert!(!s1.contains("<drawing "), "sheet 1 has no drawing");
        let s2 = String::from_utf8(entry(&zip, "xl/worksheets/sheet2.xml").unwrap()).unwrap();
        assert!(
            s2.contains("<drawing r:id=\"rId1\"/>"),
            "sheet 2 has the drawing"
        );
        let rels =
            String::from_utf8(entry(&zip, "xl/worksheets/_rels/sheet2.xml.rels").unwrap()).unwrap();
        assert!(rels.contains("Target=\"../drawings/drawing1.xml\""));
    }

    #[test]
    fn ods_with_shapes_draws_path_and_colours() {
        let grids = vec![vec![vec!["A".to_string()]]];
        let shapes = vec![vec![sheet_shape()]];
        let zip = to_ods_with_shapes(&grids, &[], &shapes);
        // mimetype still first + stored (unchanged container contract).
        let nlen = u16::from_le_bytes([zip[26], zip[27]]) as usize;
        assert_eq!(&zip[30..30 + nlen], b"mimetype");

        let content = String::from_utf8(entry(&zip, "content.xml").unwrap()).unwrap();
        assert!(
            content.contains("office:value-type=\"string\""),
            "cells still written"
        );
        assert!(
            content.contains("<draw:path "),
            "non-rect shape → draw:path"
        );
        assert!(content.contains("svg:d=\""), "path data present");
        assert!(
            content.contains("draw:fill=\"solid\" draw:fill-color=\"#FF0000\""),
            "red fill carried: {content}"
        );
        assert!(
            content.contains("svg:stroke-color=\"#0000FF\""),
            "blue stroke"
        );
        assert!(content.contains("<draw:stroke-dash "), "exact dash defined");
        // The dash style must be referenced from automatic-styles, inside the sheet.
        assert!(
            content.contains("<office:automatic-styles>")
                && content.contains("draw:stroke=\"dash\""),
            "dash style wired"
        );
    }

    #[test]
    fn ods_without_shapes_is_unchanged() {
        let grids = vec![vec![vec!["only".to_string()]]];
        assert_eq!(
            to_ods(&grids),
            to_ods_with_shapes(&grids, &[], &[Vec::new()]),
            "no shapes ⇒ identical ODS output"
        );
    }

    // ── 1:1 absolute placement: exact coordinates + container validity ─────────
    //
    // These prove the export half reproduces the PDF layout: two text runs at
    // *distinct* positions plus a framed rectangle (an "encadré") land at their
    // exact coordinates — EMU for OOXML (DOCX/PPTX), points for ODF (ODT/ODP) —
    // and the produced .pptx/.odp/.docx/.odt is a valid, unzippable container
    // whose part XML is well-formed (balanced tags). This is the visual 1:1 a
    // reader (PowerPoint/Word/Impress) renders from the placed objects.

    /// One page with two text boxes at clearly different positions plus a
    /// red-filled blue-stroked rectangle (the box/frame). Coordinates picked so
    /// every EMU/point value is exact and unambiguous.
    fn two_boxes_and_rect_page() -> Vec<ConvPage> {
        vec![ConvPage {
            width: 612.0,
            height: 792.0,
            texts: vec![
                PlacedText {
                    text: "Box A".to_string(),
                    x: 72.0,
                    y: 100.0,
                    width: 144.0,
                    height: 12.0,
                    style: TextStyle::default(),
                },
                PlacedText {
                    text: "Box B".to_string(),
                    x: 300.0,
                    y: 400.0,
                    width: 180.0,
                    height: 18.0,
                    style: TextStyle::default(),
                },
            ],
            images: Vec::new(),
            shapes: vec![PlacedShape {
                x: 130.0,
                y: 560.0,
                width: 200.0,
                height: 90.0,
                fill: Some([1.0, 0.0, 0.0]),
                stroke: Some([0.0, 0.0, 1.0]),
                stroke_width: 2.0,
                ..PlacedShape::default()
            }],
        }]
    }

    /// All local-file-header entry names in our ZIP, in stored order. A non-empty
    /// list with our known parts proves the container is unzippable.
    fn zip_entry_names(zip: &[u8]) -> Vec<String> {
        let mut names = Vec::new();
        let mut i = 0;
        while i + 30 <= zip.len() && zip[i..i + 4] == [0x50, 0x4b, 0x03, 0x04] {
            let comp = u32::from_le_bytes(zip[i + 18..i + 22].try_into().unwrap()) as usize;
            let nlen = u16::from_le_bytes([zip[i + 26], zip[i + 27]]) as usize;
            let elen = u16::from_le_bytes([zip[i + 28], zip[i + 29]]) as usize;
            names.push(String::from_utf8_lossy(&zip[i + 30..i + 30 + nlen]).into_owned());
            i = i + 30 + nlen + elen + comp;
        }
        names
    }

    /// Lightweight well-formedness check: every `<tag …>`/`</tag>` pair balances
    /// (self-closing `<…/>`, the `<?xml …?>` PI and `<!-- -->` comments ignored).
    /// Enough to catch a truncated or mis-nested part without a full XML parser.
    fn xml_is_balanced(xml: &str) -> bool {
        let bytes = xml.as_bytes();
        let mut stack: Vec<String> = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'<' {
                i += 1;
                continue;
            }
            // Processing instruction or comment/declaration: skip to '>'.
            if xml[i..].starts_with("<?") || xml[i..].starts_with("<!") {
                match xml[i..].find('>') {
                    Some(rel) => {
                        i += rel + 1;
                        continue;
                    }
                    None => return false,
                }
            }
            let Some(rel) = xml[i..].find('>') else {
                return false;
            };
            let inner = &xml[i + 1..i + rel];
            i += rel + 1;
            if let Some(name) = inner.strip_prefix('/') {
                // Closing tag: must match the top of the stack.
                let name = name.trim();
                match stack.pop() {
                    Some(open) if open == name => {}
                    _ => return false,
                }
            } else if inner.ends_with('/') {
                // Self-closing element: no stack change.
            } else {
                // Opening tag: push the element name (up to first whitespace).
                let name = inner.split_whitespace().next().unwrap_or("").to_string();
                if name.is_empty() {
                    return false;
                }
                stack.push(name);
            }
        }
        stack.is_empty()
    }

    #[test]
    fn xml_balance_checker_is_sound() {
        assert!(xml_is_balanced("<?xml version=\"1.0\"?><a><b/><c>x</c></a>"));
        assert!(xml_is_balanced("<ns:a xmlns:ns=\"u\"><ns:b/></ns:a>"));
        assert!(!xml_is_balanced("<a><b></a>"), "mis-nested");
        assert!(!xml_is_balanced("<a><b>"), "unclosed");
        assert!(!xml_is_balanced("<a></a></b>"), "extra close");
    }

    #[test]
    fn pptx_places_two_boxes_and_a_rect_at_exact_emu() {
        let zip = to_pptx(&two_boxes_and_rect_page());

        // Container is a valid, unzippable .pptx with the slide part present.
        let names = zip_entry_names(&zip);
        assert!(
            names.iter().any(|n| n == "ppt/slides/slide1.xml"),
            "slide part present in a readable zip: {names:?}"
        );

        let slide = String::from_utf8(entry(&zip, "ppt/slides/slide1.xml").unwrap()).unwrap();
        assert!(xml_is_balanced(&slide), "slide XML is well-formed");

        // 1 pt = 12700 EMU. Box A at (72,100), Box B at (300,400): distinct
        // a:off — proof the two boxes are NOT stacked at the origin.
        assert!(
            slide.contains("<a:off x=\"914400\" y=\"1270000\"/>"),
            "Box A at exact EMU (72pt,100pt): {slide}"
        );
        assert!(
            slide.contains("<a:off x=\"3810000\" y=\"5080000\"/>"),
            "Box B at exact EMU (300pt,400pt) — distinct from Box A"
        );
        // Box extents are exact too (144×12 and 180×18 pt).
        assert!(
            slide.contains("<a:ext cx=\"1828800\" cy=\"152400\"/>"),
            "Box A extent 144×12 pt"
        );
        assert!(
            slide.contains("<a:ext cx=\"2286000\" cy=\"228600\"/>"),
            "Box B extent 180×18 pt"
        );
        // The framed rectangle (encadré) at (130,560), 200×90 pt, as a real
        // rect shape carrying its fill + stroke colours.
        assert!(
            slide.contains("<a:off x=\"1651000\" y=\"7112000\"/>"),
            "rectangle at exact EMU (130pt,560pt)"
        );
        assert!(
            slide.contains("<a:ext cx=\"2540000\" cy=\"1143000\"/>"),
            "rectangle extent 200×90 pt"
        );
        assert!(
            slide.contains("<a:prstGeom prst=\"rect\">"),
            "encadré is a real rectangle shape"
        );
        assert!(
            slide.contains("<a:srgbClr val=\"FF0000\">")
                && slide.contains("<a:srgbClr val=\"0000FF\">"),
            "rectangle keeps its red fill + blue stroke"
        );
        // Both runs are real, editable text.
        assert!(slide.contains("<a:t>Box A</a:t>") && slide.contains("<a:t>Box B</a:t>"));
    }

    #[test]
    fn odp_places_two_boxes_and_a_rect_at_exact_points() {
        let zip = to_odp(&two_boxes_and_rect_page());

        // Valid .odp: mimetype is the first, stored entry; content part present.
        let names = zip_entry_names(&zip);
        assert_eq!(names.first().map(String::as_str), Some("mimetype"));
        assert!(names.iter().any(|n| n == "content.xml"), "content present");

        let content = String::from_utf8(entry(&zip, "content.xml").unwrap()).unwrap();
        assert!(xml_is_balanced(&content), "ODP content XML is well-formed");

        // ODF places frames in points via num(): integers stay integral.
        // Box A at (72,100), Box B at (300,400) — distinct svg:x/svg:y.
        assert!(
            content.contains("svg:x=\"72pt\" svg:y=\"100pt\" svg:width=\"144pt\" svg:height=\"12pt\""),
            "Box A frame at exact points: {content}"
        );
        assert!(
            content.contains("svg:x=\"300pt\" svg:y=\"400pt\" svg:width=\"180pt\" svg:height=\"18pt\""),
            "Box B frame at exact points — distinct from Box A"
        );
        // The framed rectangle at (130,560), 200×90 pt — a real draw:rect with
        // its fill + stroke colours.
        assert!(
            content.contains("<draw:rect "),
            "encadré is a real ODF rectangle"
        );
        assert!(
            content.contains("svg:x=\"130pt\" svg:y=\"560pt\" svg:width=\"200pt\" svg:height=\"90pt\""),
            "rectangle at exact points"
        );
        assert!(
            content.contains("draw:fill-color=\"#FF0000\"")
                && content.contains("svg:stroke-color=\"#0000FF\""),
            "rectangle keeps its red fill + blue stroke"
        );
        assert!(
            content.contains("Box A") && content.contains("Box B"),
            "both runs present as text boxes"
        );
    }

    #[test]
    fn docx_anchors_two_boxes_and_a_rect_at_exact_emu() {
        let zip = to_docx(&two_boxes_and_rect_page());
        let doc = String::from_utf8(entry(&zip, "word/document.xml").unwrap()).unwrap();
        assert!(xml_is_balanced(&doc), "document XML is well-formed");

        // Word anchors carry the page-relative offset in EMU on posOffset.
        assert!(
            doc.contains("<wp:posOffset>914400</wp:posOffset>")
                && doc.contains("<wp:posOffset>1270000</wp:posOffset>"),
            "Box A anchored at exact EMU (72pt,100pt): {doc}"
        );
        assert!(
            doc.contains("<wp:posOffset>3810000</wp:posOffset>")
                && doc.contains("<wp:posOffset>5080000</wp:posOffset>"),
            "Box B anchored at exact EMU (300pt,400pt)"
        );
        // The framed rectangle anchored at (130,560) pt with its colours.
        assert!(
            doc.contains("<wp:posOffset>1651000</wp:posOffset>")
                && doc.contains("<wp:posOffset>7112000</wp:posOffset>"),
            "rectangle anchored at exact EMU (130pt,560pt)"
        );
        assert!(
            doc.contains("<a:srgbClr val=\"FF0000\">")
                && doc.contains("<a:srgbClr val=\"0000FF\">"),
            "rectangle keeps red fill + blue stroke"
        );
    }

    #[test]
    fn odt_frames_two_boxes_and_a_rect_at_exact_points() {
        let zip = to_odt(&two_boxes_and_rect_page());
        let content = String::from_utf8(entry(&zip, "content.xml").unwrap()).unwrap();
        assert!(xml_is_balanced(&content), "ODT content XML is well-formed");

        assert!(
            content.contains("svg:x=\"72pt\" svg:y=\"100pt\" svg:width=\"144pt\" svg:height=\"12pt\""),
            "Box A frame at exact points: {content}"
        );
        assert!(
            content.contains("svg:x=\"300pt\" svg:y=\"400pt\" svg:width=\"180pt\" svg:height=\"18pt\""),
            "Box B frame at exact points"
        );
        assert!(
            content.contains("<draw:rect ")
                && content.contains("svg:x=\"130pt\" svg:y=\"560pt\" svg:width=\"200pt\" svg:height=\"90pt\""),
            "encadré is a real ODF rectangle at exact points"
        );
        assert!(
            content.contains("draw:fill-color=\"#FF0000\"")
                && content.contains("svg:stroke-color=\"#0000FF\""),
            "rectangle keeps red fill + blue stroke"
        );
    }
}
