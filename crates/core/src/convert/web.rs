//! HTML export — standalone, self-contained, zero-dependency.
//!
//! Each page becomes a sized `<div>`; text runs are absolutely-positioned
//! `<span>`s carrying the recovered font/weight/style/colour, images are
//! inlined as `data:` URIs, and shapes keep their vector geometry as inline
//! `<svg>`. Real, selectable content — not a page raster.

use super::{base64, ConvPage};

fn esc(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

fn n(value: f64) -> String {
    crate::content::num(value)
}

/// Convert pages to a standalone HTML document.
pub fn to_html(pages: &[ConvPage]) -> String {
    let mut html = String::from(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<style>body{background:#eee;margin:0}\
.page{position:relative;margin:8px auto;background:#fff;box-shadow:0 0 4px rgba(0,0,0,.3)}\
.t{position:absolute;white-space:pre}\
.s{position:absolute;border:.5pt solid #808080}\
img{position:absolute}</style></head><body>",
    );
    for page in pages {
        html.push_str(&format!(
            "<div class=\"page\" style=\"width:{}pt;height:{}pt\">",
            n(page.width),
            n(page.height)
        ));

        // Images first (background), then shapes, then text on top.
        for img in &page.images {
            html.push_str(&format!(
                "<img style=\"left:{}pt;top:{}pt;width:{}pt;height:{}pt\" src=\"data:image/png;base64,{}\"/>",
                n(img.x),
                n(img.y),
                n(img.width.max(1.0)),
                n(img.height.max(1.0)),
                base64(&img.png),
            ));
        }
        for s in &page.shapes {
            html.push_str(&format!(
                "<div class=\"s\" style=\"left:{}pt;top:{}pt;width:{}pt;height:{}pt\"></div>",
                n(s.x),
                n(s.y),
                n(s.width.max(1.0)),
                n(s.height.max(1.0)),
            ));
        }
        for t in &page.texts {
            let st = &t.style;
            let mut style = format!(
                "left:{}pt;top:{}pt;font-size:{}pt",
                n(t.x),
                n(t.y),
                n(t.height.max(1.0))
            );
            if !st.family.is_empty() {
                let mut fam = String::new();
                esc(&st.family, &mut fam);
                style.push_str(&format!(";font-family:'{}',{}", fam, st.generic.css()));
            } else {
                style.push_str(&format!(";font-family:{}", st.generic.css()));
            }
            if st.bold {
                style.push_str(";font-weight:bold");
            }
            if st.italic {
                style.push_str(";font-style:italic");
            }
            if st.has_visible_color() {
                style.push_str(&format!(";color:#{}", st.hex_color()));
            }
            html.push_str(&format!("<span class=\"t\" style=\"{style}\">"));
            esc(&t.text, &mut html);
            html.push_str("</span>");
        }
        html.push_str("</div>");
    }
    html.push_str("</body></html>");
    html
}

// ───────────────────────────── model → semantic HTML ─────────────────────────────

use crate::content::vector::PathSeg;
use crate::model::CellValue;
use crate::model::{
    Align, Block, BlockKind, Cell, CellVAlign, CharStyle, CodeBlock, Document, ImageRef, Inline,
    LinkTarget, List, ListMarker, Paragraph, Shape, Sheet, SheetBlock, Slide, SlideBlock, Table,
};

/// Convert a unified [`Document`] to **semantic, reflowable** HTML: headings
/// become `<h1>`..`<h6>`, paragraphs `<p>`, lists `<ul>`/`<ol>` with `<li>`,
/// tables real `<table>`/`<tr>`/`<td>` (with `colspan`/`rowspan`), inline runs
/// styled `<span>`s, links `<a href>`, and images `<img>` data-URIs. Unlike
/// [`to_html`] (absolutely-positioned PDF text boxes) this is flowing markup an
/// author could edit.
pub fn html_from_model(doc: &Document) -> String {
    let mut html = String::from("<!DOCTYPE html><html><head><meta charset=\"utf-8\">");
    if let Some(lang) = &doc.meta.lang {
        // Re-open the <html> tag with the language; simplest is to inject it.
        html = html.replace("<html>", &format!("<html lang=\"{}\">", attr_esc(lang)));
    }
    if let Some(title) = &doc.meta.title {
        let mut t = String::new();
        esc(title, &mut t);
        html.push_str(&format!("<title>{t}</title>"));
    }
    html.push_str(
        "<style>body{font-family:sans-serif;max-width:50em;margin:2em auto;padding:0 1em}\
table{border-collapse:collapse}td,th{border:1px solid #ccc;padding:.25em .5em}\
img{max-width:100%}\
pre{font-family:monospace;background:#f2f2f2;border:1px solid #ccc;padding:.5em;white-space:pre;overflow:auto}\
blockquote{margin:0 0 0 1em;padding-left:.75em;border-left:3px solid #ccc;color:#555}\
hr{border:0;border-top:1px solid #999}</style></head><body>",
    );
    for section in &doc.sections {
        if let Some(header) = &section.header {
            html.push_str("<header>");
            html_blocks(header, doc, &mut html);
            html.push_str("</header>");
        }
        for page in &section.pages {
            html_blocks(&page.blocks, doc, &mut html);
        }
        if let Some(footer) = &section.footer {
            html.push_str("<footer>");
            html_blocks(footer, doc, &mut html);
            html.push_str("</footer>");
        }
    }
    html.push_str("</body></html>");
    html
}

/// Escape a string for use inside a double-quoted HTML attribute.
fn attr_esc(s: &str) -> String {
    let mut out = String::new();
    esc(s, &mut out);
    out
}

fn html_blocks(blocks: &[Block], doc: &Document, out: &mut String) {
    for b in blocks {
        html_block(b, doc, out);
    }
}

fn html_block(block: &Block, doc: &Document, out: &mut String) {
    match &block.kind {
        BlockKind::Paragraph(p) => {
            out.push_str(&format!("<p{}>", para_style_attr(p)));
            html_inlines(&p.runs, doc, out);
            out.push_str("</p>");
        }
        BlockKind::Heading(h) => {
            let lvl = h.level.clamp(1, 6);
            out.push_str(&format!("<h{lvl}{}>", para_style_attr(&h.para)));
            html_inlines(&h.para.runs, doc, out);
            out.push_str(&format!("</h{lvl}>"));
        }
        BlockKind::List(list) => html_list(list, doc, out),
        BlockKind::Table(table) => html_table(table, doc, out),
        BlockKind::Image(img) => html_image(img, doc, out),
        BlockKind::Shape(shape) => html_shape(shape, out),
        BlockKind::TextBox(tb) => {
            out.push_str("<div>");
            html_blocks(&tb.blocks, doc, out);
            out.push_str("</div>");
        }
        BlockKind::CodeBlock(cb) => html_code(cb, out),
        BlockKind::Blockquote(bq) => {
            out.push_str("<blockquote>");
            html_blocks(&bq.blocks, doc, out);
            out.push_str("</blockquote>");
        }
        BlockKind::HorizontalRule => out.push_str("<hr/>"),
        BlockKind::Sheet(sb) => html_sheet(sb, out),
        BlockKind::Slide(sb) => html_slides(sb, doc, out),
    }
}

/// A code block → `<pre><code>` (preformatted, monospaced via the document
/// stylesheet), with an optional `language-*` class from the fence info-string.
fn html_code(cb: &CodeBlock, out: &mut String) {
    let class = match &cb.lang {
        Some(l) if !l.trim().is_empty() => format!(" class=\"language-{}\"", attr_esc(l.trim())),
        _ => String::new(),
    };
    out.push_str(&format!("<pre><code{class}>"));
    esc(&cb.code, out);
    out.push_str("</code></pre>");
}

/// A `style="…"` attribute for a paragraph's alignment (only when not the
/// default left), else empty.
fn para_style_attr(p: &Paragraph) -> String {
    match p.style.align {
        Align::Left => String::new(),
        Align::Center => " style=\"text-align:center\"".to_string(),
        Align::Right => " style=\"text-align:right\"".to_string(),
        Align::Justify => " style=\"text-align:justify\"".to_string(),
    }
}

fn html_inlines(runs: &[Inline], doc: &Document, out: &mut String) {
    for r in runs {
        match r {
            Inline::Run(run) => {
                if run.text.is_empty() {
                    continue;
                }
                let style = char_style_css(&run.style);
                if style.is_empty() {
                    esc(&run.text, out);
                } else {
                    out.push_str(&format!("<span style=\"{style}\">"));
                    esc(&run.text, out);
                    out.push_str("</span>");
                }
            }
            Inline::LineBreak => out.push_str("<br/>"),
            Inline::Image(img) => html_image_inline(img, doc, out),
            Inline::Link { href, children } => {
                let target = match href {
                    LinkTarget::Url(u) => attr_esc(u),
                    LinkTarget::Page(p) => format!("#page{p}"),
                };
                out.push_str(&format!("<a href=\"{target}\">"));
                html_inlines(children, doc, out);
                out.push_str("</a>");
            }
        }
    }
}

/// Inline CSS for a [`CharStyle`] (font/size/weight/style/decoration/colour).
fn char_style_css(style: &CharStyle) -> String {
    let mut css = String::new();
    if !style.family.is_empty() {
        css.push_str(&format!(
            "font-family:'{}',{}",
            attr_esc(&style.family),
            style.generic.css()
        ));
    }
    if style.size_pt > 0.0 {
        css.push_str(&format!(";font-size:{}pt", n(style.size_pt)));
    }
    if style.bold {
        css.push_str(";font-weight:bold");
    }
    if style.italic {
        css.push_str(";font-style:italic");
    }
    let mut decos = Vec::new();
    if style.underline {
        decos.push("underline");
    }
    if style.strike {
        decos.push("line-through");
    }
    if !decos.is_empty() {
        css.push_str(&format!(";text-decoration:{}", decos.join(" ")));
    }
    if let Some([r, g, b]) = style.color {
        if r > 0.02 || g > 0.02 || b > 0.02 {
            let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            css.push_str(&format!(";color:#{:02X}{:02X}{:02X}", q(r), q(g), q(b)));
        }
    }
    // Run highlight / background (`w:highlight`/`w:shd`/`fo:background-color`):
    // emit `background-color` so the HTML→PDF engine paints a filled rectangle
    // behind the run. Any colour is honoured (a dark highlight is legitimate),
    // unlike the near-black guard on the text colour above. `None` ⇒ nothing.
    if let Some([r, g, b]) = style.background {
        let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        css.push_str(&format!(
            ";background-color:#{:02X}{:02X}{:02X}",
            q(r),
            q(g),
            q(b)
        ));
    }
    css.trim_start_matches(';').to_string()
}

fn html_list(list: &List, doc: &Document, out: &mut String) {
    let tag = if list.ordered { "ol" } else { "ul" };
    let type_attr = if list.ordered {
        match list.marker {
            ListMarker::LowerAlpha => " type=\"a\"",
            ListMarker::UpperAlpha => " type=\"A\"",
            ListMarker::LowerRoman => " type=\"i\"",
            ListMarker::UpperRoman => " type=\"I\"",
            _ => "",
        }
    } else {
        ""
    };
    out.push_str(&format!("<{tag}{type_attr}>"));
    for item in &list.items {
        out.push_str("<li>");
        html_blocks(&item.blocks, doc, out);
        out.push_str("</li>");
    }
    out.push_str(&format!("</{tag}>"));
}

fn html_table(table: &Table, doc: &Document, out: &mut String) {
    out.push_str("<table>");
    if !table.col_widths.is_empty() {
        out.push_str("<colgroup>");
        for w in &table.col_widths {
            if *w > 0.0 {
                out.push_str(&format!("<col style=\"width:{}pt\"/>", n(*w)));
            } else {
                out.push_str("<col/>");
            }
        }
        out.push_str("</colgroup>");
    }
    for row in &table.rows {
        out.push_str("<tr>");
        for cell in &row.cells {
            html_cell(cell, doc, out);
        }
        out.push_str("</tr>");
    }
    out.push_str("</table>");
}

fn html_cell(cell: &Cell, doc: &Document, out: &mut String) {
    let mut attrs = String::new();
    if cell.col_span > 1 {
        attrs.push_str(&format!(" colspan=\"{}\"", cell.col_span));
    }
    if cell.row_span > 1 {
        attrs.push_str(&format!(" rowspan=\"{}\"", cell.row_span));
    }
    // Accumulate inline-style declarations (shading, vertical alignment) into one
    // `style` attribute; the HTML layout engine honours `vertical-align` on cells.
    let mut style = String::new();
    if let Some([r, g, b]) = cell.shading {
        let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        style.push_str(&format!(
            "background-color:#{:02X}{:02X}{:02X};",
            q(r),
            q(g),
            q(b)
        ));
    }
    if let Some(va) = cell.vertical_align {
        let v = match va {
            CellVAlign::Top => "top",
            CellVAlign::Middle => "middle",
            CellVAlign::Bottom => "bottom",
        };
        style.push_str(&format!("vertical-align:{v};"));
    }
    if !style.is_empty() {
        attrs.push_str(&format!(" style=\"{style}\""));
    }
    out.push_str(&format!("<td{attrs}>"));
    html_blocks(&cell.blocks, doc, out);
    out.push_str("</td>");
}

fn html_image(img: &ImageRef, doc: &Document, out: &mut String) {
    out.push_str("<p>");
    html_image_inline(img, doc, out);
    out.push_str("</p>");
}

fn html_image_inline(img: &ImageRef, doc: &Document, out: &mut String) {
    let Some(res) = doc.resources.images.get(&img.resource) else {
        return;
    };
    let mime = match res.format.as_str() {
        "jpeg" | "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    };
    let alt = attr_esc(img.alt.as_deref().unwrap_or(""));
    out.push_str(&format!(
        "<img alt=\"{alt}\" src=\"data:{mime};base64,{}\"/>",
        base64(&res.bytes)
    ));
}

fn html_shape(shape: &Shape, out: &mut String) {
    // Preserve the vector geometry as a self-contained inline `<svg>`: the path's
    // bounds give a `viewBox` (and `width`/`height` in points so it scales in
    // reflow), the segments become the `d` attribute, and the shape's paint maps
    // to `fill`/`stroke`/`stroke-width`/`stroke-dasharray`. PDF geometry is in
    // user space (origin bottom-left, Y up); SVG is top-left/Y down, so points
    // are translated to the bounds origin and flipped vertically.
    let Some((min_x, min_y, max_x, max_y)) = shape_bounds(&shape.segments) else {
        // No drawable geometry (empty path or a single point): fall back to a
        // tiny bordered box so the shape is acknowledged rather than dropped.
        out.push_str(&shape_placeholder(shape));
        return;
    };
    let width = max_x - min_x;
    let height = max_y - min_y;

    let mut d = String::new();
    // (x, y) in PDF user space → (x - min_x, max_y - y) in SVG space.
    let pt = |x: f64, y: f64| format!("{} {}", n(x - min_x), n(max_y - y));
    for seg in &shape.segments {
        match *seg {
            PathSeg::Move(x, y) => {
                d.push_str(&format!("M{} ", pt(x, y)));
            }
            PathSeg::Line(x, y) => {
                d.push_str(&format!("L{} ", pt(x, y)));
            }
            PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => {
                d.push_str(&format!("C{} {} {} ", pt(x1, y1), pt(x2, y2), pt(x3, y3)));
            }
            PathSeg::Close => d.push_str("Z "),
        }
    }
    let d = d.trim_end();

    let mut paint = format!(" fill=\"{}\"", svg_fill(shape.fill));
    if let Some(stroke) = shape.stroke {
        paint.push_str(&format!(" stroke=\"{}\"", rgb_hex(stroke)));
        if shape.stroke_width > 0.0 {
            paint.push_str(&format!(" stroke-width=\"{}\"", n(shape.stroke_width)));
        }
        if !shape.dash.is_empty() {
            let dashes: Vec<String> = shape.dash.iter().map(|v| n(*v)).collect();
            paint.push_str(&format!(" stroke-dasharray=\"{}\"", dashes.join(",")));
        }
    }

    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" \
viewBox=\"0 0 {vw} {vh}\" width=\"{vw}pt\" height=\"{vh}pt\" \
style=\"display:inline-block\"><path d=\"{d}\"{paint}/></svg>",
        vw = n(width.max(0.0)),
        vh = n(height.max(0.0)),
    ));
}

/// Axis-aligned bounding box `(min_x, min_y, max_x, max_y)` over every point of a
/// path (Bézier control points included). `None` when the path has no points or
/// is a single degenerate point (zero width *and* height) — neither yields a
/// renderable `<svg>` viewBox, so the caller falls back to a placeholder.
fn shape_bounds(segments: &[PathSeg]) -> Option<(f64, f64, f64, f64)> {
    let mut bounds: Option<(f64, f64, f64, f64)> = None;
    let mut add = |x: f64, y: f64| match &mut bounds {
        Some((min_x, min_y, max_x, max_y)) => {
            *min_x = min_x.min(x);
            *min_y = min_y.min(y);
            *max_x = max_x.max(x);
            *max_y = max_y.max(y);
        }
        None => bounds = Some((x, y, x, y)),
    };
    for seg in segments {
        match *seg {
            PathSeg::Move(x, y) | PathSeg::Line(x, y) => add(x, y),
            PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => {
                add(x1, y1);
                add(x2, y2);
                add(x3, y3);
            }
            PathSeg::Close => {}
        }
    }
    match bounds {
        Some((min_x, min_y, max_x, max_y)) if max_x > min_x || max_y > min_y => {
            Some((min_x, min_y, max_x, max_y))
        }
        _ => None,
    }
}

/// The `fill` attribute value: the shape's fill colour as `#RRGGBB`, or `none`
/// for a stroke-only (unfilled) shape so the path isn't filled black by default.
fn svg_fill(fill: Option<[f64; 3]>) -> String {
    match fill {
        Some(rgb) => rgb_hex(rgb),
        None => "none".to_string(),
    }
}

/// Format an RGB colour (components `0.0..=1.0`) as a `#RRGGBB` hex string.
fn rgb_hex([r, g, b]: [f64; 3]) -> String {
    let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02X}{:02X}{:02X}", q(r), q(g), q(b))
}

/// Fallback for a shape with no drawable geometry: a tiny bordered box carrying
/// the fill colour, so the shape is acknowledged rather than silently dropped.
fn shape_placeholder(shape: &Shape) -> String {
    let mut style = String::from("display:inline-block;width:1em;height:1em;border:1px solid #888");
    if let Some(rgb) = shape.fill {
        style.push_str(&format!(";background:{}", rgb_hex(rgb)));
    }
    format!("<span style=\"{style}\"></span>")
}

fn html_sheet(sheet: &SheetBlock, out: &mut String) {
    for s in &sheet.sheets {
        html_sheet_table(s, out);
    }
}

fn html_sheet_table(sheet: &Sheet, out: &mut String) {
    out.push_str("<table>");
    for row in &sheet.rows {
        out.push_str("<tr>");
        for cell in &row.cells {
            let mut style = String::new();
            if let Some([r, g, b]) = cell.fill {
                let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
                style = format!(
                    " style=\"background-color:#{:02X}{:02X}{:02X}\"",
                    q(r),
                    q(g),
                    q(b)
                );
            }
            out.push_str(&format!("<td{style}>"));
            let text = match &cell.value {
                CellValue::Empty => String::new(),
                CellValue::Text(t) => t.clone(),
                CellValue::Number(num) => crate::content::num(*num),
                CellValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
            };
            esc(&text, out);
            out.push_str("</td>");
        }
        out.push_str("</tr>");
    }
    out.push_str("</table>");
}

fn html_slides(slides: &SlideBlock, doc: &Document, out: &mut String) {
    for slide in &slides.slides {
        html_slide(slide, doc, out);
    }
}

fn html_slide(slide: &Slide, doc: &Document, out: &mut String) {
    out.push_str("<section>");
    // Slide background fill (`Slide::background`): a full-coverage,
    // absolutely-positioned backdrop `<div>` emitted first so it paints behind the
    // slide content. Sized to the slide geometry; the engine clips any overflow.
    if let Some([r, g, b]) = slide.background {
        let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        out.push_str(&format!(
            "<div style=\"position:absolute;left:0;top:0;width:{:.0}pt;height:{:.0}pt;background:#{:02X}{:02X}{:02X}\"></div>",
            slide.geometry.width,
            slide.geometry.height,
            q(r),
            q(g),
            q(b)
        ));
    }
    for ph in &slide.placeholders {
        html_block(&ph.block, doc, out);
    }
    for sh in &slide.shapes {
        html_block(sh, doc, out);
    }
    out.push_str("</section>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{Generic, PlacedImage, PlacedText, TextStyle};

    #[test]
    fn html_carries_styled_text_and_inline_image() {
        let pages = vec![ConvPage {
            width: 300.0,
            height: 400.0,
            texts: vec![PlacedText {
                text: "Bold & red".to_string(),
                x: 10.0,
                y: 20.0,
                width: 100.0,
                height: 12.0,
                style: TextStyle {
                    family: "Helvetica".to_string(),
                    generic: Generic::Sans,
                    bold: true,
                    italic: false,
                    color: Some([1.0, 0.0, 0.0]),
                    background: None,
                },
            }],
            images: vec![PlacedImage {
                png: vec![0x89, 0x50, 0x4e, 0x47],
                x: 0.0,
                y: 0.0,
                width: 50.0,
                height: 50.0,
            }],
            shapes: Vec::new(),
        }];
        let html = to_html(&pages);
        assert!(html.starts_with("<!DOCTYPE html"));
        assert!(html.contains("Bold &amp; red"), "escaped text");
        assert!(html.contains("font-weight:bold"));
        assert!(html.contains("color:#FF0000"));
        assert!(html.contains("font-family:'Helvetica',sans-serif"));
        assert!(
            html.contains("data:image/png;base64,iVBORw=="),
            "image inlined as data URI"
        );
    }

    #[test]
    fn char_style_css_emits_run_background_color() {
        // A run with a highlight emits `background-color` (any colour, including
        // dark) so the HTML→PDF engine paints it behind the glyphs.
        let lit = char_style_css(&CharStyle {
            background: Some([1.0, 1.0, 0.0]),
            ..CharStyle::default()
        });
        assert!(
            lit.contains("background-color:#FFFF00"),
            "yellow highlight in inline CSS: {lit}"
        );

        // A run without a background emits no `background-color` declaration.
        let plain = char_style_css(&CharStyle {
            color: Some([1.0, 0.0, 0.0]),
            ..CharStyle::default()
        });
        assert!(
            !plain.contains("background-color"),
            "plain run carries no background: {plain}"
        );
    }

    #[test]
    fn html_from_model_renders_run_highlight() {
        use crate::model::{InlineRun, Page, Paragraph, Section};
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Paragraph(Paragraph {
                            runs: vec![Inline::Run(InlineRun {
                                text: "lit".to_string(),
                                style: CharStyle {
                                    background: Some([1.0, 1.0, 0.0]),
                                    ..CharStyle::default()
                                },
                                source_index: None,
                            })],
                            ..Paragraph::default()
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        };
        let html = html_from_model(&doc);
        assert!(
            html.contains("background-color:#FFFF00") && html.contains("lit"),
            "the highlighted run is emitted with its background: {html}"
        );
    }

    #[test]
    fn html_from_model_paints_slide_background() {
        // A `Slide::background` (#51) becomes a full-coverage absolutely-positioned
        // backdrop `<div>` inside the slide `<section>`, so a coloured deck no
        // longer renders white. Slide geometry 720×540pt.
        use crate::model::{Page, PageGeometry, Section, Slide, SlideBlock};
        let slide = Slide {
            geometry: PageGeometry {
                width: 720.0,
                height: 540.0,
                ..PageGeometry::default()
            },
            background: Some([0.125, 0.305, 0.474]), // ≈ #204E79
            ..Slide::default()
        };
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Slide(SlideBlock {
                            slides: vec![slide],
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        };
        let html = html_from_model(&doc);
        assert!(
            html.contains("position:absolute"),
            "backdrop positioned: {html}"
        );
        assert!(
            html.contains("width:720pt") && html.contains("height:540pt"),
            "covers slide: {html}"
        );
        assert!(
            html.contains("background:#204E79"),
            "fill colour painted: {html}"
        );
    }

    /// Wrap a single [`Shape`] in a one-page document and render to HTML.
    fn html_for_shape(shape: Shape) -> String {
        use crate::model::{Page, Section};
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::Shape(shape),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        };
        html_from_model(&doc)
    }

    #[test]
    fn html_shape_renders_filled_stroked_path_as_inline_svg() {
        // A filled + stroked rectangle (10,20)-(110,70) in PDF user space → an
        // inline <svg> with a real viewBox + geometry, NOT a 1em bordered box.
        let shape = Shape {
            segments: vec![
                PathSeg::Move(10.0, 20.0),
                PathSeg::Line(110.0, 20.0),
                PathSeg::Line(110.0, 70.0),
                PathSeg::Line(10.0, 70.0),
                PathSeg::Close,
            ],
            fill: Some([1.0, 0.0, 0.0]),
            stroke: Some([0.0, 0.0, 1.0]),
            stroke_width: 2.0,
            dash: Vec::new(),
        };
        let html = html_for_shape(shape);
        assert!(html.contains("<svg "), "inline svg emitted: {html}");
        // 100pt wide × 50pt tall, scalable.
        assert!(
            html.contains("viewBox=\"0 0 100 50\"")
                && html.contains("width=\"100pt\"")
                && html.contains("height=\"50pt\""),
            "viewBox + size from bounds: {html}"
        );
        // Geometry preserved (Y flipped: top of box at y=70 → SVG y=0).
        assert!(
            html.contains("<path d=\"M0 50 L100 50 L100 0 L0 0 Z\""),
            "path geometry preserved: {html}"
        );
        assert!(html.contains("fill=\"#FF0000\""), "fill colour: {html}");
        assert!(
            html.contains("stroke=\"#0000FF\"") && html.contains("stroke-width=\"2\""),
            "stroke colour + width: {html}"
        );
        assert!(
            !html.contains("width:1em") && !html.contains("border:1px solid #888"),
            "no longer a 1em bordered placeholder box: {html}"
        );
    }

    #[test]
    fn html_shape_stroke_only_path_has_fill_none() {
        // A stroke-only path (no fill) must render `fill="none"`, else the path
        // would be filled black by the SVG default.
        let shape = Shape {
            segments: vec![PathSeg::Move(0.0, 0.0), PathSeg::Line(50.0, 30.0)],
            fill: None,
            stroke: Some([0.0, 0.0, 0.0]),
            stroke_width: 1.0,
            dash: Vec::new(),
        };
        let html = html_for_shape(shape);
        assert!(html.contains("<svg "), "inline svg emitted: {html}");
        assert!(
            html.contains("fill=\"none\""),
            "unfilled → fill=none: {html}"
        );
        assert!(html.contains("stroke=\"#000000\""), "stroke kept: {html}");
    }

    #[test]
    fn html_shape_emits_dash_pattern() {
        // A dashed stroke surfaces as `stroke-dasharray`.
        let shape = Shape {
            segments: vec![PathSeg::Move(0.0, 0.0), PathSeg::Line(40.0, 0.0)],
            fill: None,
            stroke: Some([0.2, 0.2, 0.2]),
            stroke_width: 1.0,
            dash: vec![3.0, 2.0],
        };
        let html = html_for_shape(shape);
        assert!(
            html.contains("stroke-dasharray=\"3,2\""),
            "dash pattern surfaced: {html}"
        );
    }

    #[test]
    fn html_shape_freeform_bezier_geometry_is_preserved() {
        // A free-form path with a cubic Bézier emits a `C` command (geometry not
        // reduced to a primitive box). Bounds span (0,0)-(30,40).
        let shape = Shape {
            segments: vec![
                PathSeg::Move(0.0, 0.0),
                PathSeg::Cubic(10.0, 40.0, 20.0, 40.0, 30.0, 0.0),
            ],
            fill: Some([0.0, 0.5, 0.0]),
            stroke: None,
            stroke_width: 0.0,
            dash: Vec::new(),
        };
        let html = html_for_shape(shape);
        assert!(
            html.contains("viewBox=\"0 0 30 40\""),
            "viewBox from curve extent: {html}"
        );
        assert!(
            html.contains("<path d=\"M0 40 C10 0 20 0 30 40\""),
            "cubic geometry preserved (Y flipped): {html}"
        );
        assert!(
            html.contains("fill=\"#008000\"") && !html.contains("stroke="),
            "filled, no stroke attrs: {html}"
        );
    }

    #[test]
    fn html_shape_without_geometry_falls_back_to_placeholder() {
        // An empty (point-less) shape can't form a viewBox; keep the small box so
        // the shape isn't silently dropped.
        let shape = Shape {
            fill: Some([1.0, 0.0, 0.0]),
            ..Shape::default()
        };
        let html = html_for_shape(shape);
        assert!(!html.contains("<svg "), "no svg for empty geometry: {html}");
        assert!(
            html.contains("width:1em") && html.contains("background:#FF0000"),
            "fallback placeholder retains fill: {html}"
        );
    }

    #[test]
    fn html_textbox_block_keeps_its_text() {
        // A textbox carries flowing text (not vector geometry); its text must
        // survive into the HTML alongside any shapes.
        use crate::model::{InlineRun, Page, Section, TextBox};
        let doc = Document {
            sections: vec![Section {
                pages: vec![Page {
                    blocks: vec![Block {
                        kind: BlockKind::TextBox(TextBox {
                            blocks: vec![Block {
                                kind: BlockKind::Paragraph(Paragraph {
                                    runs: vec![Inline::Run(InlineRun {
                                        text: "Caption text".to_string(),
                                        style: CharStyle::default(),
                                        source_index: None,
                                    })],
                                    ..Paragraph::default()
                                }),
                                ..Default::default()
                            }],
                        }),
                        ..Default::default()
                    }],
                    absolute: false,
                }],
                ..Section::default()
            }],
            ..Document::default()
        };
        let html = html_from_model(&doc);
        assert!(
            html.contains("Caption text"),
            "textbox text preserved: {html}"
        );
    }
}
