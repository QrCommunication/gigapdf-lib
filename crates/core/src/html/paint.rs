//! Paint a laid-out HTML document to PDF, with text rendered in **embedded
//! Google fonts** (real glyphs + real metrics), backgrounds/borders as vector
//! rectangles, and images placed from `data:` URIs.
//!
//! Two-phase, matching the engine's zero-network rule: [`needed_fonts`] tells
//! the host which font files to download from Google Fonts; the host fetches
//! them and passes the bytes to [`render`], which embeds them and measures text
//! with their true advance widths so the output is identical to the page.

use crate::content::num;
use crate::convert::build::PdfBuilder;
use crate::document::Document;
use crate::font::{bundled, catalog, google, shape::Shaper, truetype::TrueTypeFont};

use super::css::{
    collect_style_css, BorderStyle, ConicGradient, CssGradient, Display, LinearGradient,
    RadialGradient, Style, Stylesheet,
};
use super::dom::{self, Element, Node};
use super::layout::{
    fragment_bbox, layout_document_framed, shift_fragment, Fragment, Frame, Layout, Measure,
};
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

/// A parsed face plus its OpenType-Layout shaper (kerning + ligatures), built
/// once and reused for both measuring and painting so the two never disagree.
struct Face {
    ttf: TrueTypeFont,
    shaper: Shaper,
}

impl Face {
    fn parse(bytes: &[u8]) -> Option<Face> {
        let ttf = TrueTypeFont::parse(bytes)?;
        let shaper = Shaper::new(&ttf);
        Some(Face { ttf, shaper })
    }
}

/// Parsed faces used for line-breaking before any PDF object exists.
struct MeasureBook {
    faces: Vec<(Key, Face)>,
    /// Bundled last-resort face (Liberation Sans), parsed once. Used for real
    /// metrics whenever no host-provided face matches a run, so offline /
    /// unknown-family text still lays out with true advance widths instead of a
    /// rough estimate. `None` only if the bundled program failed to parse.
    fallback: Option<Face>,
}

impl MeasureBook {
    fn new(fonts: &[ProvidedFont]) -> MeasureBook {
        let faces = fonts
            .iter()
            .filter_map(|f| {
                Face::parse(&f.ttf)
                    .map(|face| (key(&f.family, weight_bold(f.weight), f.italic), face))
            })
            .collect();
        MeasureBook {
            faces,
            fallback: Face::parse(bundled::FALLBACK_TTF),
        }
    }

    /// Nearest *host-provided* face (font + shaper) for a style: exact
    /// (family,bold,italic) → same family → any provided face. `None` when no
    /// font was provided at all (the caller then falls back to the bundled face).
    fn provided(&self, style: &Style) -> Option<&Face> {
        let fam = style.font_family.to_ascii_lowercase();
        self.faces
            .iter()
            .find(|(k, _)| k.0 == fam && k.1 == style.bold && k.2 == style.italic)
            .or_else(|| self.faces.iter().find(|(k, _)| k.0 == fam))
            .or_else(|| self.faces.first())
            .map(|(_, f)| f)
    }

    /// The face (font + shaper) to *measure and draw* a run with: a host-provided
    /// face when one exists (online path), otherwise the bundled fallback. `None`
    /// only if even the bundled font failed to parse.
    fn resolve_face(&self, style: &Style) -> Option<&Face> {
        self.provided(style).or(self.fallback.as_ref())
    }

    /// The TrueType program to measure and draw a run with (provided or bundled).
    fn face(&self, style: &Style) -> Option<&TrueTypeFont> {
        self.resolve_face(style).map(|f| &f.ttf)
    }
}

impl Measure for MeasureBook {
    fn width(&self, text: &str, style: &Style) -> f64 {
        if let Some(face) = self.resolve_face(style) {
            let w = shaped_run_width(&face.ttf, &face.shaper, text, style.font_size);
            // Synthetic-bold widening when no provided face matches the requested
            // weight band, graduated by the numeric `font-weight` (heavier ⇒
            // wider). A face that already supplies the bold variant needs none.
            w * synthetic_bold_factor(style, style_has_bold_face(self, style))
        } else {
            // Neither a provided nor the bundled face is available — rough
            // estimate (should not happen in practice).
            let per = if style.generic_mono { 0.6 } else { 0.5 };
            text.chars().count() as f64 * style.font_size * per
        }
    }
}

/// Advance-width multiplier emulating heavier `font-weight` when no real bold
/// face is available, graduated across the 100–900 scale.
///
/// * weight ≤ 500 (regular and lighter) ⇒ `1.0` (no widening — there is no
///   "thinning" of a regular face, and a lighter request renders as regular).
/// * weight ≥ 600 with a **real** bold face provided ⇒ `1.0` (the bold glyphs
///   already carry the extra width).
/// * weight 600–700 with **no** bold face ⇒ exactly `1.03` (byte-identical to the
///   previous single bold factor, so ordinary `font-weight: bold` is unchanged).
/// * weight > 700 with **no** bold face ⇒ widening graduated from `1.03` at 700 up
///   to ~`1.06` at 900, so `900` reads heavier than `700` even on a regular-only
///   family. Kept deliberately gentle to avoid visibly mis-spacing text.
fn synthetic_bold_factor(style: &Style, has_bold_face: bool) -> f64 {
    if style.font_weight < 600 || has_bold_face {
        return 1.0;
    }
    // 600–700 → 1.03 (unchanged); 700–900 → 1.03…1.06.
    let t = ((style.font_weight as f64 - 700.0) / 200.0).clamp(0.0, 1.0);
    1.03 + 0.03 * t
}

