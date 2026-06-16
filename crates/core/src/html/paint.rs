//! Paint a laid-out HTML document to PDF, with text rendered in **embedded
//! Google fonts** (real glyphs + real metrics), backgrounds/borders as vector
//! rectangles, and images placed from `data:` URIs.
//!
//! Two-phase, matching the engine's zero-network rule: [`needed_fonts`] tells
//! the host which font files to download from Google Fonts; the host fetches
//! them and passes the bytes to [`render`], which embeds them and measures text
//! with their true advance widths so the output is identical to the page.

use crate::convert::build::PdfBuilder;
use crate::document::Document;
use crate::font::{catalog, google, truetype::TrueTypeFont};

use super::css::{collect_style_css, Display, Style, Stylesheet};
use super::dom::{self, Element, Node};
use super::layout::{layout_document_framed, Fragment, Frame, Layout, Measure};
use super::page::{substitute_tokens, Margins, RenderOptions};

/// A font the host must download (resolved against the Google-Fonts catalogue).
#[derive(Debug, Clone)]
pub struct FontRequest {
    pub family: String,
    pub weight: u16,
    pub italic: bool,
    /// Google-Fonts CSS URL (host fetches → TTF, like the rest of the engine).
    pub url: String,
}

/// A downloaded font supplied back to [`render`].
#[derive(Debug, Clone)]
pub struct ProvidedFont {
    pub family: String,
    pub weight: u16,
    pub italic: bool,
    pub ttf: Vec<u8>,
}

type Key = (String, bool, bool);

fn key(family: &str, bold: bool, italic: bool) -> Key {
    (family.to_ascii_lowercase(), bold, italic)
}

fn weight_bold(w: u16) -> bool {
    w >= 600
}

// ─── font resolution + measurement ────────────────────────────────────────────

/// Parsed faces used for line-breaking before any PDF object exists.
struct MeasureBook {
    faces: Vec<(Key, TrueTypeFont)>,
}

impl MeasureBook {
    fn new(fonts: &[ProvidedFont]) -> MeasureBook {
        let faces = fonts
            .iter()
            .filter_map(|f| {
                TrueTypeFont::parse(&f.ttf)
                    .map(|ttf| (key(&f.family, weight_bold(f.weight), f.italic), ttf))
            })
            .collect();
        MeasureBook { faces }
    }

    /// Nearest face for a style: exact (family,bold,italic) → same family →
    /// any face. `None` when no font was provided at all.
    fn face(&self, style: &Style) -> Option<&TrueTypeFont> {
        let fam = style.font_family.to_ascii_lowercase();
        self.faces
            .iter()
            .find(|(k, _)| k.0 == fam && k.1 == style.bold && k.2 == style.italic)
            .or_else(|| self.faces.iter().find(|(k, _)| k.0 == fam))
            .or_else(|| self.faces.first())
            .map(|(_, t)| t)
    }
}

impl Measure for MeasureBook {
    fn width(&self, text: &str, style: &Style) -> f64 {
        if let Some(ttf) = self.face(style) {
            let upm = ttf.units_per_em().max(1.0);
            let mut w = 0.0;
            for c in text.chars() {
                let gid = ttf.gid_for_unicode(c as u32).unwrap_or(0);
                w += ttf.advance_width(gid) / upm * style.font_size;
            }
            let boldish = if style.bold && !style_has_bold_face(self, style) {
                1.03
            } else {
                1.0
            };
            w * boldish
        } else {
            // No fonts provided yet — rough estimate (host still fetches them).
            let per = if style.generic_mono { 0.6 } else { 0.5 };
            text.chars().count() as f64 * style.font_size * per
        }
    }
}

fn style_has_bold_face(book: &MeasureBook, style: &Style) -> bool {
    let fam = style.font_family.to_ascii_lowercase();
    book.faces.iter().any(|(k, _)| k.0 == fam && k.1)
}

// ─── needed fonts (phase 1) ───────────────────────────────────────────────────

/// Resolve every (family, weight, italic) combination the document references
/// to a Google-Fonts download URL, deduplicated. The host fetches these.
pub fn needed_fonts(html: &str) -> Vec<FontRequest> {
    needed_fonts_with(html, None, None)
}

/// Like [`needed_fonts`] but also scans the running `header`/`footer` HTML, so
/// the fonts they reference are requested too.
pub fn needed_fonts_with(
    html: &str,
    header: Option<&str>,
    footer: Option<&str>,
) -> Vec<FontRequest> {
    // Run inline <script>s on the body first so script-generated content is seen.
    let body = crate::js::run_inline_scripts(html);
    let mut seen: Vec<Key> = Vec::new();
    let root = Style {
        display: Display::Block,
        ..Style::default()
    };
    for src in [Some(body.as_str()), header, footer].into_iter().flatten() {
        let nodes = dom::parse(src);
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        collect_fonts(&nodes, &sheet, &root, &[], &mut seen);
    }

    let mut out = Vec::new();
    for (fam, bold, italic) in seen {
        // Resolve to a real catalogue family (handles aliases / casing).
        let canonical = catalog::lookup(&fam).map(|f| f.family.to_string());
        let Some(family) = canonical else { continue };
        let weight = if bold { 700 } else { 400 };
        out.push(FontRequest {
            url: google::css_url(&family, weight, italic),
            family,
            weight,
            italic,
        });
    }
    out
}

fn collect_fonts(
    nodes: &[Node],
    sheet: &Stylesheet,
    parent: &Style,
    ancestors: &[&Element],
    seen: &mut Vec<Key>,
) {
    for n in nodes {
        match n {
            Node::Text(t) => {
                if !t.trim().is_empty() && !parent.font_family.is_empty() {
                    let k = key(&parent.font_family, parent.bold, parent.italic);
                    if !seen.contains(&k) {
                        seen.push(k);
                    }
                }
            }
            Node::Element(e) => {
                if matches!(e.tag.as_str(), "style" | "script" | "head") {
                    continue;
                }
                let st = sheet.computed(e, parent, ancestors);
                let mut na = ancestors.to_vec();
                na.push(e);
                collect_fonts(&e.children, sheet, &st, &na, seen);
            }
        }
    }
}

// ─── render (phase 2) ──────────────────────────────────────────────────────────

/// Render `html` to a PDF using the supplied fonts. `page_w`/`page_h` and
/// `margin` are in points (US-Letter portrait with 0.5in margins is a good
/// default: `612, 792, 36`). Uniform margins, no running header/footer — for the
/// full page control see [`render_with`]. Returns the PDF bytes.
pub fn render(
    html: &str,
    fonts: &[ProvidedFont],
    page_w: f64,
    page_h: f64,
    margin: f64,
) -> Vec<u8> {
    let mut opts = RenderOptions::new(page_w, page_h);
    opts.margins = Margins::uniform(margin);
    render_with(html, fonts, &opts)
}

/// Render `html` to a PDF with full page control: named/explicit size, per-side
/// margins, and a running header/footer (with `{{page}}` / `{{pages}}`
/// substitution) painted in the top/bottom margins of every page.
pub fn render_with(html: &str, fonts: &[ProvidedFont], opts: &RenderOptions) -> Vec<u8> {
    // Run inline <script>s first so script-driven DOM mutations are rendered.
    let body_html = crate::js::run_inline_scripts(html);
    let nodes = dom::parse(&body_html);
    let sheet = Stylesheet::new(&collect_style_css(&nodes));
    let book = MeasureBook::new(fonts);
    let frame = Frame {
        page_w: opts.page_w,
        page_h: opts.page_h,
        top: opts.margins.top,
        right: opts.margins.right,
        bottom: opts.margins.bottom,
        left: opts.margins.left,
    };
    let layout = layout_document_framed(&nodes, &sheet, &book, &frame);
    let n_pages = layout.pages.len().max(1);

    // Per-page header/footer fragments (token substitution + positioning).
    let headers = build_running(&opts.header, &book, opts, n_pages, true);
    let footers = build_running(&opts.footer, &book, opts, n_pages, false);

    paint(&layout, &book, fonts, opts, &headers, &footers).unwrap_or_else(|| {
        // Last-resort: a valid (blank) document of the right page count.
        let mut b = PdfBuilder::new();
        for _ in 0..n_pages {
            b.add_page(opts.page_w, opts.page_h);
        }
        b.finish()
    })
}

/// Lay out a running header/footer snippet once per page, substituting the
/// `{{page}}` / `{{pages}}` tokens, and position it in the page margin.
fn build_running(
    snippet: &Option<String>,
    book: &MeasureBook,
    opts: &RenderOptions,
    n_pages: usize,
    is_header: bool,
) -> Vec<Vec<Fragment>> {
    let Some(tpl) = snippet else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(n_pages);
    for i in 0..n_pages {
        let page_no = opts.start_page_number + i as u32;
        let html = substitute_tokens(tpl, page_no, n_pages as u32);
        let (frags, h) = layout_band(
            &html,
            book,
            opts.page_w,
            opts.margins.left,
            opts.margins.right,
        );
        // Header sits at `header_offset` from the top; footer's bottom sits at
        // `footer_offset` from the page bottom.
        let dy = if is_header {
            opts.header_offset
        } else {
            opts.page_h - opts.footer_offset - h
        };
        out.push(offset_fragments(frags, dy));
    }
    out
}