/// Advance of a text run in points, **shaped**: characters are mapped to glyph
/// ids, GSUB ligatures/substitutions applied (so a ligated pair counts as one
/// glyph's advance), then the per-glyph `hmtx` widths are summed with the GPOS
/// pair-kern adjustment between adjacent glyphs folded in. This is the same
/// number the painter draws against, so kerned/ligated text lays out correctly.
fn shaped_run_width(ttf: &TrueTypeFont, shaper: &Shaper, text: &str, font_size: f64) -> f64 {
    let upm = ttf.units_per_em().max(1.0);
    let gids: Vec<u16> = text
        .chars()
        .map(|c| ttf.gid_for_unicode(c as u32).unwrap_or(0))
        .collect();
    let shaped = if shaper.is_empty() {
        gids
    } else {
        shaper.substitute(&gids)
    };
    let mut units = 0.0;
    for (i, &g) in shaped.iter().enumerate() {
        units += ttf.advance_width(g);
        if i + 1 < shaped.len() {
            units += shaper.kern(g, shaped[i + 1]) as f64;
        }
    }
    units / upm * font_size
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
        // base-14 standard families are drawn natively from the bundled
        // substitute (the render path uses `add_text_standard`), so the host
        // must NOT fetch or supply them — otherwise they'd be embedded/
        // referenced as a normal provided face. Skip them from the fetch list.
        if bundled::base14_kind(&fam).is_some() {
            continue;
        }
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

/// One external thing the document needs the host to fetch. Fonts carry their
/// Google-Fonts download metadata; images carry the raw URL referenced by an
/// `<img src>`. A single discovery call so the host runs **one** fetch loop —
/// the font and image ports share this list rather than duplicating it.
#[derive(Debug, Clone)]
pub enum ResourceNeed {
    /// A font to resolve against Google Fonts (host fetches `url` → TTF).
    Font(FontRequest),
    /// An external image URL (`<img src>`, non-`data:`) the host must download.
    Image(String),
}

/// Every external resource the document (and its running header/footer) needs:
/// the [`FontRequest`]s plus the external image URLs. The host downloads each
/// and supplies the bytes back — fonts via [`ProvidedFont`], images via
/// [`RenderOptions::resources`](super::page::RenderOptions::resources) — keeping
/// the engine zero-network. `data:` image URIs are inlined and never listed.
pub fn needed_resources(
    html: &str,
    header: Option<&str>,
    footer: Option<&str>,
) -> Vec<ResourceNeed> {
    let mut out: Vec<ResourceNeed> = needed_fonts_with(html, header, footer)
        .into_iter()
        .map(ResourceNeed::Font)
        .collect();
    // Same DOM walk shape as the font scan: run inline scripts so script-injected
    // <img> are seen, then collect external sources.
    let body = crate::js::run_inline_scripts(html);
    let mut urls: Vec<String> = Vec::new();
    for src in [Some(body.as_str()), header, footer].into_iter().flatten() {
        collect_image_urls(&dom::parse(src), &mut urls);
    }
    out.extend(urls.into_iter().map(ResourceNeed::Image));
    out
}

/// Collect external `<img src>` URLs (skipping `data:` URIs and duplicates).
fn collect_image_urls(nodes: &[Node], out: &mut Vec<String>) {
    for n in nodes {
        if let Node::Element(e) = n {
            if e.tag == "img" {
                if let Some(src) = e.attr("src") {
                    if !src.is_empty() && !src.starts_with("data:") && !out.iter().any(|u| u == src)
                    {
                        out.push(src.to_string());
                    }
                }
            }
            collect_image_urls(&e.children, out);
        }
    }
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
            Fragment::Border { y, h, .. } => y + h,
            Fragment::Gradient { y, h, .. } => y + h,
            Fragment::Clipped { inner, .. } => fragment_bbox(inner).3,
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
            Fragment::Text {
                x,
                y,
                w,
                style,
                text,
            } => Fragment::Text {
                x,
                y: y + dy,
                w,
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
                radius,
                radius_v,
                shadow,
            } => Fragment::Rect {
                x,
                y: y + dy,
                w,
                h,
                fill,
                stroke,
                stroke_w,
                opacity,
                radius,
                radius_v,
                shadow,
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
            Fragment::Border {
                x,
                y,
                w,
                h,
                horizontal,
                width,
                color,
                style,
                opacity,
            } => Fragment::Border {
                x,
                y: y + dy,
                w,
                h,
                horizontal,
                width,
                color,
                style,
                opacity,
            },
            Fragment::Gradient {
                x,
                y,
                w,
                h,
                gradient,
                opacity,
            } => Fragment::Gradient {
                x,
                y: y + dy,
                w,
                h,
                gradient,
                opacity,
            },
            clipped @ Fragment::Clipped { .. } => {
                // Move the clip window and its content together.
                let mut moved = clipped;
                shift_fragment(&mut moved, 0.0, dy);
                moved
            }
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
    // Resolve a run to a *provided* font object id (no fallback).
    let resolve_provided = |objs: &[(Key, u32)], style: &Style| -> Option<u32> {
        let fam = style.font_family.to_ascii_lowercase();
        objs.iter()
            .find(|(k, _)| k.0 == fam && k.1 == style.bold && k.2 == style.italic)
            .or_else(|| objs.iter().find(|(k, _)| k.0 == fam))
            .or_else(|| objs.first())
            .map(|(_, id)| *id)
    };
    // Embed the bundled last-resort face only when some painted text run has no
    // matching provided font — so runs render real, selectable glyphs offline /
    // for unknown families, without bloating output that needs no fallback (the
    // full program would otherwise stay embedded when no run references it).
    let needs_fallback = layout
        .pages
        .iter()
        .chain(headers.iter())
        .chain(footers.iter())
        .flatten()
        .any(|frag| match frag {
            Fragment::Text { style, text, .. } => {
                !style.hidden
                    && !text.trim_end_matches('\n').is_empty()
                    && resolve_provided(&objs, style).is_none()
            }
            _ => false,
        });
    let fallback_id = if needs_fallback {
        doc.embed_truetype_font(bundled::FALLBACK_FAMILY, bundled::FALLBACK_TTF)
            .ok()
    } else {
        None
    };
    let resolve = |style: &Style| -> Option<u32> {
        // Host / Google fonts always win when present; the bundled face is the
        // last resort (real glyphs + metrics).
        resolve_provided(&objs, style).or(fallback_id)
    };

    // Paint one fragment list onto a page (shared by body, header and footer).
    let paint_frags = |doc: &mut Document, page: u32, frags: &[Fragment]| {
        for frag in frags {
            // Unwrap nested `Clipped` layers: emit one `q … re W n` per clip
            // (flip Y to PDF user-space), paint the inner fragment, then balance
            // with one `Q` per clip. Nested clips intersect — `overflow` boxes
            // nest. A bare fragment leaves `clips` empty ⇒ byte-identical output.
            let mut clips: Vec<[f64; 4]> = Vec::new();
            let mut real = frag;
            while let Fragment::Clipped { rect, inner } = real {
                clips.push(*rect);
                real = inner;
            }
            for r in &clips {
                let _ = doc.push_clip_rect(page, r[0], page_h - r[1] - r[3], r[2], r[3]);
            }
            match real {
                Fragment::Rect {
                    x,
                    y,
                    w,
                    h,
                    fill,
                    stroke,
                    stroke_w,
                    opacity,
                    radius,
                    radius_v,
                    shadow,
                } => {
                    let rounded = radius.iter().any(|r| *r > 0.0)
                        || radius_v.iter().any(|r| *r > 0.0);

                    // Drop shadow first (painted behind the box): an offset rect
                    // grown by `spread`, in the shadow colour, with a soft blurred
                    // edge. Tracks the box's (possibly elliptical) corners.
                    if let Some(sh) = shadow {
                        if !sh.inset {
                            paint_box_shadow(
                                doc, page, page_h, *x, *y, *w, *h, *radius, *radius_v, sh,
                            );
                        }
                    }

                    let stroke_c = stroke.filter(|_| *stroke_w > 0.0);
                    if !rounded {
                        // Square corners — unchanged rectangular path (byte-for-byte
                        // identical to the pre-radius behaviour).
                        let _ = doc.add_rectangle(
                            page,
                            *x,
                            page_h - y - h,
                            *w,
                            *h,
                            stroke_c,
                            *fill,
                            stroke_w.max(0.0),
                            *opacity,
                        );
                    } else {
                        // Rounded box: emit a rounded-rect path whose fill (and, for
                        // a uniform border, stroke) follow the rounded contour.
                        // `add_path` maps SVG-(0,0)→(ox,oy) with Y flipped, so we
                        // pass the path in top-down page coords and oy = page_h.
                        let d = rounded_rect_path(*x, *y, *w, *h, *radius, *radius_v);
                        let _ = doc.add_path(
                            page,
                            &d,
                            0.0,
                            page_h,
                            stroke_c,
                            *fill,
                            stroke_w.max(0.0),
                            *opacity,
                        );
                    }
                }
                Fragment::Text {
                    x, y, style, text, ..
                } => {
                    if style.hidden {
                        continue; // `visibility: hidden` — occupies space, no ink
                    }
                    let trimmed = text.trim_end_matches('\n');
                    if trimmed.is_empty() {
                        continue;
                    }
                    // Baseline ≈ top + ascent (0.8·size), flipped.
                    let baseline = page_h - (y + style.font_size * 0.8);
                    // Fold the colour's alpha (`rgba`/`hsla`/`#rgba`) into the
                    // element opacity for this run.
                    let text_opacity = (style.opacity * style.color_alpha).clamp(0.0, 1.0);
                    let id = match resolve(style) {
                        Some(id) => id,
                        None => {
                            // Deep safety net: not even the bundled fallback face
                            // could be embedded. Fall back to a built-in base-14
                            // standard font so text still renders — picked from the
                            // run's serif/mono + bold/italic, mapped to WinAnsi.
                            let base14 = base14_for(style);
                            let _ = doc.add_text_standard(
                                page,
                                *x,
                                baseline,
                                style.font_size,
                                trimmed,
                                base14,
                                style.color,
                                text_opacity,
                                0.0,
                            );
                            let w = book.width(trimmed, style);
                            paint_text_decorations(doc, page, page_h, *x, *y, w, style);
                            continue;
                        }
                    };

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
                                            text_opacity,
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
                                text_opacity,
                                0.0,
                            );
                        }
                    } else if crate::font::shape::detect_complex_script(trimmed).is_some() {
                        // Arabic-family joining, Hebrew, or any combining diacritic:
                        // draw the run shaped so GPOS mark attachment / cursive forms
                        // take effect. Latin/simple runs skip this and keep the plain
                        // `add_text` path (byte-identical output).
                        let _ = doc.add_text_shaped(
                            page,
                            *x,
                            baseline,
                            style.font_size,
                            trimmed,
                            id,
                            style.color,
                            text_opacity,
                            0.0,
                        );
                    } else {
                        let _ = doc.add_text(
                            page,
                            *x,
                            baseline,
                            style.font_size,
                            trimmed,
                            id,
                            style.color,
                            text_opacity,
                            0.0,
                        );
                    }

                    // Decoration rules (underline / line-through / overline).
                    let w = book.width(trimmed, style);
                    paint_text_decorations(doc, page, page_h, *x, *y, w, style);
                }
                Fragment::Image { x, y, w, h, src } => {
                    if let Some(data) = resolve_image(src, &opts.resources) {
                        let _ = doc.add_image(page, &data, *x, page_h - y - h, *w, *h, 1.0);
                    }
                }
                Fragment::Svg { x, y, w, h, image } => {
                    // Native vector — placed top-down → PDF bottom-up box.
                    let _ = doc.draw_svg_image(page, image, *x, page_h - y - h, *w, *h);
                }
                Fragment::Border {
                    x,
                    y,
                    w,
                    h,
                    horizontal,
                    width,
                    color,
                    style,
                    opacity,
                } => {
                    paint_styled_border(
                        doc, page, page_h, *x, *y, *w, *h, *horizontal, *width, *color, *style,
                        *opacity,
                    );
                }
                Fragment::Gradient {
                    x,
                    y,
                    w,
                    h,
                    gradient,
                    opacity,
                } => {
                    paint_css_gradient(doc, page, page_h, *x, *y, *w, *h, gradient, *opacity);
                }
                // Unwrapped above into `clips` + `real`; never a `Clipped` here.
                Fragment::Clipped { .. } => {}
            }
            for _ in &clips {
                let _ = doc.restore_graphics(page);
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

/// Choose the base-14 standard font name for a style when no embedded face is
/// available: serif→Times, monospace→Courier, else Helvetica — with the
/// bold/italic suffix each family uses.
fn base14_for(style: &Style) -> &'static str {
    match (
        style.generic_serif,
        style.generic_mono,
        style.bold,
        style.italic,
    ) {
        (true, _, true, true) => "Times-BoldItalic",
        (true, _, true, false) => "Times-Bold",
        (true, _, false, true) => "Times-Italic",
        (true, _, false, false) => "Times-Roman",
        (_, true, true, true) => "Courier-BoldOblique",
        (_, true, true, false) => "Courier-Bold",
        (_, true, false, true) => "Courier-Oblique",
        (_, true, false, false) => "Courier",
        (_, _, true, true) => "Helvetica-BoldOblique",
        (_, _, true, false) => "Helvetica-Bold",
        (_, _, false, true) => "Helvetica-Oblique",
        (_, _, false, false) => "Helvetica",
    }
}

/// Paint a run's text-decoration rules (underline / line-through / overline) as
/// thin filled rectangles spanning `width`, at the run's top-down offset.
/// Shared by the embedded and base-14 text paths.
fn paint_text_decorations(
    doc: &mut Document,
    page: u32,
    page_h: f64,
    x: f64,
    y: f64,
    width: f64,
    style: &Style,
) {
    let size = style.font_size;
    // The run's baseline sits at top + 0.8·size (matching the text path); rule
    // positions below are expressed relative to that baseline.
    let baseline = size * 0.8;
    let thickness = (size / 14.0).max(0.4);
    let mut draw = |top_offset: f64| {
        let _ = doc.add_rectangle(
            page,
            x,
            page_h - (y + top_offset),
            width,
            thickness,
            None,
            Some(style.color),
            0.0,
            (style.opacity * style.color_alpha).clamp(0.0, 1.0),
        );
    };
    if style.underline {
        draw(baseline + size * 0.12); // just under the baseline
    }
    if style.strike {
        draw(baseline - size * 0.30); // mid-height, through the text
    }
    if style.overline {
        draw(size * 0.02); // near the top of the em box
    }
}

/// Build an SVG path string for a rounded rectangle, in **top-down page coords**
/// (x right, y downward — the same space as a [`Fragment::Rect`]). Designed to be
/// drawn with [`crate::document::Document::add_path`] using `ox = 0, oy = page_h`,
/// which flips Y so the path lands at `page_h - y`.
///
/// `radius` / `radius_v` are the per-corner **horizontal** and **vertical** radii
/// `[top-left, top-right, bottom-right, bottom-left]` (already clamped by the
/// layout). Each corner uses an SVG elliptical arc (`A rx ry …`), which `add_path`
/// expands to cubic Béziers — so the emitted content stream carries real `c`
/// curve operators at every non-zero corner. When a corner's `rx == ry` (the
/// circular default, where `radius_v` mirrors `radius`) the emitted arc is
/// byte-identical to the previous circular-only path. A corner whose horizontal
/// **or** vertical radius is zero degenerates to a straight `L` to the corner
/// point, so a box with one rounded corner still renders correctly.
fn rounded_rect_path(x: f64, y: f64, w: f64, h: f64, radius: [f64; 4], radius_v: [f64; 4]) -> String {
    let [tlh, trh, brh, blh] = radius;
    let [tlv, trv, brv, blv] = radius_v;
    // A corner is rounded only when BOTH its radii are positive.
    let on = |a: f64, b: f64| a > 0.0 && b > 0.0;
    // Path travels clockwise (SVG Y-down): start just right of the top-left
    // corner, across the top edge, then each side + corner arc, and close.
    // SVG arc `A rx ry x-rotation large-arc sweep ex ey`; sweep = 1 (clockwise).
    let mut d = String::with_capacity(200);
    let tl_x = if on(tlh, tlv) { tlh } else { 0.0 };
    d.push_str(&format!("M {} {} ", num(x + tl_x), num(y)));
    // Top edge → top-right corner.
    let tr_x = if on(trh, trv) { trh } else { 0.0 };
    let tr_y = if on(trh, trv) { trv } else { 0.0 };
    d.push_str(&format!("L {} {} ", num(x + w - tr_x), num(y)));
    if on(trh, trv) {
        d.push_str(&format!(
            "A {} {} 0 0 1 {} {} ",
            num(trh),
            num(trv),
            num(x + w),
            num(y + tr_y)
        ));
    }
    // Right edge → bottom-right corner.
    let br_x = if on(brh, brv) { brh } else { 0.0 };
    let br_y = if on(brh, brv) { brv } else { 0.0 };
    d.push_str(&format!("L {} {} ", num(x + w), num(y + h - br_y)));
    if on(brh, brv) {
        d.push_str(&format!(
            "A {} {} 0 0 1 {} {} ",
            num(brh),
            num(brv),
            num(x + w - br_x),
            num(y + h)
        ));
    }
    // Bottom edge → bottom-left corner.
    let bl_x = if on(blh, blv) { blh } else { 0.0 };
    let bl_y = if on(blh, blv) { blv } else { 0.0 };
    d.push_str(&format!("L {} {} ", num(x + bl_x), num(y + h)));
    if on(blh, blv) {
        d.push_str(&format!(
            "A {} {} 0 0 1 {} {} ",
            num(blh),
            num(blv),
            num(x),
            num(y + h - bl_y)
        ));
    }
    // Left edge → top-left corner.
    let tl_y = if on(tlh, tlv) { tlv } else { 0.0 };
    d.push_str(&format!("L {} {} ", num(x), num(y + tl_y)));
    if on(tlh, tlv) {
        d.push_str(&format!(
            "A {} {} 0 0 1 {} {} ",
            num(tlh),
            num(tlv),
            num(x + tl_x),
            num(y)
        ));
    }
    d.push('Z');
    d
}

/// Paint a `box-shadow` behind a box: the box offset by `(dx, dy)` and grown by
/// `spread` on every side, in the shadow colour, tracking the box's (possibly
/// elliptical) corners.
///
/// With **`blur == 0`** this is a single hard filled rect/rounded-path —
/// byte-for-byte the previous behaviour. With **`blur > 0`** a soft edge is built
/// without any blur/clip primitive: an opaque inner core (the spread box shrunk
/// by ~half the blur) plus a stack of concentric rings expanding outward to the
/// full blur radius, each a low-alpha rect whose overlaps accumulate into a
/// roughly-Gaussian falloff (a multi-pass box-blur approximation). The summed
/// peak alpha is held near the legacy single-rect value so existing blurred
/// shadows don't suddenly darken.
#[allow(clippy::too_many_arguments)]
fn paint_box_shadow(
    doc: &mut Document,
    page: u32,
    page_h: f64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    radius: [f64; 4],
    radius_v: [f64; 4],
    sh: &super::css::BoxShadow,
) {
    // Base shadow box: grow by spread on each side, offset by (dx, dy) (top-down).
    let sx = x - sh.spread + sh.dx;
    let sy = y - sh.spread + sh.dy;
    let sw = w + 2.0 * sh.spread;
    let shh = h + 2.0 * sh.spread;
    if sw <= 0.0 || shh <= 0.0 {
        return; // a large negative spread can collapse the shadow — draw nothing
    }
    // Grow the corner radii by spread so the shadow's corners track the box.
    let grow = |arr: [f64; 4]| {
        [
            if arr[0] > 0.0 { (arr[0] + sh.spread).max(0.0) } else { 0.0 },
            if arr[1] > 0.0 { (arr[1] + sh.spread).max(0.0) } else { 0.0 },
            if arr[2] > 0.0 { (arr[2] + sh.spread).max(0.0) } else { 0.0 },
            if arr[3] > 0.0 { (arr[3] + sh.spread).max(0.0) } else { 0.0 },
        ]
    };
    let rh = grow(radius);
    let rv = grow(radius_v);

    // Emit one filled (rounded or square) shadow box at a given offset rect.
    let mut fill_box = |bx: f64, by: f64, bw: f64, bh: f64, rh: [f64; 4], rv: [f64; 4], a: f64| {
        if bw <= 0.0 || bh <= 0.0 {
            return;
        }
        if rh.iter().any(|v| *v > 0.0) || rv.iter().any(|v| *v > 0.0) {
            let d = rounded_rect_path(bx, by, bw, bh, rh, rv);
            let _ = doc.add_path(page, &d, 0.0, page_h, None, Some(sh.color), 0.0, a);
        } else {
            let _ = doc.add_rectangle(
                page,
                bx,
                page_h - by - bh,
                bw,
                bh,
                None,
                Some(sh.color),
                0.0,
                a,
            );
        }
    };

    if sh.blur <= 0.0 {
        // Hard shadow — unchanged single box at the legacy alpha (1.0).
        fill_box(sx, sy, sw, shh, rh, rv, 1.0);
        return;
    }

    // Soft shadow. The blur fans out ~½·blur each side of the spread edge, so the
    // umbra (fully-opaque core) shrinks by ½·blur and the penumbra extends ½·blur
    // outward. Keep the peak ≈ the legacy dimmed alpha so we don't darken.
    let reach = sh.blur * 0.5;
    let peak = (1.0 / (1.0 + sh.blur / 8.0)).clamp(0.15, 1.0);

    // Opaque-ish core (the spread box shrunk by `reach`, never past its centre).
    let core_inset = reach.min(sw / 2.0 - 0.5).min(shh / 2.0 - 0.5).max(0.0);
    let core_r = |r: [f64; 4]| {
        [
            (r[0] - core_inset).max(0.0),
            (r[1] - core_inset).max(0.0),
            (r[2] - core_inset).max(0.0),
            (r[3] - core_inset).max(0.0),
        ]
    };
    fill_box(
        sx + core_inset,
        sy + core_inset,
        sw - 2.0 * core_inset,
        shh - 2.0 * core_inset,
        core_r(rh),
        core_r(rv),
        peak,
    );

    // Penumbra rings: RINGS nested boxes from the core out to core+2·reach, each
    // at a small alpha. Overlaps accumulate to a smooth falloff. Per-ring alpha is
    // sized so the stack peaks near `peak` rather than summing far above it.
    const RINGS: usize = 6;
    let ring_alpha = (peak / RINGS as f64).clamp(0.02, 0.5);
    for i in 1..=RINGS {
        // Each ring grows the spread box outward toward the full penumbra; the
        // outermost (i = RINGS) reaches `+reach` past the spread edge.
        let grow_amt = (i as f64 / RINGS as f64) * reach;
        let bx = sx - grow_amt;
        let by = sy - grow_amt;
        let bw = sw + 2.0 * grow_amt;
        let bh = shh + 2.0 * grow_amt;
        let ring_r = |r: [f64; 4]| {
            [
                if r[0] > 0.0 { r[0] + grow_amt } else { 0.0 },
                if r[1] > 0.0 { r[1] + grow_amt } else { 0.0 },
                if r[2] > 0.0 { r[2] + grow_amt } else { 0.0 },
                if r[3] > 0.0 { r[3] + grow_amt } else { 0.0 },
            ]
        };
        fill_box(bx, by, bw, bh, ring_r(rh), ring_r(rv), ring_alpha);
    }
}

/// Paint one `dashed`/`dotted`/`double` border side as filled rectangles, the
/// same primitive every solid border already uses (so it composites and
/// paginates identically). The band `(x, y, w, h)` is top-down: a `horizontal`
/// side runs along `x` (length `w`, thickness `h`); a vertical side runs along
/// `y` (length `h`, thickness `w`). `width` is the border width.
///
/// - **dashed**: dash ≈ 3×width, gap ≈ 2×width — a row of short bars.
/// - **dotted**: dash = gap = width — square dots.
/// - **double**: two thin lines (≈ width/3 each) at the band's two edges with a
///   gap between, spanning the whole side.
///
/// A `Solid` side never reaches here (it stays a plain filled rect upstream).
#[allow(clippy::too_many_arguments)]
fn paint_styled_border(
    doc: &mut Document,
    page: u32,
    page_h: f64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    horizontal: bool,
    width: f64,
    color: [f64; 3],
    style: BorderStyle,
    opacity: f64,
) {
    if width <= 0.0 || w <= 0.0 || h <= 0.0 {
        return;
    }
    // Emit one top-down rect → PDF (origin bottom-left, Y-up).
    let mut fill = |rx: f64, ry: f64, rw: f64, rh: f64| {
        if rw > 0.0 && rh > 0.0 {
            let _ = doc.add_rectangle(
                page,
                rx,
                page_h - ry - rh,
                rw,
                rh,
                None,
                Some(color),
                0.0,
                opacity,
            );
        }
    };

    match style {
        BorderStyle::Double => {
            // Two parallel lines ≈ width/3 thick at the band's far edges.
            let t = (width / 3.0).max(0.3);
            if horizontal {
                fill(x, y, w, t); // top line of the band
                fill(x, y + h - t, w, t); // bottom line
            } else {
                fill(x, y, t, h); // left line of the band
                fill(x + w - t, y, t, h); // right line
            }
        }
        // Dashed / dotted: march filled bars along the side's long axis.
        _ => {
            let (dash, gap) = match style {
                BorderStyle::Dotted => (width.max(0.3), width.max(0.3)),
                _ => (width * 3.0, width * 2.0), // Dashed
            };
            let period = (dash + gap).max(0.1);
            let length = if horizontal { w } else { h };
            let mut pos = 0.0;
            while pos < length - 0.01 {
                let seg = dash.min(length - pos); // clip the final dash
                if horizontal {
                    fill(x + pos, y, seg, h);
                } else {
                    fill(x, y + pos, w, seg);
                }
                pos += period;
            }
        }
    }
}

/// Paint any CSS gradient background filling the box `(x, y, w, h)` (top-down),
/// dispatching by kind: `linear`/`radial` go through the engine's PDF shading
/// machinery (a real gradient, not a raster), `conic` is approximated by a fan of
/// flat-coloured vector sectors. A zero-area box draws nothing.
#[allow(clippy::too_many_arguments)]
fn paint_css_gradient(
    doc: &mut Document,
    page: u32,
    page_h: f64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    grad: &CssGradient,
    opacity: f64,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    match grad {
        CssGradient::Linear(g) => paint_linear_gradient(doc, page, page_h, x, y, w, h, g, opacity),
        CssGradient::Radial(g) => paint_radial_gradient(doc, page, page_h, x, y, w, h, g, opacity),
        CssGradient::Conic(g) => paint_conic_gradient(doc, page, page_h, x, y, w, h, g, opacity),
    }
}

/// Paint a CSS `radial-gradient` filling the box `(x, y, w, h)` (top-down) as a
/// **true PDF radial shading** clipped to the box, reusing the SVG layer's
/// `GradKind::Radial` (`/ShadingType 3`). The centre and end radius come from the
/// parsed fractions; the colour ramp runs centre→edge.
#[allow(clippy::too_many_arguments)]
fn paint_radial_gradient(
    doc: &mut Document,
    page: u32,
    page_h: f64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    grad: &RadialGradient,
    opacity: f64,
) {
    let Some(img) = radial_gradient_svg_image(grad, w, h, opacity.clamp(0.0, 1.0)) else {
        return;
    };
    let _ = doc.draw_svg_image(page, &img, x, page_h - y - h, w, h);
}

/// Paint a CSS `conic-gradient` filling the box `(x, y, w, h)` (top-down) as a
/// fan of flat-coloured triangular sectors radiating from the centre. There is no
/// native PDF conic shading, so this vector approximation samples the angular
/// colour ramp at a fine step; the sectors over-cover the box (their outer radius
/// reaches the farthest corner) and the SVG is clipped to the box on placement.
#[allow(clippy::too_many_arguments)]
fn paint_conic_gradient(
    doc: &mut Document,
    page: u32,
    page_h: f64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    grad: &ConicGradient,
    opacity: f64,
) {
    let Some(img) = conic_gradient_svg_image(grad, w, h, opacity.clamp(0.0, 1.0)) else {
        return;
    };
    let _ = doc.draw_svg_image(page, &img, x, page_h - y - h, w, h);
}

/// Build a one-primitive [`crate::svg::SvgImage`] — a `w×h` rectangle filled with
/// an SVG **radial** gradient — from a CSS [`RadialGradient`]. The centre
/// `(cx, cy)` and radius `r` are resolved from the box dimensions (`cx`/`cy` are
/// fractions of `w`/`h`; `r` is a fraction of `min(w,h)/2`). `None` if there are
/// fewer than two stops.
fn radial_gradient_svg_image(
    grad: &RadialGradient,
    w: f64,
    h: f64,
    alpha: f64,
) -> Option<crate::svg::SvgImage> {
    use crate::content::svg_path::Seg;
    use crate::svg::{Fill, GradKind, GradStop, Gradient, Prim, SvgImage};

    if grad.stops.len() < 2 {
        return None;
    }
    let cx = grad.cx * w;
    let cy = grad.cy * h;
    let r = (grad.r * (w.min(h) / 2.0)).max(1e-3);

    let offsets = resolve_stop_offsets(&grad.stops);
    let stops: Vec<GradStop> = grad
        .stops
        .iter()
        .zip(offsets)
        .map(|(st, off)| GradStop {
            offset: off,
            rgb: st.color,
            alpha,
        })
        .collect();

    let segs = vec![
        Seg::Move(0.0, 0.0),
        Seg::Line(w, 0.0),
        Seg::Line(w, h),
        Seg::Line(0.0, h),
        Seg::Close,
    ];
    let prim = Prim {
        segs,
        fill: Some(Fill::Gradient(Gradient {
            kind: GradKind::Radial {
                cx,
                cy,
                r,
                fx: cx,
                fy: cy,
            },
            stops,
        })),
        stroke: None,
        stroke_w: 0.0,
        fill_opacity: 1.0,
        stroke_opacity: 1.0,
    };
    Some(SvgImage {
        width: w,
        height: h,
        view_box: [0.0, 0.0, w, h],
        prims: vec![prim],
    })
}

/// Build a multi-primitive [`crate::svg::SvgImage`] approximating a CSS
/// `conic-gradient`: a fan of flat-coloured triangular sectors around the centre
/// `(cx, cy)` (fractions of the box), each spanning a small angle and coloured by
/// sampling the angular ramp at its mid-angle. The sectors reach past the box
/// corners (radius = the farthest corner distance) so the whole box is covered;
/// placement clips the SVG to the box. `None` if there are fewer than two stops.
fn conic_gradient_svg_image(
    grad: &ConicGradient,
    w: f64,
    h: f64,
    alpha: f64,
) -> Option<crate::svg::SvgImage> {
    use crate::content::svg_path::Seg;
    use crate::svg::{Fill, Prim, SvgImage};

    if grad.stops.len() < 2 {
        return None;
    }
    let cx = grad.cx * w;
    let cy = grad.cy * h;
    // Cover to the farthest corner so no box area is left unpainted.
    let far = |px: f64, py: f64| ((cx - px).powi(2) + (cy - py).powi(2)).sqrt();
    let radius = far(0.0, 0.0)
        .max(far(w, 0.0))
        .max(far(w, h))
        .max(far(0.0, h))
        + 1.0;

    let offsets = resolve_stop_offsets(&grad.stops);
    // Enough sectors that the flat-fill banding is invisible at print scale.
    const SECTORS: usize = 180;
    let mut prims: Vec<Prim> = Vec::with_capacity(SECTORS);
    let step = 1.0 / SECTORS as f64;
    for i in 0..SECTORS {
        let t0 = i as f64 * step;
        let t1 = (i + 1) as f64 * step;
        let tmid = (t0 + t1) * 0.5;
        let rgb = sample_ramp(&grad.stops, &offsets, tmid);
        // CSS conic: `0` points up and sweeps clockwise. Convert the turn fraction
        // to an SVG-(Y-down) angle measured from the +Y-up "north", clockwise.
        let a0 = conic_angle(grad.from_deg, t0);
        let a1 = conic_angle(grad.from_deg, t1);
        let (s0, c0) = a0.sin_cos();
        let (s1, c1) = a1.sin_cos();
        // Sector triangle centre → arc edge at t0 → arc edge at t1 (flat fill; the
        // arc chord is negligible at 2°/sector).
        let segs = vec![
            Seg::Move(cx, cy),
            Seg::Line(cx + radius * s0, cy - radius * c0),
            Seg::Line(cx + radius * s1, cy - radius * c1),
            Seg::Close,
        ];
        prims.push(Prim {
            segs,
            fill: Some(Fill::Solid(rgb)),
            stroke: None,
            stroke_w: 0.0,
            fill_opacity: alpha,
            stroke_opacity: 1.0,
        });
    }
    Some(SvgImage {
        width: w,
        height: h,
        view_box: [0.0, 0.0, w, h],
        prims,
    })
}

/// Angle (radians) of a conic-gradient sample at turn-fraction `t`, given the CSS
/// `from` start angle in degrees (`0` = up, clockwise). The returned angle is the
/// clockwise rotation from the upward axis, so a caller using `(sin, -cos)` lands
/// the point in SVG Y-down space.
fn conic_angle(from_deg: f64, t: f64) -> f64 {
    (from_deg + t * 360.0).to_radians()
}

/// Sample a gradient colour ramp (stops + their resolved `0..=1` offsets) at
/// position `t`, linearly interpolating between the bracketing stops. Clamps to
/// the end colours outside the stop range.
fn sample_ramp(
    stops: &[super::css::GradientStop],
    offsets: &[f64],
    t: f64,
) -> [f64; 3] {
    if stops.is_empty() {
        return [0.0, 0.0, 0.0];
    }
    if t <= offsets[0] {
        return stops[0].color;
    }
    let last = stops.len() - 1;
    if t >= offsets[last] {
        return stops[last].color;
    }
    for i in 0..last {
        let (a, b) = (offsets[i], offsets[i + 1]);
        if t >= a && t <= b {
            let span = (b - a).max(1e-9);
            let f = ((t - a) / span).clamp(0.0, 1.0);
            let ca = stops[i].color;
            let cb = stops[i + 1].color;
            return [
                ca[0] + (cb[0] - ca[0]) * f,
                ca[1] + (cb[1] - ca[1]) * f,
                ca[2] + (cb[2] - ca[2]) * f,
            ];
        }
    }
    stops[last].color
}

/// Paint a CSS `linear-gradient` filling the box `(x, y, w, h)` (top-down) as a
/// **true PDF axial shading** clipped to the box.
///
/// The gradient is expressed as a one-rect [`crate::svg::SvgImage`] whose fill is
/// an SVG linear gradient; [`Document::draw_svg_image`] then reuses the engine's
/// existing axial-shading + tiling-pattern machinery (a `/ShadingType 2` with a
/// sampled colour ramp), so no new PDF object plumbing is needed and the result
/// is a real gradient, not a raster. Endpoints follow the CSS angle convention
/// (`0deg` up, `90deg` right, clockwise); stops without a position are spread
/// evenly between their positioned neighbours.
#[allow(clippy::too_many_arguments)]
fn paint_linear_gradient(
    doc: &mut Document,
    page: u32,
    page_h: f64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    grad: &LinearGradient,
    opacity: f64,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let Some(img) = gradient_svg_image(grad, w, h, opacity.clamp(0.0, 1.0)) else {
        return;
    };
    // The image's viewBox is [0,0,w,h]; place it on the PDF box (Y-flipped). The
    // box opacity rides on the gradient stops' alpha (draw_svg_image folds the
    // mean stop alpha into a transient ExtGState).
    let _ = doc.draw_svg_image(page, &img, x, page_h - y - h, w, h);
}

/// Build a one-primitive [`crate::svg::SvgImage`] — a `w×h` rectangle filled with
/// an axial SVG gradient — from a CSS [`LinearGradient`]. The viewBox is
/// `[0,0,w,h]` (Y-down, matching SVG), so the caller maps it straight onto the
/// PDF box. Gradient-line endpoints are derived from the CSS angle; `None` only
/// if there are fewer than two stops (already guaranteed by the parser, but
/// defensive). `alpha` (the box opacity, `0..=1`) is applied uniformly to every
/// stop.
fn gradient_svg_image(
    grad: &LinearGradient,
    w: f64,
    h: f64,
    alpha: f64,
) -> Option<crate::svg::SvgImage> {
    use crate::content::svg_path::Seg;
    use crate::svg::{Fill, GradKind, GradStop, Gradient, Prim, SvgImage};

    if grad.stops.len() < 2 {
        return None;
    }

    // CSS angle (0 = up, clockwise) → gradient-line direction in SVG (Y-down)
    // space: math direction is (sin θ, cos θ) with Y up, so Y-down flips cos.
    let theta = grad.angle_deg.to_radians();
    let (s, c) = (theta.sin(), theta.cos());
    let dir = (s, -c); // unit vector, SVG Y-down
                       // Line length so 0%/100% reach the box extent along the line.
    let len = (w * s).abs() + (h * c).abs();
    let (cx, cy) = (w / 2.0, h / 2.0);
    let half = len / 2.0;
    let (x1, y1) = (cx - dir.0 * half, cy - dir.1 * half); // first stop
    let (x2, y2) = (cx + dir.0 * half, cy + dir.1 * half); // last stop

    // Resolve stop positions: fill `None`s evenly between positioned neighbours
    // (CSS default-placement), clamp to monotonic non-decreasing offsets.
    let offsets = resolve_stop_offsets(&grad.stops);
    let stops: Vec<GradStop> = grad
        .stops
        .iter()
        .zip(offsets)
        .map(|(st, off)| GradStop {
            offset: off,
            rgb: st.color,
            alpha,
        })
        .collect();

    // A closed rectangle path over the whole viewBox, filled with the gradient.
    let segs = vec![
        Seg::Move(0.0, 0.0),
        Seg::Line(w, 0.0),
        Seg::Line(w, h),
        Seg::Line(0.0, h),
        Seg::Close,
    ];
    let prim = Prim {
        segs,
        fill: Some(Fill::Gradient(Gradient {
            kind: GradKind::Linear { x1, y1, x2, y2 },
            stops,
        })),
        stroke: None,
        stroke_w: 0.0,
        fill_opacity: 1.0,
        stroke_opacity: 1.0,
    };
    Some(SvgImage {
        width: w,
        height: h,
        view_box: [0.0, 0.0, w, h],
        prims: vec![prim],
    })
}

/// Resolve each gradient stop to a concrete `0..=1` offset: explicit positions
/// are kept (clamped non-decreasing), and a run of `None` positions is spread
/// evenly between the surrounding fixed offsets (CSS stop placement). The
/// first/last default to 0 and 1 when unspecified. Shared by linear / radial /
/// conic — they all place stops along a normalised `0..=1` axis.
fn resolve_stop_offsets(stops: &[super::css::GradientStop]) -> Vec<f64> {
    let n = stops.len();
    // Seed with explicit positions; first→0, last→1 when absent.
    let mut off: Vec<Option<f64>> = stops.iter().map(|s| s.pos).collect();
    if off[0].is_none() {
        off[0] = Some(0.0);
    }
    if off[n - 1].is_none() {
        off[n - 1] = Some(1.0);
    }
    // Interpolate each gap of unspecified stops between known anchors.
    let mut out = vec![0.0; n];
    let mut i = 0;
    while i < n {
        if let Some(v) = off[i] {
            out[i] = v;
            i += 1;
            continue;
        }
        // `i` is the first unknown after anchor `i-1`; find the next known `j`.
        let lo = out[i - 1];
        let mut j = i;
        while j < n && off[j].is_none() {
            j += 1;
        }
        let hi = off[j].unwrap_or(1.0);
        let span = (j - (i - 1)) as f64;
        for (k, slot) in (i..j).enumerate() {
            out[slot] = lo + (hi - lo) * ((k + 1) as f64 / span);
        }
        i = j;
    }
    // Enforce monotonic non-decreasing offsets (a later smaller value clamps up).
    for k in 1..n {
        if out[k] < out[k - 1] {
            out[k] = out[k - 1];
        }
    }
    out
}

/// Resolve an `<img>` source to image bytes: a `data:` URI is decoded inline; any
/// other URL is looked up in the host-provided `resources` map (the engine never
/// fetches the network). Returns `None` when the URL wasn't supplied — the image
/// is simply omitted, exactly as a browser shows a broken image.
fn resolve_image(
    src: &str,
    resources: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Option<Vec<u8>> {
    if src.starts_with("data:") {
        decode_data_uri(src)
    } else {
        resources.get(src).cloned()
    }
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
    fn render_invoice_table_with_borders_and_shaded_header() {
        // A collapsed, fully-ruled table with a grey header row and per-side
        // border + background on cells. Exercises the whole pipeline (cascade →
        // table layout → per-side border rects + cell backgrounds → PDF). It must
        // produce a valid, non-trivial PDF without panicking.
        let html = r#"
            <table style="border-collapse:collapse">
              <tr>
                <th style="background:#dddddd;border:1pt solid #000000">Item</th>
                <th style="background:#dddddd;border:1pt solid #000000;text-align:right">Total</th>
              </tr>
              <tr>
                <td style="border:1pt solid #000000;vertical-align:middle">Widget</td>
                <td style="border:1pt solid #000000;border-bottom:2pt solid #ff0000;text-align:right">12.00</td>
              </tr>
            </table>"#;
        let pdf = render(html, &[], 612.0, 792.0, 36.0);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF header");
        assert!(
            pdf.len() > 400,
            "invoice table produced a non-trivial PDF ({} bytes)",
            pdf.len()
        );
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

    #[test]
    fn needed_resources_lists_external_image_urls() {
        let html = r#"<img src="https://x.test/logo.png" width="20" height="20">
                      <img src="data:image/png;base64,iVBORw0KGgo=">"#;
        let needs = needed_resources(html, None, None);
        let imgs: Vec<&str> = needs
            .iter()
            .filter_map(|n| match n {
                ResourceNeed::Image(u) => Some(u.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(imgs, vec!["https://x.test/logo.png"], "data: URI excluded");
    }

    #[test]
    fn unmatched_family_uses_bundled_face_for_metrics() {
        // No provided fonts and an unknown family: the MeasureBook must measure
        // with the bundled fallback's *real* advance widths, not the rough
        // character-count estimate (count · size · 0.5).
        let book = MeasureBook::new(&[]);
        let style = Style {
            font_family: "NoSuchFont".into(),
            font_size: 12.0,
            ..Style::default()
        };
        let w = book.width("Hello", &style);
        let rough = 5.0 * 12.0 * 0.5; // the old fallback estimate
        assert!(book.fallback.is_some(), "the bundled fallback face parsed");
        assert!(w > 0.0, "non-trivial measured width");
        assert!(
            (w - rough).abs() > 0.5,
            "width comes from real font metrics, not the rough estimate (w={w}, rough={rough})"
        );
        // Sanity: the real proportional width of "Hello" at 12pt sits well under
        // the rough 0.5-em-per-char figure.
        assert!(
            w < rough,
            "proportional metrics are tighter than the estimate (w={w})"
        );
    }

    #[test]
    fn render_with_no_fonts_embeds_selectable_bundled_glyphs() {
        // Text-bearing HTML with an unknown family and NO provided fonts: the
        // bundled fallback must be embedded with real glyphs AND a /ToUnicode
        // CMap, so the text round-trips on extraction (selectable/copyable) — not
        // tofu, and not invisible. Re-opening the produced PDF and reading the
        // text back is the definitive proof the bundled face was embedded.
        let phrase = "Hello bundled fallback";
        let pdf = render(
            &format!(r#"<p style="font-family:NoSuchFont">{phrase}</p>"#),
            &[],
            612.0,
            792.0,
            36.0,
        );
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");

        let doc = Document::open(&pdf).expect("re-open the rendered PDF");
        let runs = doc.page_text_runs(1).expect("page text runs");
        let text: String = runs.iter().map(|r| r.text.as_str()).collect();
        assert!(
            text.contains("Hello") && text.contains("fallback"),
            "bundled glyphs are real & selectable — extracted {text:?}"
        );
        assert_eq!(
            text.chars().filter(|&c| c == '\u{FFFD}').count(),
            0,
            "no tofu — the embedded /ToUnicode CMap maps the glyphs ({text:?})"
        );
    }

    #[test]
    fn provided_font_takes_precedence_over_bundled() {
        // A host-provided font must be the face used for a run — the online path
        // is unchanged and the bundled fallback never shadows it. Supply a real
        // parseable font (the bundled program reused under a different family) and
        // assert `face()` returns the *provided* face, not the bundled one.
        let provided = ProvidedFont {
            family: "Provided".into(),
            weight: 400,
            italic: false,
            ttf: bundled::FALLBACK_TTF.to_vec(),
        };
        let book = MeasureBook::new(&[provided]);
        let style = Style {
            font_family: "Provided".into(),
            font_size: 10.0,
            ..Style::default()
        };
        let chosen = book.face(&style).expect("a face is chosen");
        let provided_face = &book.provided(&style).expect("the provided face exists").ttf;
        assert!(
            std::ptr::eq(chosen, provided_face),
            "the provided face is used, not the bundled fallback"
        );
        assert!(
            !std::ptr::eq(chosen, &book.fallback.as_ref().unwrap().ttf),
            "the bundled fallback does not shadow a provided font"
        );
    }

    #[test]
    fn external_image_embeds_from_resources_map() {
        use super::super::page::RenderOptions;
        // A real 4x4 PNG the host "downloaded" for the external URL.
        let rgba = vec![200u8; 4 * 4 * 4];
        let png = crate::raster::png::encode_png(4, 4, &rgba);
        let url = "https://x.test/logo.png";
        let html = format!(r#"<img src="{url}" width="40" height="40">"#);

        // Without the resource: the image URL can't resolve → omitted.
        let without = render_with(&html, &[], &RenderOptions::new(612.0, 792.0));

        // With the host-provided bytes in the resources map: embedded.
        let mut opts = RenderOptions::new(612.0, 792.0);
        opts.resources.insert(url.to_string(), png.clone());
        let with = render_with(&html, &[], &opts);

        assert!(with.starts_with(b"%PDF-") && without.starts_with(b"%PDF-"));
        assert!(
            with.len() > without.len() + png.len() / 2,
            "the external image bytes were embedded (with={} vs without={})",
            with.len(),
            without.len()
        );
    }

    /// Build a one-page doc and return the `y` of every `re` (rectangle) op in
    /// its content stream — used to locate decoration rules deterministically.
    fn rect_ys(style: &Style, page_h: f64) -> Vec<f64> {
        let mut b = PdfBuilder::new();
        b.add_page(612.0, page_h);
        let mut doc = Document::open(&b.finish()).expect("open");
        // A 100pt-wide run at top-down y = 100 on the page.
        paint_text_decorations(&mut doc, 1, page_h, 50.0, 100.0, 100.0, style);
        let content = String::from_utf8_lossy(&doc.page_content(1).expect("content")).to_string();
        content
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                let rest = l.strip_suffix(" re")?;
                // `{x} {y} {w} {h}` — take the second field (y).
                rest.split_whitespace().nth(1)?.parse::<f64>().ok()
            })
            .collect()
    }

    #[test]
    fn underline_draws_a_rule_below_the_baseline() {
        let style = Style {
            underline: true,
            font_size: 20.0,
            ..Style::default()
        };
        let ys = rect_ys(&style, 800.0);
        assert_eq!(ys.len(), 1, "exactly one underline rule emitted");
        // Top-down run top = 100, baseline = 100 + 0.8·20 = 116; underline sits
        // 0.12·20 = 2.4 below ⇒ top-down 118.4 ⇒ PDF y = 800 − 118.4 = 681.6.
        assert!(
            (ys[0] - 681.6).abs() < 0.5,
            "underline rule just under the baseline (got {})",
            ys[0]
        );
    }

    #[test]
    fn line_through_draws_a_rule_at_mid_height() {
        let style = Style {
            strike: true,
            font_size: 20.0,
            ..Style::default()
        };
        let ys = rect_ys(&style, 800.0);
        assert_eq!(ys.len(), 1, "exactly one line-through rule emitted");
        // Baseline top-down 116; strike sits 0.30·20 = 6 above ⇒ top-down 110 ⇒
        // PDF y = 800 − 110 = 690. It must sit ABOVE the underline (681.6).
        assert!(
            (ys[0] - 690.0).abs() < 0.5,
            "line-through rule through the text (got {})",
            ys[0]
        );
        let under = {
            let s = Style {
                underline: true,
                font_size: 20.0,
                ..Style::default()
            };
            rect_ys(&s, 800.0)[0]
        };
        assert!(
            ys[0] > under,
            "strike (PDF y {}) sits above underline ({under})",
            ys[0]
        );
    }

    #[test]
    fn s_tag_strikes_through_end_to_end() {
        // The UA sheet maps <s>/<strike>/<del> to line-through; a full render of
        // `<s>` must therefore emit a decoration rule (no panic, valid PDF).
        let pdf = render("<p><s>gone</s></p>", &[], 612.0, 792.0, 36.0);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF for struck text");
    }

    // ── border-radius / box-shadow paint ────────────────────────────────────

    /// Render `html` and return page-1's content stream as text (re-opening the
    /// produced PDF), so tests can assert on the emitted PDF operators.
    fn page1_content(html: &str) -> String {
        let pdf = render(html, &[], 612.0, 792.0, 36.0);
        let doc = Document::open(&pdf).expect("re-open rendered PDF");
        String::from_utf8_lossy(&doc.page_content(1).expect("page content")).into_owned()
    }

    /// Count Bézier curve operators (`… c`) — present for any rounded corner.
    fn count_curves(content: &str) -> usize {
        content
            .lines()
            .filter(|l| l.trim_end().ends_with(" c"))
            .count()
    }

    #[test]
    fn rounded_box_emits_bezier_curves() {
        // A rounded background must produce real Bézier corner arcs (`c` ops) —
        // the rounded contour the fill follows. (A rounded *fill* follows the
        // path itself rather than a `W n` clip; the `W n` clip primitive is used
        // only to realise `overflow: hidden|clip` — see the box-decoration helper
        // and `overflow_hidden_emits_a_real_clip_for_straddling_content`.)
        let content = page1_content(
            r#"<div style="background:#3366cc;border-radius:12pt;padding:10pt">x</div>"#,
        );
        assert!(
            count_curves(&content) >= 4,
            "≥4 Bézier corner arcs for the four rounded corners ({} found)\n{content}",
            count_curves(&content)
        );
        // The fill colour is painted (the blue background) and the path is filled.
        assert!(content.contains("0.2 0.4 0.8 rg"), "blue fill set");
    }

    #[test]
    fn overflow_hidden_emits_a_real_clip_for_straddling_content() {
        // Every `overflow: hidden` box whose content straddles an edge must emit a
        // real `q … re W n … Q` clip. Three independent overflow vectors:

        // (1) Horizontal — a child wider than the box (explicit `width`).
        let horizontal = page1_content(
            r#"<div style="overflow:hidden;width:50pt"><div style="width:300pt;height:30pt;background:#888888"></div></div>"#,
        );
        assert!(
            horizontal.contains("W n"),
            "horizontal overflow clips\n{horizontal}"
        );

        // (2) Vertical — a definite `height` shorter than the content. `height` is
        // now definite (caps the box), not a min-height that would just grow it.
        let vertical = page1_content(
            r#"<div style="overflow:hidden;height:20pt"><div style="height:200pt;background:#888888">x</div></div>"#,
        );
        assert!(
            vertical.contains("W n"),
            "vertical overflow clips\n{vertical}"
        );

        // (3) Text — a no-wrap run wider than the box. Text runs now carry a real
        // advance width, so an overflowing run registers as straddling.
        let text = page1_content(
            r#"<div style="overflow:hidden;width:30pt;white-space:nowrap">wwwwwwwwwwwwwwwwwwww</div>"#,
        );
        assert!(text.contains("W n"), "text overflow clips\n{text}");

        // Control — the horizontal box WITHOUT overflow:hidden emits no clip, so
        // the `W n` above is the overflow clip, not an always-on artifact.
        let unclipped = page1_content(
            r#"<div style="width:50pt"><div style="width:300pt;height:30pt;background:#888888"></div></div>"#,
        );
        assert!(
            !unclipped.contains("W n"),
            "no overflow ⇒ no clip\n{unclipped}"
        );
    }

    #[test]
    fn rgba_colour_applies_alpha_via_extgstate() {
        // A semi-transparent background folds its alpha into the fill opacity,
        // which the painter realises with an `/ExtGState … gs`. An opaque colour
        // uses none — so the `gs` is the alpha, not an always-on artifact.
        let translucent =
            page1_content(r#"<div style="background:rgba(0,0,0,0.5);padding:10pt">x</div>"#);
        assert!(
            translucent.contains(" gs"),
            "rgba background sets an opacity ExtGState\n{translucent}"
        );
        let opaque = page1_content(r#"<div style="background:rgb(0,0,0);padding:10pt">x</div>"#);
        assert!(
            !opaque.contains(" gs"),
            "opaque background uses no ExtGState\n{opaque}"
        );
    }

    #[test]
    fn square_box_uses_rectangle_op_not_curves() {
        // Guard: with no radius the background still paints via the `re` rectangle
        // operator and emits NO Bézier corners (unchanged fast path).
        let content = page1_content(r#"<div style="background:#3366cc;padding:10pt">x</div>"#);
        assert!(
            content.contains(" re"),
            "square background uses the `re` rectangle op\n{content}"
        );
        assert_eq!(
            count_curves(&content),
            0,
            "no Bézier corners for a square box\n{content}"
        );
    }

    #[test]
    fn box_shadow_paints_a_shadow_fill_before_the_box() {
        // The shadow (red here, for a visible marker) must be filled BEFORE the
        // box's own background (grey), so it sits behind it. We look for the red
        // shadow fill appearing earlier in the stream than the grey box fill.
        let content = page1_content(
            r#"<div style="background:#cccccc;box-shadow:4pt 4pt 6pt #ff0000;padding:10pt">x</div>"#,
        );
        let red = content.find("1 0 0 rg");
        let grey = content.find("0.8 0.8 0.8 rg");
        assert!(red.is_some(), "shadow fill (red) is emitted\n{content}");
        assert!(grey.is_some(), "box background (grey) is emitted");
        assert!(
            red.unwrap() < grey.unwrap(),
            "shadow paints behind (before) the box background"
        );
    }

    #[test]
    fn rounded_box_with_shadow_renders_valid_pdf() {
        // End-to-end smoke: a rounded card with a blurred drop shadow renders a
        // valid, non-trivial PDF without panicking.
        let pdf = render(
            r#"<div style="background:#ffffff;border:2pt solid #334155;border-radius:14pt;box-shadow:0pt 6pt 12pt 2pt #00000040;padding:16pt">Card</div>"#,
            &[],
            612.0,
            792.0,
            36.0,
        );
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        assert!(pdf.len() > 400, "non-trivial output ({} bytes)", pdf.len());
    }

    // ── styled borders (dashed/dotted/double) / linear-gradient paint ────────

    /// Count `re` (rectangle) path operators in a one-page render's content
    /// stream — each border filled-rect / dash segment is one `re`.
    fn rect_op_count(html: &str) -> usize {
        let pdf = render(html, &[], 300.0, 200.0, 10.0);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        let doc = Document::open(&pdf).expect("re-open");
        let content = String::from_utf8_lossy(&doc.page_content(1).expect("content")).to_string();
        content.matches(" re").count()
    }

    #[test]
    fn solid_border_stays_one_filled_rect_per_side() {
        // The legacy path is unchanged: an all-four-sides solid border emits
        // exactly four filled rectangles — never the styled-border segments.
        let n =
            rect_op_count(r#"<div style="border:2pt solid #000000;width:100pt;height:40pt"></div>"#);
        assert_eq!(n, 4, "solid border = one filled rect per side (got {n})");
    }

    #[test]
    fn dashed_border_emits_many_dash_segments() {
        // A dashed border marches a row of short bars along each side, so it
        // emits far MORE rectangles than the 4 a solid border would.
        let dashed = rect_op_count(
            r#"<div style="border:2pt dashed #000000;width:100pt;height:40pt"></div>"#,
        );
        let solid =
            rect_op_count(r#"<div style="border:2pt solid #000000;width:100pt;height:40pt"></div>"#);
        assert!(
            dashed > solid * 3,
            "dashed border splits into many segments (dashed={dashed} vs solid={solid})"
        );
    }

    #[test]
    fn dotted_border_dots_march_along_the_side() {
        // A single 90pt dotted side at 3pt width: square dots with period
        // = dash + gap = 2·width = 6pt ⇒ ~15 dots. Several, but bounded.
        let n = rect_op_count(
            r#"<div style="border-bottom:3pt dotted #000000;width:90pt;height:30pt"></div>"#,
        );
        assert!(
            (8..=20).contains(&n),
            "one dotted side ≈ a dozen square dots (got {n})"
        );
    }

    #[test]
    fn double_border_draws_two_parallel_lines() {
        // A single `double` side renders as exactly two thin parallel rects.
        let n = rect_op_count(
            r#"<div style="border-top:6pt double #000000;width:90pt;height:30pt"></div>"#,
        );
        assert_eq!(n, 2, "double border = two parallel lines per side (got {n})");
    }

    #[test]
    fn linear_gradient_background_emits_axial_shading() {
        // A `linear-gradient` background paints a real PDF axial shading
        // (`/ShadingType 2`), referenced from the page via a `/Pattern` fill.
        let pdf = render(
            r#"<div style="background:linear-gradient(90deg,#ff0000,#0000ff);width:100pt;height:40pt"></div>"#,
            &[],
            300.0,
            200.0,
            10.0,
        );
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        let raw = String::from_utf8_lossy(&pdf);
        assert!(
            raw.contains("/ShadingType"),
            "an axial shading dictionary was written"
        );
        let doc = Document::open(&pdf).expect("re-open");
        let content = String::from_utf8_lossy(&doc.page_content(1).expect("content")).to_string();
        assert!(
            content.contains("Pattern cs") && content.contains("scn"),
            "the box fills with the shading pattern (content: {content:?})"
        );
    }

    #[test]
    fn gradient_geometry_maps_css_angle_to_endpoints() {
        use crate::html::css::{GradientStop, LinearGradient};
        use crate::svg::Fill;
        let stops = vec![
            GradientStop {
                color: [1.0, 0.0, 0.0],
                pos: None,
            },
            GradientStop {
                color: [0.0, 0.0, 1.0],
                pos: None,
            },
        ];
        // `90deg` ≡ "to right": gradient line runs left→right across the box,
        // centred vertically. Box 100×40 ⇒ start (0,20), end (100,20).
        let img = gradient_svg_image(
            &LinearGradient {
                angle_deg: 90.0,
                stops: stops.clone(),
            },
            100.0,
            40.0,
            1.0,
        )
        .expect("an image is built");
        let prim = &img.prims[0];
        let Some(Fill::Gradient(g)) = &prim.fill else {
            panic!("the rect is gradient-filled");
        };
        match g.kind {
            crate::svg::GradKind::Linear { x1, y1, x2, y2 } => {
                assert!(
                    (x1 - 0.0).abs() < 0.01 && (x2 - 100.0).abs() < 0.01,
                    "x endpoints span the width ({x1}→{x2})"
                );
                assert!(
                    (y1 - 20.0).abs() < 0.01 && (y2 - 20.0).abs() < 0.01,
                    "y stays centred ({y1},{y2})"
                );
            }
            _ => panic!("axial (linear) gradient"),
        }
        assert_eq!(g.stops.len(), 2, "both stops carried");
        // First stop at 0, second at 1 (auto-placed ends).
        assert!((g.stops[0].offset - 0.0).abs() < 0.01 && (g.stops[1].offset - 1.0).abs() < 0.01);

        // `180deg` ≡ "to bottom" (the default): line runs top→bottom, centred
        // horizontally. Box 100×40 ⇒ start (50,0), end (50,40) in SVG Y-down.
        let img2 = gradient_svg_image(
            &LinearGradient {
                angle_deg: 180.0,
                stops,
            },
            100.0,
            40.0,
            1.0,
        )
        .unwrap();
        if let Some(Fill::Gradient(g2)) = &img2.prims[0].fill {
            if let crate::svg::GradKind::Linear { x1, y1, x2, y2 } = g2.kind {
                assert!(
                    (x1 - 50.0).abs() < 0.01 && (x2 - 50.0).abs() < 0.01,
                    "x centred ({x1},{x2})"
                );
                assert!(
                    (y1 - 0.0).abs() < 0.01 && (y2 - 40.0).abs() < 0.01,
                    "y spans top→bottom ({y1}→{y2})"
                );
            }
        }
    }

    #[test]
    fn gradient_stop_positions_auto_placed_between_anchors() {
        // Three stops, middle one unpositioned ⇒ it lands midway (0.5).
        use crate::html::css::{GradientStop, LinearGradient};
        let g = LinearGradient {
            angle_deg: 90.0,
            stops: vec![
                GradientStop {
                    color: [1.0, 0.0, 0.0],
                    pos: Some(0.0),
                },
                GradientStop {
                    color: [0.0, 1.0, 0.0],
                    pos: None,
                },
                GradientStop {
                    color: [0.0, 0.0, 1.0],
                    pos: Some(1.0),
                },
            ],
        };
        let offs = resolve_stop_offsets(&g.stops);
        assert_eq!(offs.len(), 3);
        assert!(
            (offs[1] - 0.5).abs() < 0.01,
            "middle stop auto-placed at 0.5 (got {})",
            offs[1]
        );
    }

    #[test]
    fn styled_borders_and_gradient_render_without_panic() {
        // A box that combines a dashed border, a double side, and a gradient
        // background must produce a valid, non-trivial PDF (the whole pipeline).
        let html = r#"<div style="background:linear-gradient(45deg,#ffcc00,#cc00ff 80%);
                                   border:3pt dashed #003366;
                                   border-bottom:4pt double #ff0000;
                                   width:160pt;height:80pt"></div>"#;
        let pdf = render(html, &[], 300.0, 200.0, 12.0);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        assert!(pdf.len() > 400, "non-trivial output ({} bytes)", pdf.len());
    }

    #[test]
    fn radial_gradient_renders_a_valid_pdf() {
        let html = r#"<div style="background:radial-gradient(circle at 30% 40%, #ffcc00, #cc00ff);
                                   width:160pt;height:80pt"></div>"#;
        let pdf = render(html, &[], 300.0, 200.0, 12.0);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        assert!(pdf.len() > 400, "non-trivial output ({} bytes)", pdf.len());
    }

    #[test]
    fn conic_gradient_renders_a_valid_pdf() {
        let html = r#"<div style="background:conic-gradient(from 45deg at 50% 50%,
                                   #ff0000, #00ff00, #0000ff, #ff0000);
                                   width:80pt;height:80pt"></div>"#;
        let pdf = render(html, &[], 300.0, 200.0, 12.0);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        // The sector fan adds many path prims, so the stream is sizeable.
        assert!(pdf.len() > 600, "non-trivial conic output ({} bytes)", pdf.len());
    }

    #[test]
    fn rounded_rect_path_circular_is_unchanged_and_elliptical_differs() {
        // Circular (h == v): the emitted arcs use equal rx/ry (the pre-existing
        // path), so a 10pt corner shows "A 10 10".
        let circ = rounded_rect_path(0.0, 0.0, 100.0, 60.0, [10.0; 4], [10.0; 4]);
        assert!(circ.contains("A 10 10"), "circular arc rx==ry: {circ}");
        // Elliptical (h != v): a 10×4 corner emits "A 10 4".
        let ell = rounded_rect_path(0.0, 0.0, 100.0, 60.0, [10.0; 4], [4.0; 4]);
        assert!(ell.contains("A 10 4"), "elliptical arc rx!=ry: {ell}");
        // Both are valid closed paths.
        assert!(circ.trim_end().ends_with('Z') && ell.trim_end().ends_with('Z'));
    }

    #[test]
    fn elliptical_rounded_box_renders_a_valid_pdf() {
        let html = r#"<div style="background:#3366cc;border-radius:20pt / 8pt;
                                   width:120pt;height:60pt">x</div>"#;
        let pdf = render(html, &[], 300.0, 200.0, 12.0);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        assert!(pdf.len() > 400, "non-trivial output ({} bytes)", pdf.len());
    }

    #[test]
    fn blurred_box_shadow_renders_and_adds_content_vs_hard_shadow() {
        // A blurred shadow (soft rings) must produce a strictly larger content
        // stream than the same shadow with zero blur (single hard rect), proving
        // the soft-edge rings are actually emitted.
        let hard = render(
            r#"<div style="background:#ffffff;box-shadow:4pt 4pt 0pt #000000;padding:10pt">x</div>"#,
            &[],
            200.0,
            120.0,
            12.0,
        );
        let soft = render(
            r#"<div style="background:#ffffff;box-shadow:4pt 4pt 12pt #000000;padding:10pt">x</div>"#,
            &[],
            200.0,
            120.0,
            12.0,
        );
        assert!(hard.starts_with(b"%PDF-") && soft.starts_with(b"%PDF-"));
        assert!(
            soft.len() > hard.len(),
            "blurred shadow emits more content than a hard one (soft={}, hard={})",
            soft.len(),
            hard.len()
        );
    }

    #[test]
    fn multi_layer_box_shadow_renders_a_valid_pdf() {
        let pdf = render(
            r#"<div style="background:#fff;box-shadow:1pt 1pt 3pt #000, 6pt 6pt 12pt #444;padding:10pt">x</div>"#,
            &[],
            220.0,
            140.0,
            12.0,
        );
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        assert!(pdf.len() > 400, "non-trivial output ({} bytes)", pdf.len());
    }

    #[test]
    fn synthetic_bold_factor_grades_by_weight() {
        let make = |w: u16| Style {
            font_weight: w,
            bold: w >= 600,
            ..Style::default()
        };
        // No bold face available → graduated widening.
        // Light/regular: never widened.
        assert_eq!(synthetic_bold_factor(&make(100), false), 1.0);
        assert_eq!(synthetic_bold_factor(&make(400), false), 1.0);
        assert_eq!(synthetic_bold_factor(&make(500), false), 1.0);
        // Canonical bold (600/700) is exactly 1.03 — unchanged from the legacy
        // single factor.
        assert!((synthetic_bold_factor(&make(600), false) - 1.03).abs() < 1e-9);
        assert!((synthetic_bold_factor(&make(700), false) - 1.03).abs() < 1e-9);
        // Heavier weights widen further, monotonically, up to ~1.06 at 900.
        let w800 = synthetic_bold_factor(&make(800), false);
        let w900 = synthetic_bold_factor(&make(900), false);
        assert!(w800 > 1.03 && w900 > w800, "800<900 widening: {w800} {w900}");
        assert!((w900 - 1.06).abs() < 1e-9, "900 → 1.06, got {w900}");
        // A real bold face suppresses synthetic widening entirely.
        assert_eq!(synthetic_bold_factor(&make(900), true), 1.0);
    }
}