/// Lay out a snippet on a single (very tall) page and return its fragments
/// (top-down from `y = 0`) plus the total content height.
fn layout_band(
    html: &str,
    book: &MeasureBook,
    page_w: f64,
    left: f64,
    right: f64,
) -> (Vec<Fragment>, f64) {
    let nodes = dom::parse(html);
    let sheet = Stylesheet::new(&collect_style_css(&nodes));
    let frame = Frame {
        page_w,
        page_h: 1.0e6,
        top: 0.0,
        right,
        bottom: 0.0,
        left,
    };
    let layout = layout_document_framed(&nodes, &sheet, book, &frame);
    let frags = layout.pages.into_iter().next().unwrap_or_default();
    let h = frags.iter().fold(0.0_f64, |m, f| {
        let bottom = match f {
            Fragment::Text { y, style, .. } => y + style.font_size,
            Fragment::Rect { y, h, .. } => y + h,
            Fragment::Image { y, h, .. } => y + h,
            Fragment::Svg { y, h, .. } => y + h,
        };
        m.max(bottom)
    });
    (frags, h)
}

/// Shift every fragment down by `dy` (places a band inside the page margin).
fn offset_fragments(frags: Vec<Fragment>, dy: f64) -> Vec<Fragment> {
    frags
        .into_iter()
        .map(|f| match f {
            Fragment::Text { x, y, style, text } => Fragment::Text {
                x,
                y: y + dy,
                style,
                text,
            },
            Fragment::Rect {
                x,
                y,
                w,
                h,
                fill,
                stroke,
                stroke_w,
                opacity,
            } => Fragment::Rect {
                x,
                y: y + dy,
                w,
                h,
                fill,
                stroke,
                stroke_w,
                opacity,
            },
            Fragment::Image { x, y, w, h, src } => Fragment::Image {
                x,
                y: y + dy,
                w,
                h,
                src,
            },
            Fragment::Svg { x, y, w, h, image } => Fragment::Svg {
                x,
                y: y + dy,
                w,
                h,
                image,
            },
        })
        .collect()
}

fn paint(
    layout: &Layout,
    book: &MeasureBook,
    fonts: &[ProvidedFont],
    opts: &RenderOptions,
    headers: &[Vec<Fragment>],
    footers: &[Vec<Fragment>],
) -> Option<Vec<u8>> {
    let page_w = opts.page_w;
    let page_h = opts.page_h;
    // Blank pages first (PdfBuilder), then re-open as an editable Document so we
    // can embed fonts and place real text/vector/image content.
    let mut b = PdfBuilder::new();
    for _ in 0..layout.pages.len().max(1) {
        b.add_page(page_w, page_h);
    }
    let mut doc = Document::open(&b.finish()).ok()?;

    // Embed every provided font once; remember its object id by face key.
    let mut objs: Vec<(Key, u32)> = Vec::new();
    for f in fonts {
        if let Ok(id) = doc.embed_truetype_font(&f.family, &f.ttf) {
            objs.push((key(&f.family, weight_bold(f.weight), f.italic), id));
        }
    }
    let resolve = |style: &Style| -> Option<u32> {
        let fam = style.font_family.to_ascii_lowercase();
        objs.iter()
            .find(|(k, _)| k.0 == fam && k.1 == style.bold && k.2 == style.italic)
            .or_else(|| objs.iter().find(|(k, _)| k.0 == fam))
            .or_else(|| objs.first())
            .map(|(_, id)| *id)
    };

    // Paint one fragment list onto a page (shared by body, header and footer).
    let paint_frags = |doc: &mut Document, page: u32, frags: &[Fragment]| {
        for frag in frags {
            match frag {
                Fragment::Rect {
                    x,
                    y,
                    w,
                    h,
                    fill,
                    stroke,
                    stroke_w,
                    opacity,
                } => {
                    // Top-down → PDF bottom-up (origin bottom-left).
                    let _ = doc.add_rectangle(
                        page,
                        *x,
                        page_h - y - h,
                        *w,
                        *h,
                        stroke.filter(|_| *stroke_w > 0.0),
                        *fill,
                        stroke_w.max(0.0),
                        *opacity,
                    );
                }
                Fragment::Text { x, y, style, text } => {
                    if style.hidden {
                        continue; // `visibility: hidden` — occupies space, no ink
                    }
                    let trimmed = text.trim_end_matches('\n');
                    if trimmed.is_empty() {
                        continue;
                    }
                    let Some(id) = resolve(style) else { continue };
                    // Baseline ≈ top + ascent (0.8·size), flipped.
                    let baseline = page_h - (y + style.font_size * 0.8);

                    // Colour-emoji fast path: when the resolved face has COLR/CPAL
                    // tables and this run holds a colour glyph, draw those glyphs as
                    // native vector layers and the rest as ordinary text.
                    let face = book.face(style);
                    let colors = face.and_then(|f| f.color_glyphs()); // COLR/CPAL
                    let sbix = face.and_then(|f| f.sbix_glyphs()); // Apple bitmap emoji
                                                                   // Classify a char: `Some((gid, is_colr))` for a colour glyph —
                                                                   // `is_colr=false` is an sbix bitmap; `None` is ordinary text.
                    let classify = |f: &crate::font::truetype::TrueTypeFont,
                                    ch: char|
                     -> Option<(u16, bool)> {
                        let g = f.gid_for_unicode(ch as u32)?;
                        if colors
                            .as_ref()
                            .map(|c| c.layers(g).is_some())
                            .unwrap_or(false)
                        {
                            Some((g, true))
                        } else if sbix.as_ref().map(|s| s.glyph(g).is_some()).unwrap_or(false) {
                            Some((g, false))
                        } else {
                            None
                        }
                    };
                    let color_run = face
                        .map(|f| trimmed.chars().any(|ch| classify(f, ch).is_some()))
                        .unwrap_or(false);

                    if let (true, Some(face)) = (color_run, face) {
                        // Walk the run, advancing by the same per-glyph widths the
                        // layout used, so colour glyphs land where text expects.
                        let mut pen = *x;
                        let mut seg = String::new();
                        let mut seg_x = pen;
                        for ch in trimmed.chars() {
                            let cw = book.width(&ch.to_string(), style);
                            match classify(face, ch) {
                                Some((g, is_colr)) => {
                                    if !seg.is_empty() {
                                        let _ = doc.add_text(
                                            page,
                                            seg_x,
                                            baseline,
                                            style.font_size,
                                            &seg,
                                            id,
                                            style.color,
                                            1.0,
                                            0.0,
                                        );
                                        seg.clear();
                                    }
                                    if is_colr {
                                        if let Some(c) = colors.as_ref() {
                                            let _ = doc.draw_color_glyph(
                                                page,
                                                face,
                                                c,
                                                g,
                                                pen,
                                                baseline,
                                                style.font_size,
                                                style.color,
                                            );
                                        }
                                    } else {
                                        let _ = doc.draw_sbix_glyph(
                                            page,
                                            face,
                                            g,
                                            pen,
                                            baseline,
                                            style.font_size,
                                        );
                                    }
                                    pen += cw;
                                    seg_x = pen;
                                }
                                None => {
                                    if seg.is_empty() {
                                        seg_x = pen;
                                    }
                                    seg.push(ch);
                                    pen += cw;
                                }
                            }
                        }
                        if !seg.is_empty() {
                            let _ = doc.add_text(
                                page,
                                seg_x,
                                baseline,
                                style.font_size,
                                &seg,
                                id,
                                style.color,
                                1.0,
                                0.0,
                            );
                        }
                    } else {
                        let _ = doc.add_text(
                            page,
                            *x,
                            baseline,
                            style.font_size,
                            trimmed,
                            id,
                            style.color,
                            1.0,
                            0.0,
                        );
                    }

                    // Decoration rules (underline / line-through / overline) are
                    // thin filled rects at the run's top-down offset.
                    let size = style.font_size;
                    let mut rule = |top_offset: f64| {
                        let w = book.width(trimmed, style);
                        let _ = doc.add_rectangle(
                            page,
                            *x,
                            page_h - (y + top_offset),
                            w,
                            (size * 0.06).max(0.4),
                            None,
                            Some(style.color),
                            0.0,
                            style.opacity,
                        );
                    };
                    if style.underline {
                        rule(size); // at the baseline
                    }
                    if style.strike {
                        rule(size * 0.55); // through the x-height
                    }
                    if style.overline {
                        rule(size * 0.05); // near the top
                    }
                }
                Fragment::Image { x, y, w, h, src } => {
                    if let Some(data) = decode_data_uri(src) {
                        let _ = doc.add_image(page, &data, *x, page_h - y - h, *w, *h, 1.0);
                    }
                }
                Fragment::Svg { x, y, w, h, image } => {
                    // Native vector — placed top-down → PDF bottom-up box.
                    let _ = doc.draw_svg_image(page, image, *x, page_h - y - h, *w, *h);
                }
            }
        }
    };

    for (pi, frags) in layout.pages.iter().enumerate() {
        let page = (pi + 1) as u32;
        paint_frags(&mut doc, page, frags);
        if let Some(hd) = headers.get(pi) {
            paint_frags(&mut doc, page, hd);
        }
        if let Some(ft) = footers.get(pi) {
            paint_frags(&mut doc, page, ft);
        }
    }

    doc.save_compressed().into()
}

/// Decode a `data:[mime];base64,…` image URI to raw bytes (PNG/JPEG).
fn decode_data_uri(src: &str) -> Option<Vec<u8>> {
    let rest = src.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let meta = &rest[..comma];
    let data = &rest[comma + 1..];
    if meta.contains("base64") {
        base64_decode(data)
    } else {
        Some(data.as_bytes().to_vec())
    }
}

/// Minimal standard-alphabet base64 decoder (ignores whitespace/newlines).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needed_fonts_resolves_google_family() {
        let reqs = needed_fonts(r#"<p style="font-family:Roboto">Hello</p>"#);
        assert!(
            reqs.iter()
                .any(|r| r.family.eq_ignore_ascii_case("Roboto") && r.url.contains("fonts")),
            "Roboto is requested with a Google-Fonts URL: {reqs:?}"
        );
    }

    #[test]
    fn render_produces_a_valid_pdf() {
        // No fonts provided: backgrounds still paint; the result is a real PDF.
        let pdf = render(
            r#"<div style="background:#eeeeee;padding:10pt"><p>Hello world</p></div>"#,
            &[],
            612.0,
            792.0,
            36.0,
        );
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF header");
        assert!(pdf.len() > 200, "non-trivial output ({} bytes)", pdf.len());
    }

    #[test]
    fn footer_tokens_and_bottom_placement() {
        use super::super::page::RenderOptions;
        let book = MeasureBook::new(&[]);
        let mut opts = RenderOptions::new(400.0, 600.0);
        opts.footer = Some("Page {{page}} of {{pages}}".into());
        opts.footer_offset = 20.0;
        let footers = build_running(&opts.footer, &book, &opts, 3, false);
        assert_eq!(footers.len(), 3, "one footer per page");
        // Page 2's footer carries the substituted "Page 2 of 3".
        let p2: String = footers[1]
            .iter()
            .filter_map(|f| match f {
                Fragment::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            p2.contains('2') && p2.contains('3'),
            "footer text 2/3: {p2:?}"
        );
        // It is positioned down in the bottom margin (near the page bottom).
        let max_y = footers[1]
            .iter()
            .filter_map(|f| match f {
                Fragment::Text { y, .. } => Some(*y),
                _ => None,
            })
            .fold(0.0_f64, f64::max);
        assert!(
            max_y > 500.0,
            "footer sits near the page bottom (y={max_y})"
        );
    }

    #[test]
    fn render_with_named_size_runs_end_to_end() {
        use super::super::page::{page_size, Margins, RenderOptions};
        let (w, h) = page_size("A5").unwrap();
        let mut opts = RenderOptions::new(w, h);
        opts.margins = Margins::symmetric(48.0, 36.0);
        opts.header = Some(r#"<div style="background:#eeeeee">Report</div>"#.into());
        opts.footer =
            Some(r#"<div style="text-align:center">Page {{page}}/{{pages}}</div>"#.into());
        let html = format!("<div>{}</div>", "<p>content line</p>".repeat(100));
        let pdf = render_with(&html, &[], &opts);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        assert!(pdf.len() > 400, "non-trivial output ({} bytes)", pdf.len());
    }

    #[test]
    fn base64_round_trips_png_magic() {
        // "iVBORw0KGgo=" is the base64 of the PNG signature start.
        let bytes = base64_decode("iVBORw0KGgo=").unwrap();
        assert_eq!(&bytes[..4], &[0x89, 0x50, 0x4E, 0x47]);
    }
}
