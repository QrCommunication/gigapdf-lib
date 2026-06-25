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

/// Phase-1 fetch-dedup key: `(family lc, bold, italic)`. The *fetch* list only
/// distinguishes regular vs bold (it maps to Google-Fonts 400/700 downloads); the
/// finer per-weight selection happens at render time against the provided faces.
type Key = (String, bool, bool);

fn key(family: &str, bold: bool, italic: bool) -> Key {
    (family.to_ascii_lowercase(), bold, italic)
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

/// One parsed host-provided face, carrying the **exact** numeric weight (not a
/// binary bold bit) so the CSS weight-matching algorithm can pick between
/// per-weight instances of the same family (e.g. 300/400/500/700).
struct ProvidedFace {
    family: String, // ASCII-lowercased family
    weight: u16,    // exact CSS weight (1–1000)
    italic: bool,
    face: Face,
}

/// Parsed faces used for line-breaking before any PDF object exists.
struct MeasureBook {
    faces: Vec<ProvidedFace>,
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
                Face::parse(&f.ttf).map(|face| ProvidedFace {
                    family: f.family.to_ascii_lowercase(),
                    weight: f.weight,
                    italic: f.italic,
                    face,
                })
            })
            .collect();
        MeasureBook {
            faces,
            fallback: Face::parse(bundled::FALLBACK_TTF),
        }
    }

    /// The nearest *host-provided* face for a style, plus the exact weight of the
    /// face that won, applying the CSS font-matching weight algorithm: among
    /// same-family + same-italic faces, the closest weight per [`weight_pref`];
    /// then same-family any italic; then any provided face. `None` when no font
    /// was provided at all (the caller then falls back to the bundled face).
    fn provided_match(&self, style: &Style) -> Option<(&Face, u16)> {
        let fam = style.font_family.to_ascii_lowercase();
        let want = style.font_weight;
        // 1. Same family + same italic: pick the best weight.
        self.faces
            .iter()
            .filter(|f| f.family == fam && f.italic == style.italic)
            .min_by_key(|f| weight_pref(f.weight, want))
            // 2. Same family, ignore italic: still honour weight matching.
            .or_else(|| {
                self.faces
                    .iter()
                    .filter(|f| f.family == fam)
                    .min_by_key(|f| weight_pref(f.weight, want))
            })
            // 3. Any provided face at all (last resort before the bundled font).
            .or_else(|| self.faces.first())
            .map(|f| (&f.face, f.weight))
    }

    /// The face (font + shaper) to *measure and draw* a run with, plus the weight
    /// of the matched provided face (used to grade synthetic faux-bold). A
    /// host-provided face wins (online path); otherwise the bundled fallback,
    /// reported at the run's requested weight so no synthetic widening is added on
    /// top of it. `None` only if even the bundled font failed to parse.
    fn resolve_face_weight(&self, style: &Style) -> Option<(&Face, u16)> {
        self.provided_match(style)
            .or_else(|| self.fallback.as_ref().map(|f| (f, style.font_weight)))
    }

    /// The face (font + shaper) to *measure and draw* a run with: a host-provided
    /// face when one exists (online path), otherwise the bundled fallback. `None`
    /// only if even the bundled font failed to parse.
    fn resolve_face(&self, style: &Style) -> Option<&Face> {
        self.resolve_face_weight(style).map(|(f, _)| f)
    }

    /// The TrueType program to measure and draw a run with (provided or bundled).
    fn face(&self, style: &Style) -> Option<&TrueTypeFont> {
        self.resolve_face(style).map(|f| &f.ttf)
    }
}

impl Measure for MeasureBook {
    fn width(&self, text: &str, style: &Style) -> f64 {
        if let Some((face, matched_weight)) = self.resolve_face_weight(style) {
            let w = shaped_run_width(&face.ttf, &face.shaper, text, style.font_size);
            // Synthetic-bold widening graded by how much heavier the requested
            // weight is than the face that actually matched: a request served by a
            // real same-or-heavier face needs none; the lighter the matched face,
            // the more it widens (e.g. 700 onto a 400-only family).
            w * synthetic_bold_factor(style.font_weight, matched_weight)
        } else {
            // Neither a provided nor the bundled face is available — rough
            // estimate (should not happen in practice).
            let per = if style.generic_mono { 0.6 } else { 0.5 };
            text.chars().count() as f64 * style.font_size * per
        }
    }
}

/// Advance-width multiplier emulating a heavier `font-weight` than the face that
/// was actually matched, graded by the **gap** `requested − matched`.
///
/// `requested` is the run's CSS weight; `matched` is the weight of the
/// host-provided (or bundled) face the painter resolved. The widening only kicks
/// in when the matched face is *lighter* than requested — a request served by a
/// real same-or-heavier face carries its extra width in the glyphs themselves and
/// needs none.
///
/// Calibration (kept gentle to avoid visibly mis-spacing text), expressed as
/// `1.0 + 0.03 · (gap / 300)` capped at +0.06:
/// * `gap ≤ 0` (matched ≥ requested) ⇒ `1.0`.
/// * `font-weight: bold` (700) onto a regular-only family ⇒ gap 300 ⇒ exactly
///   `1.03` — byte-identical to the previous single bold factor.
/// * `900` onto a 400-only family ⇒ gap 500, clamped ⇒ `1.06`, so `900` reads
///   heavier than `700`.
fn synthetic_bold_factor(requested: u16, matched: u16) -> f64 {
    let gap = requested.saturating_sub(matched) as f64;
    if gap <= 0.0 {
        return 1.0;
    }
    // 1.0 + 0.03·(gap/300), capped at +0.06 (reached by gap ≥ 600).
    (1.0 + 0.03 * (gap / 300.0)).min(1.06)
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

/// Ordering key implementing the CSS Fonts §font-matching **weight** algorithm:
/// smaller tuple = better candidate for the desired weight `want`. Feed it to
/// `min_by_key` over the same-family candidate faces.
///
/// The first element is the priority **band** (0 = best); the second is the
/// in-band tie-breaker (the weight distance, so the closest face within a band
/// wins). Per the spec, for a desired weight `want`:
/// * `want ∈ [400, 500]`: faces in `[want, 500]` ascending, then `< want`
///   descending, then `> 500` ascending.
/// * `want < 400`: faces `≤ want` descending, then `> want` ascending.
/// * `want > 500`: faces `≥ want` ascending, then `< want` descending.
fn weight_pref(face: u16, want: u16) -> (u8, u16) {
    let dist = face.abs_diff(want);
    if (400..=500).contains(&want) {
        if face >= want && face <= 500 {
            (0, dist) // within [want, 500], nearest first
        } else if face < want {
            (1, dist) // lighter than want, nearest (largest) first
        } else {
            (2, dist) // heavier than 500, nearest first
        }
    } else if want < 400 {
        if face <= want {
            (0, dist) // at-or-below want, nearest (heaviest) first
        } else {
            (1, dist) // above want, nearest first
        }
    } else {
        // want > 500
        if face >= want {
            (0, dist) // at-or-above want, nearest first
        } else {
            (1, dist) // below want, nearest (heaviest) first
        }
    }
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
            // `background-image: url(...)` in an inline `style` is an external image
            // too (a class-rule background still needs the cascade — not walked here).
            if let Some(u) = e.attr("style").and_then(super::css::extract_css_url) {
                if !u.starts_with("data:") && !out.iter().any(|x| x == &u) {
                    out.push(u);
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

/// Decode the inline `@font-face` rules in the document's author CSS into render
/// faces and prepend them to the caller-supplied `fonts`.
///
/// For each `@font-face` rule, its `src` URLs are tried in order: a `data:` URI
/// (base64 or raw/percent) is decoded to bytes and fed to
/// [`crate::font::webfont::sfnt_from_web_font`], which accepts raw ttf/otf, WOFF1
/// and WOFF2 (decompressing + reversing the WOFF2 glyf transform). The first src
/// that yields a parseable sfnt becomes a [`ProvidedFont`] for that family /
/// weight / style. Non-`data:` srcs (external path/URL) are skipped — the engine
/// never touches the network or disk.
///
/// The inline faces are placed **before** the caller's `fonts` so they are
/// available to the CSS family/weight/style matcher. The measurer and the painter
/// both pick a face with `min_by_key` over this same-ordered list, which returns
/// the *first* among equal-best candidates — so on an exact `(family, weight,
/// italic)` collision the inline `@font-face` face is the one used, and crucially
/// the painted glyphs always match the measured advances (the two never diverge).
fn fonts_with_inline_faces(nodes: &[Node], fonts: &[ProvidedFont]) -> Vec<ProvidedFont> {
    let css = collect_style_css(nodes);
    let sheet = Stylesheet::new(&css);
    let rules = sheet.font_face_rules();
    if rules.is_empty() {
        return fonts.to_vec();
    }
    let mut combined: Vec<ProvidedFont> = Vec::with_capacity(rules.len() + fonts.len());
    for rule in rules {
        for src in &rule.srcs {
            if !src.starts_with("data:") {
                continue; // external src — not fetched (engine is zero-network)
            }
            let Some(bytes) = decode_data_uri(src) else {
                continue;
            };
            if let Some(ttf) = crate::font::webfont::sfnt_from_web_font(&bytes) {
                combined.push(ProvidedFont {
                    family: rule.family.clone(),
                    weight: rule.weight,
                    italic: rule.italic,
                    ttf,
                });
                break; // first usable src wins for this @font-face rule
            }
        }
    }
    combined.extend_from_slice(fonts);
    combined
}

/// Render `html` to a PDF with full page control: named/explicit size, per-side
/// margins, and a running header/footer (with `{{page}}` / `{{pages}}`
/// substitution) painted in the top/bottom margins of every page.
pub fn render_with(html: &str, fonts: &[ProvidedFont], opts: &RenderOptions) -> Vec<u8> {
    // Run inline <script>s first so script-driven DOM mutations are rendered.
    let body_html = crate::js::run_inline_scripts(html);
    let nodes = dom::parse(&body_html);
    // Decode inline `@font-face { src: url(data:…) }` web fonts and register them
    // alongside the caller's fonts, so a document-embedded face actually renders.
    let fonts = fonts_with_inline_faces(&nodes, fonts);
    let fonts = fonts.as_slice();
    let sheet = Stylesheet::with_viewport(&collect_style_css(&nodes), Some(opts.page_w / 0.75));
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
    let sheet = Stylesheet::with_viewport(&collect_style_css(&nodes), Some(page_w / 0.75));
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
            Fragment::Transformed { .. } => fragment_bbox(f).3,
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
            wrapped @ (Fragment::Clipped { .. } | Fragment::Transformed { .. }) => {
                // Move the clip/transform window and its content together.
                let mut moved = wrapped;
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

    // Embed every provided font once; remember its object id with the face's
    // exact (family, weight, italic) so distinct per-weight instances of one
    // family embed as distinct font objects and stay individually selectable.
    let mut objs: Vec<(String, u16, bool, u32)> = Vec::new();
    for f in fonts {
        if let Ok(id) = doc.embed_truetype_font(&f.family, &f.ttf) {
            objs.push((f.family.to_ascii_lowercase(), f.weight, f.italic, id));
        }
    }
    // Resolve a run to a *provided* font object id (no fallback), using the same
    // CSS nearest-weight matching as the measurer so painting picks the very face
    // the layout measured: same family + same italic, closest weight per
    // `weight_pref`; then same family any italic; then any provided face.
    let resolve_provided = |objs: &[(String, u16, bool, u32)], style: &Style| -> Option<u32> {
        let fam = style.font_family.to_ascii_lowercase();
        let want = style.font_weight;
        objs.iter()
            .filter(|(f, _, it, _)| *f == fam && *it == style.italic)
            .min_by_key(|(_, w, _, _)| weight_pref(*w, want))
            .or_else(|| {
                objs.iter()
                    .filter(|(f, _, _, _)| *f == fam)
                    .min_by_key(|(_, w, _, _)| weight_pref(*w, want))
            })
            .or_else(|| objs.first())
            .map(|(_, _, _, id)| *id)
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
            // Unwrap nested `Clipped` / `Transformed` layers: emit one graphics-
            // state op per layer — a `q … re W n` clip or a `q … cm` transform
            // (both flipped from top-down CSS to PDF user space) — paint the inner
            // fragment, then balance with one `Q` per layer. Layers nest in source
            // order. A bare fragment leaves `depth = 0` ⇒ byte-identical output.
            let mut depth = 0usize;
            let mut real = frag;
            loop {
                match real {
                    Fragment::Clipped {
                        rect,
                        radius,
                        radius_v,
                        inner,
                    } => {
                        let rounded =
                            radius.iter().any(|r| *r > 0.0) || radius_v.iter().any(|r| *r > 0.0);
                        if rounded {
                            // Clip to the rounded contour (same path the box fill
                            // uses; SVG-(0,0)→(0, page_h) with Y flipped).
                            let d = rounded_rect_path(
                                rect[0], rect[1], rect[2], rect[3], *radius, *radius_v,
                            );
                            let _ = doc.push_clip_svg_path(page, &d, 0.0, page_h);
                        } else {
                            let _ = doc.push_clip_rect(
                                page,
                                rect[0],
                                page_h - rect[1] - rect[3],
                                rect[2],
                                rect[3],
                            );
                        }
                        depth += 1;
                        real = inner;
                    }
                    Fragment::Transformed { matrix, inner } => {
                        // CSS (top-down) affine → PDF (bottom-up) `cm`: conjugate by
                        // the page Y-flip `F = [1,0,0,-1,0,H]` (`F·M·F`). Nesting
                        // composes because the inner `F·F` cancels.
                        let [a, b, c, d, e, f] = *matrix;
                        let cm = [a, -b, -c, d, c * page_h + e, page_h * (1.0 - d) - f];
                        let _ = doc.push_transform(page, cm);
                        depth += 1;
                        real = inner;
                    }
                    _ => break,
                }
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

                    // Outset drop shadow first (painted behind the box): the box
                    // grown by `spread`, offset by `(dx, dy)`, in the shadow colour
                    // with a true Gaussian blurred edge. Tracks the box's (possibly
                    // elliptical) corners. Inset shadows are drawn AFTER the fill,
                    // below, so the box background doesn't cover them.
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
                    // Inset shadow last (over the background, under any content). A
                    // sharp (zero-blur) inset stays a crisp vector frame clipped to
                    // the box (cheap, recessed look); a blurred inset goes through
                    // the true Gaussian raster feather, confined to the box interior.
                    if let Some(sh) = shadow {
                        if sh.inset {
                            if sh.blur > 0.0 {
                                paint_box_shadow(
                                    doc, page, page_h, *x, *y, *w, *h, *radius, *radius_v, sh,
                                );
                            } else {
                                paint_inset_box_shadow(doc, page, page_h, *x, *y, *w, *h, sh);
                            }
                        }
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
                            for sh in style.text_shadows.iter().rev() {
                                let a = (style.opacity * sh.alpha).clamp(0.0, 1.0);
                                for (ox, oy, sc) in text_shadow_passes(sh.blur) {
                                    if sc <= 0.0 {
                                        continue;
                                    }
                                    let _ = doc.add_text_standard(
                                        page,
                                        *x + sh.dx + ox,
                                        baseline - sh.dy + oy,
                                        style.font_size,
                                        trimmed,
                                        base14,
                                        sh.color,
                                        a * sc,
                                        0.0,
                                    );
                                }
                            }
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

                    // text-shadow: offset silhouettes painted UNDER the run (in
                    // reverse so the first listed shadow ends up on top).
                    for sh in style.text_shadows.iter().rev() {
                        let a = (style.opacity * sh.alpha).clamp(0.0, 1.0);
                        for (ox, oy, sc) in text_shadow_passes(sh.blur) {
                            if sc <= 0.0 {
                                continue;
                            }
                            let _ = doc.add_text(
                                page,
                                *x + sh.dx + ox,
                                baseline - sh.dy + oy,
                                style.font_size,
                                trimmed,
                                id,
                                sh.color,
                                a * sc,
                                0.0,
                            );
                        }
                    }

                    // Colour-emoji fast path: when the resolved face has a colour
                    // table (COLR v1 gradients, COLR v0 layers, `sbix` or
                    // `CBDT`/`CBLC` bitmaps) and this run holds a colour glyph,
                    // draw those glyphs in colour and the rest as ordinary text.
                    let face = book.face(style);
                    let colrv1 = face.and_then(|f| f.colrv1_glyphs()); // COLR v1 paint graph
                    let colors = face.and_then(|f| f.color_glyphs()); // COLR v0 / CPAL
                    let sbix = face.and_then(|f| f.sbix_glyphs()); // Apple bitmap emoji
                    let cbdt = face.and_then(|f| f.cbdt_glyphs()); // Google bitmap emoji
                                                                   // How a colour glyph is drawn (highest fidelity first).
                    #[derive(Clone, Copy)]
                    enum ColorKind {
                        Colrv1,
                        Colrv0,
                        Sbix,
                        Cbdt,
                    }
                    let classify = |f: &crate::font::truetype::TrueTypeFont,
                                    ch: char|
                     -> Option<(u16, ColorKind)> {
                        let g = f.gid_for_unicode(ch as u32)?;
                        if colrv1
                            .as_ref()
                            .map(|c| c.layers(g).is_some())
                            .unwrap_or(false)
                        {
                            Some((g, ColorKind::Colrv1))
                        } else if colors
                            .as_ref()
                            .map(|c| c.layers(g).is_some())
                            .unwrap_or(false)
                        {
                            Some((g, ColorKind::Colrv0))
                        } else if sbix.as_ref().map(|s| s.glyph(g).is_some()).unwrap_or(false) {
                            Some((g, ColorKind::Sbix))
                        } else if cbdt
                            .as_ref()
                            .zip(face.and_then(|f| f.cblc_bytes()))
                            .map(|(c, cblc)| c.glyph(cblc, g).is_some())
                            .unwrap_or(false)
                        {
                            Some((g, ColorKind::Cbdt))
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
                                Some((g, kind)) => {
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
                                    match kind {
                                        ColorKind::Colrv1 => {
                                            if let Some(c) = colrv1.as_ref() {
                                                let _ = doc.draw_colrv1_glyph(
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
                                        }
                                        ColorKind::Colrv0 => {
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
                                        }
                                        ColorKind::Sbix => {
                                            let _ = doc.draw_sbix_glyph(
                                                page,
                                                face,
                                                g,
                                                pen,
                                                baseline,
                                                style.font_size,
                                            );
                                        }
                                        ColorKind::Cbdt => {
                                            let _ = doc.draw_cbdt_glyph(
                                                page,
                                                face,
                                                g,
                                                pen,
                                                baseline,
                                                style.font_size,
                                            );
                                        }
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
                // Unwrapped above into graphics-state layers + `real`; the wrapper
                // variants never reach the inner match.
                Fragment::Clipped { .. } | Fragment::Transformed { .. } => {}
            }
            for _ in 0..depth {
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

/// Paint a `box-shadow` behind (outset) or inside (inset) a box.
///
/// With **`blur == 0`** this is a single hard filled rect/rounded-path —
/// byte-for-byte the previous outset behaviour (kept as the fast path). With
/// **`blur > 0`** the shadow is a **true Gaussian feather**: the shadow shape
/// (the box's rounded-rect outline grown by `spread` and offset by `(dx, dy)`)
/// is rasterised into a single-channel coverage buffer at device resolution,
/// blurred with a real separable Gaussian (three successive box blurs, sigma =
/// `blur / 2` per the CSS spec), then placed behind the box as a PDF image whose
/// constant shadow colour is masked by the blurred alpha (an `/SMask`, wired by
/// [`Document::add_image`]). The raster is padded by ~3·sigma on every side so
/// the blur tails fit, and the shape is antialiased so even a zero-spread square
/// shadow feathers cleanly.
///
/// `inset` shadows render the **complement inside the box** (the inner band the
/// inset offset/spread leaves dark), blurred and clipped to the box interior, so
/// the soft edge stays within the element — no clip primitive needed because the
/// coverage buffer itself is confined to the box rectangle.
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
    if sh.blur <= 0.0 && !sh.inset {
        // Hard outset shadow — unchanged single box at the legacy alpha (1.0): the
        // box grown by `spread`, offset by `(dx, dy)`, corners tracking the box.
        paint_hard_outset_shadow(doc, page, page_h, x, y, w, h, radius, radius_v, sh);
        return;
    }

    // Soft (or inset) shadow → rasterise a Gaussian-feathered coverage buffer and
    // place it as a colour-image masked by that alpha. `shadow::box_shadow_png`
    // returns the device PNG plus the top-down placement rect in points.
    if let Some(img) = shadow::box_shadow_png(x, y, w, h, radius, radius_v, sh) {
        // `add_image` takes a PDF bottom-left origin; flip the top-down rect.
        let _ = doc.add_image(
            page,
            &img.png,
            img.x,
            page_h - img.y - img.h,
            img.w,
            img.h,
            1.0,
        );
    }
}

/// Emit the legacy hard outset shadow: the box grown by `spread` on every side,
/// offset by `(dx, dy)`, filled in the shadow colour at full alpha — a square
/// `re` fill or a rounded-rect path, byte-identical to the pre-Gaussian output.
#[allow(clippy::too_many_arguments)]
fn paint_hard_outset_shadow(
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
    if rh.iter().any(|v| *v > 0.0) || rv.iter().any(|v| *v > 0.0) {
        let d = rounded_rect_path(sx, sy, sw, shh, rh, rv);
        let _ = doc.add_path(page, &d, 0.0, page_h, None, Some(sh.color), 0.0, 1.0);
    } else {
        let _ = doc.add_rectangle(
            page,
            sx,
            page_h - sy - shh,
            sw,
            shh,
            None,
            Some(sh.color),
            0.0,
            1.0,
        );
    }
}

/// True Gaussian `box-shadow` feather, rasterised and embedded as a PDF
/// soft-masked image.
///
/// The shadow shape (a rounded rectangle) is sampled into a single-channel
/// coverage buffer at device resolution, blurred with a real separable Gaussian
/// (approximated by three successive box blurs — Wronski, *Fast Almost-Gaussian
/// Filtering* — accurate to <3% of an exact kernel), then exposed as an RGBA PNG
/// whose colour is constant (the shadow colour) and whose alpha is the blurred
/// coverage scaled by the shadow's own opacity. The PNG's alpha becomes a PDF
/// `/SMask` via the engine's existing [`Document::add_image`] path, so the result
/// is a genuinely blurred drop shadow — not a stack of vector rings.
///
/// Zero blur is handled by the caller's fast vector path; this module only runs
/// for blurred (or inset) shadows.
mod shadow {
    use super::super::css::BoxShadow;

    /// Device pixels per CSS point for the shadow raster. Shadows are diffuse, so
    /// 2 px/pt keeps the soft edge crisp at print scale while bounding buffer size
    /// (a 200×100 pt shadow → ~400×200 px before padding).
    const SCALE: f64 = 2.0;

    /// CSS maps the `box-shadow` blur radius to a Gaussian `stdDev ≈ blur / 2`.
    const SIGMA_PER_BLUR: f64 = 0.5;

    /// Pad the raster by this many sigmas on every side so the Gaussian tail (which
    /// is ~0 beyond 3σ) is fully captured rather than clipped at the buffer edge.
    const TAIL_SIGMAS: f64 = 3.0;

    /// Upper bound on a raster dimension (device px). A pathological blur/spread is
    /// clamped rather than allocating an unbounded buffer.
    const MAX_DIM: usize = 4096;

    /// A finished shadow raster plus its placement, in **top-down points** (the
    /// same space as a [`super::super::layout::Fragment::Rect`]). The caller flips
    /// `y` for the PDF bottom-left origin.
    pub(super) struct ShadowImage {
        pub png: Vec<u8>,
        pub x: f64,
        pub y: f64,
        pub w: f64,
        pub h: f64,
    }

    /// Build the Gaussian-feathered shadow image for box `(x, y, w, h)` (top-down
    /// points) with the given per-corner radii and shadow spec. Returns `None`
    /// when the shadow has no visible area (e.g. spread collapses it, or the box
    /// is degenerate) so the caller emits nothing.
    pub(super) fn box_shadow_png(
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        radius: [f64; 4],
        radius_v: [f64; 4],
        sh: &BoxShadow,
    ) -> Option<ShadowImage> {
        let sigma = (sh.blur * SIGMA_PER_BLUR).max(0.0);
        if sh.inset {
            inset_shadow_png(x, y, w, h, radius, radius_v, sh, sigma)
        } else {
            outset_shadow_png(x, y, w, h, radius, radius_v, sh, sigma)
        }
    }

    /// Outset shadow: rasterise the spread+offset rounded rect, blur it, and place
    /// the (padded) buffer behind the box.
    #[allow(clippy::too_many_arguments)]
    fn outset_shadow_png(
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        radius: [f64; 4],
        radius_v: [f64; 4],
        sh: &BoxShadow,
        sigma: f64,
    ) -> Option<ShadowImage> {
        // Shadow rect in points: grow by spread, offset by (dx, dy).
        let sx = x - sh.spread + sh.dx;
        let sy = y - sh.spread + sh.dy;
        let sw = w + 2.0 * sh.spread;
        let shh = h + 2.0 * sh.spread;
        if sw <= 0.0 || shh <= 0.0 {
            return None;
        }
        let rh = grow_radii(radius, sh.spread);
        let rv = grow_radii(radius_v, sh.spread);

        // Pad the buffer outward so the blur tail fits.
        let pad = (sigma * TAIL_SIGMAS).ceil().max(0.0);
        let ox = sx - pad; // top-left of the raster, in points
        let oy = sy - pad;
        let total_w = sw + 2.0 * pad;
        let total_h = shh + 2.0 * pad;
        let (dw, dh) = device_dims(total_w, total_h)?;

        // Coverage = the rounded rect, with its top-left at (pad, pad) inside the
        // raster (device px). Sample with antialiasing so the edge feathers.
        let mut cov = vec![0.0_f32; dw * dh];
        rasterize_rounded_rect(
            &mut cov,
            dw,
            dh,
            pad * SCALE,
            pad * SCALE,
            sw * SCALE,
            shh * SCALE,
            rh,
            rv,
        );
        gaussian_blur(&mut cov, dw, dh, sigma * SCALE);

        Some(ShadowImage {
            png: encode_shadow(&cov, dw, dh, sh.color),
            x: ox,
            y: oy,
            w: total_w,
            h: total_h,
        })
    }

    /// Inset shadow: the dark band is the area inside the box NOT covered by the
    /// rounded rect shrunk by `spread` and offset by `(dx, dy)`. Rasterise that
    /// complement, blur it, then keep only the part inside the box (a hard
    /// clip-to-interior multiply) so the soft edge never leaks outside.
    #[allow(clippy::too_many_arguments)]
    fn inset_shadow_png(
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        radius: [f64; 4],
        radius_v: [f64; 4],
        sh: &BoxShadow,
        sigma: f64,
    ) -> Option<ShadowImage> {
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        // The raster covers exactly the box; the inner light hole is the box shrunk
        // by spread and shifted by (dx, dy).
        let (dw, dh) = device_dims(w, h)?;
        let inner_x = (sh.spread + sh.dx) * SCALE;
        let inner_y = (sh.spread + sh.dy) * SCALE;
        let inner_w = (w - 2.0 * sh.spread) * SCALE;
        let inner_h = (h - 2.0 * sh.spread) * SCALE;
        let rh = grow_radii(radius, -sh.spread);
        let rv = grow_radii(radius_v, -sh.spread);

        // Inner-hole coverage (the lit region), blurred so its edge is soft.
        let mut hole = vec![0.0_f32; dw * dh];
        if inner_w > 0.0 && inner_h > 0.0 {
            rasterize_rounded_rect(
                &mut hole, dw, dh, inner_x, inner_y, inner_w, inner_h, rh, rv,
            );
        }
        gaussian_blur(&mut hole, dw, dh, sigma * SCALE);

        // Box mask (sharp box interior, AA edge) — the clip that confines the inset.
        let mut clip = vec![0.0_f32; dw * dh];
        rasterize_rounded_rect(
            &mut clip,
            dw,
            dh,
            0.0,
            0.0,
            w * SCALE,
            h * SCALE,
            radius,
            radius_v,
        );

        // Inset coverage = inside-the-box AND outside-the-(blurred)-hole.
        let mut cov = vec![0.0_f32; dw * dh];
        for i in 0..cov.len() {
            cov[i] = clip[i] * (1.0 - hole[i]).clamp(0.0, 1.0);
        }

        Some(ShadowImage {
            png: encode_shadow(&cov, dw, dh, sh.color),
            x,
            y,
            w,
            h,
        })
    }

    /// Grow each non-zero corner radius by `by` (clamped at 0). A zero radius stays
    /// a sharp corner; `by` may be negative (inset shrink).
    fn grow_radii(arr: [f64; 4], by: f64) -> [f64; 4] {
        let g = |r: f64| if r > 0.0 { (r + by).max(0.0) } else { 0.0 };
        [g(arr[0]), g(arr[1]), g(arr[2]), g(arr[3])]
    }

    /// Device buffer dimensions for a points rect, clamped to [`MAX_DIM`]. `None`
    /// if either axis rounds to zero.
    fn device_dims(w_pt: f64, h_pt: f64) -> Option<(usize, usize)> {
        let dw = ((w_pt * SCALE).round() as usize).clamp(0, MAX_DIM);
        let dh = ((h_pt * SCALE).round() as usize).clamp(0, MAX_DIM);
        if dw == 0 || dh == 0 {
            None
        } else {
            Some((dw, dh))
        }
    }

    /// Antialiased coverage of a rounded rectangle into `cov` (row-major, `w×h`).
    /// The rect spans `[rx, rx+rw) × [ry, ry+rh)` in device px with per-corner
    /// horizontal/vertical radii `rh`/`rv` (already in device px-equivalent points,
    /// scaled by the caller). Coverage is `1` deep inside, ramps across a 1-px edge
    /// band, and `0` outside — so the blur input is smooth, not stair-stepped.
    #[allow(clippy::too_many_arguments)]
    fn rasterize_rounded_rect(
        cov: &mut [f32],
        w: usize,
        h: usize,
        rx: f64,
        ry: f64,
        rw: f64,
        rh: f64,
        radius: [f64; 4],
        radius_v: [f64; 4],
    ) {
        if rw <= 0.0 || rh <= 0.0 {
            return;
        }
        // Per-corner radii in device px, clamped so opposite corners never overlap.
        let sx = SCALE;
        let max_rx = rw / 2.0;
        let max_ry = rh / 2.0;
        let clamp_pair = |hr: f64, vr: f64| -> (f64, f64) {
            (
                (hr * sx).min(max_rx).max(0.0),
                (vr * sx).min(max_ry).max(0.0),
            )
        };
        let (tlh, tlv) = clamp_pair(radius[0], radius_v[0]);
        let (trh, trv) = clamp_pair(radius[1], radius_v[1]);
        let (brh, brv) = clamp_pair(radius[2], radius_v[2]);
        let (blh, blv) = clamp_pair(radius[3], radius_v[3]);

        let x0 = rx;
        let y0 = ry;
        let x1 = rx + rw;
        let y1 = ry + rh;
        // Only scan the rows/cols the rect can touch.
        let cx0 = x0.floor().max(0.0) as usize;
        let cy0 = y0.floor().max(0.0) as usize;
        let cx1 = (x1.ceil() as usize).min(w);
        let cy1 = (y1.ceil() as usize).min(h);

        for py in cy0..cy1 {
            let yc = py as f64 + 0.5; // pixel centre
            for px in cx0..cx1 {
                let xc = px as f64 + 0.5;
                // Signed distance to the rounded-rect boundary (negative = inside),
                // built from the nearest corner ellipse or the straight edges.
                let d = rounded_rect_signed_distance(
                    xc,
                    yc,
                    x0,
                    y0,
                    x1,
                    y1,
                    (tlh, tlv),
                    (trh, trv),
                    (brh, brv),
                    (blh, blv),
                );
                // 1-px wide antialiased edge: coverage 1 at d≤-0.5, 0 at d≥0.5.
                let c = (0.5 - d).clamp(0.0, 1.0) as f32;
                if c > 0.0 {
                    cov[py * w + px] = c;
                }
            }
        }
    }

    /// Signed distance (device px) from point `(x, y)` to the boundary of the
    /// rounded rect `[x0,x1]×[y0,y1]`, negative inside. In a corner quadrant the
    /// distance is to that corner's ellipse; elsewhere it is the usual axis-aligned
    /// box distance. Each corner pair is `(horizontal_radius, vertical_radius)` in
    /// the order top-left, top-right, bottom-right, bottom-left.
    #[allow(clippy::too_many_arguments)]
    fn rounded_rect_signed_distance(
        x: f64,
        y: f64,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        tl: (f64, f64),
        tr: (f64, f64),
        br: (f64, f64),
        bl: (f64, f64),
    ) -> f64 {
        // Pick the corner whose rounded region this point falls in (if any).
        let corner = if x < x0 + tl.0 && y < y0 + tl.1 {
            Some((x0 + tl.0, y0 + tl.1, tl.0, tl.1)) // centre + radii
        } else if x > x1 - tr.0 && y < y0 + tr.1 {
            Some((x1 - tr.0, y0 + tr.1, tr.0, tr.1))
        } else if x > x1 - br.0 && y > y1 - br.1 {
            Some((x1 - br.0, y1 - br.1, br.0, br.1))
        } else if x < x0 + bl.0 && y > y1 - bl.1 {
            Some((x0 + bl.0, y1 - bl.1, bl.0, bl.1))
        } else {
            None
        };

        if let Some((cx, cy, rxr, ryr)) = corner {
            if rxr > 0.0 && ryr > 0.0 {
                // Distance to the ellipse, approximated in normalised space then
                // scaled back by the local radius (exact on circles, close on mild
                // ellipses — ample for a diffuse shadow edge).
                let nx = (x - cx) / rxr;
                let ny = (y - cy) / ryr;
                let nd = (nx * nx + ny * ny).sqrt();
                let scale = (rxr + ryr) * 0.5;
                return (nd - 1.0) * scale;
            }
        }
        // Axis-aligned box signed distance.
        let dx = (x0 - x).max(x - x1);
        let dy = (y0 - y).max(y - y1);
        if dx <= 0.0 && dy <= 0.0 {
            dx.max(dy) // inside: negative, nearest edge
        } else {
            let ox = dx.max(0.0);
            let oy = dy.max(0.0);
            (ox * ox + oy * oy).sqrt()
        }
    }

    /// In-place separable Gaussian blur of an alpha buffer (`w×h`, row-major) via
    /// three successive box blurs whose combined response approximates a true
    /// Gaussian of standard deviation `sigma` (device px). Below ~0.5 px the blur
    /// is a no-op (nothing visible to spread).
    pub(super) fn gaussian_blur(buf: &mut [f32], w: usize, h: usize, sigma: f64) {
        if sigma < 0.5 || w == 0 || h == 0 {
            return;
        }
        let radii = box_blur_radii(sigma);
        let mut tmp = vec![0.0_f32; buf.len()];
        for r in radii {
            if r == 0 {
                continue;
            }
            box_blur_horizontal(buf, &mut tmp, w, h, r);
            box_blur_vertical(&tmp, buf, w, h, r);
        }
    }

    /// Three box-blur radii whose convolution matches a Gaussian of std-dev
    /// `sigma`, per Wronski's *Fast Almost-Gaussian Filtering*. The "ideal" box
    /// width `wi = sqrt(12σ²/n + 1)` is split into `n` integer boxes (here `n = 3`)
    /// — some of width `wl`, the rest `wl+2` — so their averaged variance equals
    /// `σ²`. Returned as per-pass half-widths (a box of width `2r+1`).
    pub(super) fn box_blur_radii(sigma: f64) -> [usize; 3] {
        const N: f64 = 3.0;
        let wi = (12.0 * sigma * sigma / N + 1.0).sqrt();
        // Largest odd integer ≤ wi (box widths must be odd to stay centred).
        let mut wl = wi.floor() as i64;
        if wl % 2 == 0 {
            wl -= 1;
        }
        let wl = wl.max(1);
        let wu = wl + 2;
        // Count `m` of the smaller boxes that best preserves the target variance
        // (Wronski's closed form; depends only on the smaller width `wl`).
        let wlf = wl as f64;
        let m_ideal =
            (12.0 * sigma * sigma - N * wlf * wlf - 4.0 * N * wlf - 3.0 * N) / (-4.0 * wlf - 4.0);
        let m = m_ideal.round().clamp(0.0, N) as usize;
        let mut out = [0usize; 3];
        for (i, slot) in out.iter_mut().enumerate() {
            let width = if i < m { wl } else { wu };
            *slot = ((width - 1) / 2) as usize; // half-width
        }
        out
    }

    /// One horizontal box blur (window `2r+1`) from `src` into `dst`, edges clamped
    /// (the border pixel is repeated), via a sliding running sum — O(w·h).
    fn box_blur_horizontal(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
        let win = (2 * r + 1) as f32;
        for y in 0..h {
            let row = y * w;
            // Seed the window: sum over [-r, r] with clamped indices.
            let mut sum = 0.0_f32;
            for k in 0..=(2 * r) {
                let idx = k as isize - r as isize; // -r..=r relative to x=0
                let cx = idx.clamp(0, w as isize - 1) as usize;
                sum += src[row + cx];
            }
            for x in 0..w {
                dst[row + x] = sum / win;
                // Slide: drop the leftmost, add the next-right (both clamped).
                let drop_i = (x as isize - r as isize).clamp(0, w as isize - 1) as usize;
                let add_i = (x as isize + r as isize + 1).clamp(0, w as isize - 1) as usize;
                sum += src[row + add_i] - src[row + drop_i];
            }
        }
    }

    /// One vertical box blur (window `2r+1`) from `src` into `dst`, edges clamped,
    /// via a sliding running sum — O(w·h).
    fn box_blur_vertical(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
        let win = (2 * r + 1) as f32;
        for x in 0..w {
            let mut sum = 0.0_f32;
            for k in 0..=(2 * r) {
                let idx = k as isize - r as isize;
                let cy = idx.clamp(0, h as isize - 1) as usize;
                sum += src[cy * w + x];
            }
            for y in 0..h {
                dst[y * w + x] = sum / win;
                let drop_i = (y as isize - r as isize).clamp(0, h as isize - 1) as usize;
                let add_i = (y as isize + r as isize + 1).clamp(0, h as isize - 1) as usize;
                sum += src[add_i * w + x] - src[drop_i * w + x];
            }
        }
    }

    /// Encode an alpha-coverage buffer as an RGBA PNG: every pixel carries the
    /// constant shadow `colour`, with alpha = `round(coverage·255)`. The engine's
    /// image path lifts this alpha into a `/DeviceGray` `/SMask`, so the placed
    /// image paints the shadow colour faded by the Gaussian coverage.
    fn encode_shadow(cov: &[f32], w: usize, h: usize, color: [f64; 3]) -> Vec<u8> {
        let r = (color[0].clamp(0.0, 1.0) * 255.0).round() as u8;
        let g = (color[1].clamp(0.0, 1.0) * 255.0).round() as u8;
        let b = (color[2].clamp(0.0, 1.0) * 255.0).round() as u8;
        let mut rgba = Vec::with_capacity(w * h * 4);
        for &c in cov.iter().take(w * h) {
            let a = (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            rgba.extend_from_slice(&[r, g, b, a]);
        }
        crate::raster::png::encode_png(w as u32, h as u32, &rgba)
    }
}

/// Paint an `inset` `box-shadow`: a shadow-coloured frame *inside* the box so it
/// looks recessed. The unshadowed inner area is the box inset by `spread + blur`
/// on every side and shifted by the offset `(dx, dy)`; the frame between it and
/// the box edge is filled with the shadow colour, clipped to the box. Like the
/// outset path, blur is approximated (a single soft frame, not a Gaussian).
#[allow(clippy::too_many_arguments)]
fn paint_inset_box_shadow(
    doc: &mut Document,
    page: u32,
    page_h: f64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    sh: &super::css::BoxShadow,
) {
    let reach = (sh.spread + sh.blur).max(0.0);
    if reach <= 0.0 && sh.dx == 0.0 && sh.dy == 0.0 {
        return; // nothing to draw — no spread, blur or offset
    }
    // Inner unshadowed rect: box inset by `reach`, then shifted by the offset,
    // clamped inside the box.
    let hl = (x + reach + sh.dx).clamp(x, x + w);
    let ht = (y + reach + sh.dy).clamp(y, y + h);
    let hr = (x + w - reach + sh.dx).clamp(hl, x + w);
    let hb = (y + h - reach + sh.dy).clamp(ht, y + h);
    // Clip to the box so the frame never bleeds past its edges.
    let _ = doc.push_clip_rect(page, x, page_h - y - h, w, h);
    let mut band = |bx: f64, by: f64, bw: f64, bh: f64| {
        if bw > 0.01 && bh > 0.01 {
            let _ = doc.add_rectangle(
                page,
                bx,
                page_h - by - bh,
                bw,
                bh,
                None,
                Some(sh.color),
                0.0,
                1.0,
            );
        }
    };
    band(x, y, w, ht - y); // top
    band(x, hb, w, y + h - hb); // bottom
    band(x, ht, hl - x, hb - ht); // left
    band(hr, ht, x + w - hr, hb - ht); // right
    let _ = doc.restore_graphics(page);
}

/// Offset/opacity passes for one `text-shadow`, relative to its `(dx, dy)`. With
/// `blur == 0` it's a single hard silhouette; with `blur > 0` a centre pass plus
/// four offset passes at low alpha approximate a soft cloud (a cheap blur — not a
/// true Gaussian — matching the `box-shadow` blur approximation policy).
fn text_shadow_passes(blur: f64) -> [(f64, f64, f64); 5] {
    if blur <= 0.0 {
        return [
            (0.0, 0.0, 1.0),
            (0.0, 0.0, 0.0),
            (0.0, 0.0, 0.0),
            (0.0, 0.0, 0.0),
            (0.0, 0.0, 0.0),
        ];
    }
    let r = blur * 0.5;
    [
        (0.0, 0.0, 0.42),
        (r, 0.0, 0.145),
        (-r, 0.0, 0.145),
        (0.0, r, 0.145),
        (0.0, -r, 0.145),
    ]
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
        rasters: Vec::new(),
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
    // One sector per degree: the flat-fill steps are below visual acuity at any
    // print resolution (PDF has no native conic shading, so this fan is the
    // ceiling — finer steps only grow the content stream with no visible gain).
    const SECTORS: usize = 360;
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
        rasters: Vec::new(),
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
        rasters: Vec::new(),
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
        let provided_face = &book
            .provided_match(&style)
            .expect("the provided face exists")
            .0
            .ttf;
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
    fn inset_border_shades_sides_for_depth() {
        // An `inset` border draws the top/left sides darker and the bottom/right
        // sides lighter than the base colour to fake depth — so two distinct
        // shades of `#888` (0.533) appear: darken≈0.293 and lighten≈0.743.
        let content =
            page1_content(r#"<div style="border:10pt inset #888888;padding:8pt">x</div>"#);
        assert!(
            content.contains("0.29"),
            "inset top/left darkened\n{content}"
        );
        assert!(
            content.contains("0.74"),
            "inset bottom/right lightened\n{content}"
        );
        // A plain `solid` border keeps the one flat colour (0.533 → "0.53").
        let solid = page1_content(r#"<div style="border:10pt solid #888888;padding:8pt">x</div>"#);
        assert!(
            solid.contains("0.53"),
            "solid border keeps one tone\n{solid}"
        );
        assert!(
            !solid.contains("0.29"),
            "solid border is not bevelled\n{solid}"
        );
    }

    #[test]
    fn inset_box_shadow_paints_a_clipped_frame_inside_the_box() {
        // A SHARP (zero-blur) `inset` box-shadow draws a shadow-coloured frame
        // INSIDE the box, clipped to it — so the red shadow fill and a `W n` clip
        // both appear (the crisp vector path; blurred insets go through the
        // Gaussian raster feather instead — see `inset_box_shadow_renders_inside_the_box`).
        let content = page1_content(
            r#"<div style="background:#ffffff;box-shadow:inset 4pt 4pt 0 #ff0000;padding:10pt">x</div>"#,
        );
        assert!(
            content.contains("1 0 0 rg"),
            "inset shadow paints its colour\n{content}"
        );
        assert!(
            content.contains("W n"),
            "inset shadow is clipped to the box\n{content}"
        );
        // No shadow ⇒ the red fill never appears (control).
        let none = page1_content(r#"<div style="background:#ffffff;padding:10pt">x</div>"#);
        assert!(
            !none.contains("1 0 0 rg"),
            "no shadow ⇒ no shadow colour\n{none}"
        );
    }

    #[test]
    fn text_shadow_paints_an_offset_coloured_copy() {
        // The shadow re-draws the glyphs in the shadow colour, under the run.
        let content =
            page1_content(r#"<p style="color:#000000;text-shadow:2pt 2pt #ff0000">Hi</p>"#);
        assert!(
            content.contains("1 0 0 rg"),
            "text-shadow paints its colour\n{content}"
        );
        // It inherits, so a shadow on a parent reaches the child's text.
        let inherited =
            page1_content(r#"<div style="text-shadow:1pt 1pt #ff0000"><p>Hi</p></div>"#);
        assert!(
            inherited.contains("1 0 0 rg"),
            "text-shadow inherits to descendants\n{inherited}"
        );
        // No shadow ⇒ no red.
        let none = page1_content(r#"<p style="color:#000000">Hi</p>"#);
        assert!(!none.contains("1 0 0 rg"), "no shadow ⇒ no red\n{none}");
    }

    #[test]
    fn transform_emits_a_y_flipped_cm_matrix() {
        // `translate(20pt, 10pt)` commutes with the origin shift, so the CSS matrix
        // is [1,0,0,1,20,10]; the page Y-flip turns it into `1 0 0 1 20 -10 cm`.
        let content = page1_content(
            r#"<div style="transform:translate(20pt,10pt);width:50pt;height:30pt;background:#ff0000">x</div>"#,
        );
        assert!(
            content.contains("1 0 0 1 20 -10 cm"),
            "translate → Y-flipped cm\n{content}"
        );
        // A rotate emits a non-identity 2×2 (here `0 -1 1 0` for 90°).
        let rot = page1_content(
            r#"<div style="transform:rotate(90deg);width:40pt;height:40pt;background:#ff0000">x</div>"#,
        );
        assert!(rot.contains(" cm"), "rotate emits a cm\n{rot}");
        assert!(rot.contains("0 -1 1 0"), "rotate(90°) 2×2 part\n{rot}");
        // No transform ⇒ no cm op at all (the page has no images here).
        let none =
            page1_content(r#"<div style="width:50pt;height:30pt;background:#ff0000">x</div>"#);
        assert!(!none.contains(" cm"), "no transform ⇒ no cm\n{none}");
    }

    #[test]
    fn rounded_overflow_hidden_clips_to_the_curve() {
        // A rounded box with overflow:hidden + a full-bleed child clips to the
        // curve: the clip path carries bezier `c` ops, not a plain `re W n`.
        let rounded = page1_content(
            r#"<div style="width:60pt;height:40pt;border-radius:12pt;overflow:hidden"><div style="width:200pt;height:200pt;background:#ff0000"></div></div>"#,
        );
        assert!(rounded.contains("W n"), "a clip is emitted\n{rounded}");
        assert!(
            rounded.contains(" c\n"),
            "the rounded clip uses bezier curves\n{rounded}"
        );
        // A NON-rounded overflow:hidden box keeps the rectangular `re W n` clip.
        let square = page1_content(
            r#"<div style="width:60pt;height:40pt;overflow:hidden"><div style="width:200pt;height:200pt;background:#ff0000"></div></div>"#,
        );
        assert!(
            square.contains("re\nW n"),
            "square clip stays a rectangle\n{square}"
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
        // A crisp (zero-blur) shadow stays a vector fill and must be painted BEFORE
        // the box's own background (grey), so it sits behind it. We look for the red
        // shadow fill appearing earlier in the stream than the grey box fill. (The
        // blurred path is a raster image, covered separately — see
        // `blurred_box_shadow_emits_a_softmasked_image`.)
        let content = page1_content(
            r#"<div style="background:#cccccc;box-shadow:4pt 4pt 0pt #ff0000;padding:10pt">x</div>"#,
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

    // ── true Gaussian box-shadow (raster soft-mask) ──────────────────────────

    #[test]
    fn blurred_box_shadow_emits_a_softmasked_image() {
        // A blurred shadow is now a real raster feather: an /Image XObject masked
        // by a /DeviceGray /SMask (the blurred alpha), drawn (`Do`) behind the box.
        let pdf = render(
            r#"<div style="background:#ffffff;box-shadow:4pt 4pt 12pt #000000;padding:10pt">x</div>"#,
            &[],
            200.0,
            120.0,
            12.0,
        );
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        let raw = String::from_utf8_lossy(&pdf);
        assert!(
            raw.contains("/Subtype /Image"),
            "the blurred shadow embeds an image XObject"
        );
        assert!(
            raw.contains("/SMask"),
            "the shadow image carries a soft mask (the Gaussian alpha)"
        );
        let doc = Document::open(&pdf).expect("re-open");
        let content =
            String::from_utf8_lossy(&doc.page_content(1).expect("content")).to_string();
        assert!(
            content.contains(" Do"),
            "the shadow image is painted via a `Do` op\n{content}"
        );
    }

    #[test]
    fn zero_blur_box_shadow_stays_a_crisp_vector_fill() {
        // A zero-blur shadow keeps the fast vector path: a plain filled rect, NO
        // raster image (byte-cheap, sharp edge).
        let pdf = render(
            r#"<div style="background:#ffffff;box-shadow:4pt 4pt 0pt #ff0000;padding:10pt">x</div>"#,
            &[],
            200.0,
            120.0,
            12.0,
        );
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        let raw = String::from_utf8_lossy(&pdf);
        assert!(
            !raw.contains("/Subtype /Image"),
            "a sharp (zero-blur) shadow does NOT rasterise to an image"
        );
        let doc = Document::open(&pdf).expect("re-open");
        let content =
            String::from_utf8_lossy(&doc.page_content(1).expect("content")).to_string();
        assert!(
            content.contains("1 0 0 rg") && content.contains(" re"),
            "the crisp shadow is a red filled rectangle\n{content}"
        );
    }

    #[test]
    fn inset_box_shadow_renders_inside_the_box() {
        // An inset shadow now paints (it was previously skipped): a feathered band
        // confined to the box interior — embedded as a soft-masked image whose
        // placement box equals the element box.
        let pdf = render(
            r#"<div style="background:#ffffff;box-shadow:inset 0pt 0pt 8pt #000000;width:120pt;height:60pt"></div>"#,
            &[],
            220.0,
            140.0,
            12.0,
        );
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF");
        let raw = String::from_utf8_lossy(&pdf);
        assert!(
            raw.contains("/Subtype /Image") && raw.contains("/SMask"),
            "the inset shadow embeds a soft-masked image"
        );
    }

    #[test]
    fn gaussian_blur_falls_off_smoothly_from_centre_to_edge() {
        // Blur a single fully-covered pixel and check the response is a smooth,
        // monotonically-decreasing bump centred on it (a Gaussian-like kernel),
        // not a flat box or a hard ring.
        let w = 81usize;
        let h = 81usize;
        let mut buf = vec![0.0_f32; w * h];
        buf[40 * w + 40] = 1.0; // central impulse
        shadow::gaussian_blur(&mut buf, w, h, 6.0);

        let centre = buf[40 * w + 40];
        // Sample outward along the +x axis from the centre.
        let samples: Vec<f32> = (0..=30).map(|d| buf[40 * w + (40 + d)]).collect();
        assert!(centre > 0.0, "impulse spreads to a positive peak ({centre})");
        // Peak is at the centre and the profile never increases moving outward.
        for win in samples.windows(2) {
            assert!(
                win[1] <= win[0] + 1e-7,
                "blur response is monotonically non-increasing outward ({win:?})"
            );
        }
        // Far tail has effectively decayed (3σ ≈ 18 px out → near zero).
        assert!(
            samples[28] < centre * 0.05,
            "the Gaussian tail decays toward zero (tail={}, centre={centre})",
            samples[28]
        );
        // Half-maximum radius of a Gaussian is σ·sqrt(2·ln2) ≈ 1.1774·σ ≈ 7.06 px.
        let half = centre * 0.5;
        let hm_radius = samples
            .iter()
            .position(|&v| v <= half)
            .expect("profile crosses half-max within the sampled range");
        assert!(
            (5..=10).contains(&hm_radius),
            "half-max radius ≈ 1.18σ (~7 px for σ=6), got {hm_radius}"
        );
    }

    #[test]
    fn box_blur_radii_match_target_gaussian_variance() {
        // The three box-blur passes must reproduce the variance of the target
        // Gaussian: the summed variance of boxes of width (2r+1) is
        // Σ ((2r+1)² − 1)/12, which should be within a box-quantisation step of σ².
        for &sigma in &[2.0_f64, 4.0, 6.0, 10.0] {
            let radii = shadow::box_blur_radii(sigma);
            let var: f64 = radii
                .iter()
                .map(|&r| {
                    let width = (2 * r + 1) as f64;
                    (width * width - 1.0) / 12.0
                })
                .sum();
            let target = sigma * sigma;
            assert!(
                (var - target).abs() <= target * 0.25 + 1.0,
                "3 box blurs approximate σ²={target} (got variance {var}) for σ={sigma}"
            );
        }
        // σ below the visible floor yields all-zero radii (no-op blur).
        assert_eq!(shadow::box_blur_radii(0.0), [0, 0, 0]);
    }

    #[test]
    fn synthetic_bold_factor_grades_by_weight() {
        // Now graded by the GAP between the requested weight and the weight of the
        // face that actually matched: `1.0 + 0.03·(gap/300)`, capped at +0.06.

        // Matched face is same-or-heavier than requested → no widening, whatever
        // the requested weight (a real face carries its own width).
        assert_eq!(synthetic_bold_factor(100, 400), 1.0);
        assert_eq!(synthetic_bold_factor(400, 400), 1.0);
        assert_eq!(synthetic_bold_factor(500, 700), 1.0);
        assert_eq!(synthetic_bold_factor(900, 900), 1.0, "real bold face → 1.0");
        // Canonical `font-weight: bold` (700) served by a regular-only family
        // (matched 400, gap 300) is exactly 1.03 — unchanged from the legacy factor.
        assert!((synthetic_bold_factor(700, 400) - 1.03).abs() < 1e-9);
        // Heavier requests onto the same regular face widen further, monotonically.
        let w800 = synthetic_bold_factor(800, 400); // gap 400
        let w900 = synthetic_bold_factor(900, 400); // gap 500
        assert!(
            w800 > 1.03 && w900 > w800,
            "800<900 widening: {w800} {w900}"
        );
        // Gap is capped at +0.06 (reached by gap ≥ 600).
        assert!(
            (synthetic_bold_factor(900, 300) - 1.06).abs() < 1e-9,
            "cap +0.06"
        );
        assert!(
            (synthetic_bold_factor(900, 100) - 1.06).abs() < 1e-9,
            "gap 800 stays clamped at +0.06"
        );
    }

    #[test]
    fn weight_pref_implements_css_font_matching() {
        // Pick the best of a candidate set per `weight_pref` via the same
        // `min_by_key` the resolver uses.
        let best = |want: u16, faces: &[u16]| -> u16 {
            *faces.iter().min_by_key(|&&f| weight_pref(f, want)).unwrap()
        };
        let set = [100, 300, 400, 500, 700, 900];
        // 400 → exact 400.
        assert_eq!(best(400, &set), 400);
        // 500 → exact 500.
        assert_eq!(best(500, &set), 500);
        // 450 ∈ [400,500]: ascend within [450,500] first → 500, not 400.
        assert_eq!(best(450, &[100, 300, 400, 500, 700]), 500);
        // 400 with only {300,700}: in [400,500] none ≥400≤500, so lighter (<400)
        // wins over heavier (>500) → 300, NOT 700 (the classic 400→{300,700} case).
        assert_eq!(best(400, &[300, 700]), 300);
        // 500 with only {300,700}: same band rule → 300 wins over 700.
        assert_eq!(best(500, &[300, 700]), 300);
        // <400 (300): at-or-below want descends → 300 itself; else nearest above.
        assert_eq!(best(300, &set), 300);
        assert_eq!(best(200, &[100, 400, 700]), 100, "≤want (100) beats >want");
        // >500 (700): at-or-above ascends → 700; (600 not present) nearest above.
        assert_eq!(best(700, &set), 700);
        assert_eq!(best(600, &[400, 700, 900]), 700, "≥want nearest (700) wins");
        assert_eq!(
            best(800, &[300, 400, 700]),
            700,
            ">500 falls to heaviest below"
        );
    }

    /// Resolve a run at `weight` (family `Multi`) against `book` and return the
    /// exact weight of the provided face that matched — the probe used to prove
    /// per-weight faces no longer collide on a binary bold bit.
    fn matched_weight_for(book: &MeasureBook, weight: u16) -> u16 {
        let style = Style {
            font_family: "Multi".into(),
            font_size: 12.0,
            font_weight: weight,
            bold: weight >= 600,
            ..Style::default()
        };
        book.provided_match(&style)
            .expect("a provided face matches")
            .1
    }

    #[test]
    fn nearest_weight_picks_the_right_provided_face() {
        let face = |w: u16| ProvidedFont {
            family: "Multi".into(),
            weight: w,
            italic: false,
            ttf: bundled::FALLBACK_TTF.to_vec(),
        };
        // Same family provided at 300, 400 and 700 — distinct weights that used to
        // collide under the binary bold key (400 and 500 → same bucket).
        let book = MeasureBook::new(&[face(300), face(400), face(700)]);

        // Each request lands on the CSS-correct face.
        assert_eq!(matched_weight_for(&book, 300), 300, "300 → 300");
        assert_eq!(matched_weight_for(&book, 400), 400, "400 → 400");
        // 500 ∈ [400,500]: ascend within [500,500]…none, so the in-[want,500]
        // band is empty → fall to lighter (<500) nearest = 400, NOT the 700 face.
        assert_eq!(matched_weight_for(&book, 500), 400, "500 → 400 (not 700)");
        assert_eq!(matched_weight_for(&book, 700), 700, "700 → 700");
        // A heavier-than-any request snaps to the heaviest available face.
        assert_eq!(matched_weight_for(&book, 900), 700, "900 → 700 (heaviest)");
        // A lighter-than-any request snaps to the lightest available face.
        assert_eq!(matched_weight_for(&book, 100), 300, "100 → 300 (lightest)");
    }

    #[test]
    fn distinct_weights_embed_distinct_font_object_ids() {
        // Two same-family faces at 400 and 700 must embed as two distinct font
        // objects, and a run at each weight must resolve to its own object id —
        // i.e. the per-weight faces are individually selectable, not collapsed.
        let face = |w: u16| ProvidedFont {
            family: "Multi".into(),
            weight: w,
            italic: false,
            ttf: bundled::FALLBACK_TTF.to_vec(),
        };
        let fonts = [face(400), face(700)];

        let mut b = PdfBuilder::new();
        b.add_page(612.0, 792.0);
        let mut doc = Document::open(&b.finish()).expect("open");

        // Mirror the painter's embed + resolve so the test exercises the real path.
        let mut objs: Vec<(String, u16, bool, u32)> = Vec::new();
        for f in &fonts {
            let id = doc
                .embed_truetype_font(&f.family, &f.ttf)
                .expect("embed face");
            objs.push((f.family.to_ascii_lowercase(), f.weight, f.italic, id));
        }
        let resolve = |objs: &[(String, u16, bool, u32)], style: &Style| -> Option<u32> {
            let fam = style.font_family.to_ascii_lowercase();
            let want = style.font_weight;
            objs.iter()
                .filter(|(f, _, it, _)| *f == fam && *it == style.italic)
                .min_by_key(|(_, w, _, _)| weight_pref(*w, want))
                .or_else(|| {
                    objs.iter()
                        .filter(|(f, _, _, _)| *f == fam)
                        .min_by_key(|(_, w, _, _)| weight_pref(*w, want))
                })
                .or_else(|| objs.first())
                .map(|(_, _, _, id)| *id)
        };

        assert_eq!(objs.len(), 2, "two faces → two embedded font objects");
        assert_ne!(objs[0].3, objs[1].3, "distinct object ids per weight");

        let st = |w: u16| Style {
            font_family: "Multi".into(),
            font_size: 12.0,
            font_weight: w,
            bold: w >= 600,
            ..Style::default()
        };
        let id_400 = resolve(&objs, &st(400)).expect("400 resolves");
        let id_700 = resolve(&objs, &st(700)).expect("700 resolves");
        assert_ne!(
            id_400, id_700,
            "font-weight:400 and :700 select different embedded objects"
        );
        // 500 falls to the 400 face (the regular), not the 700 one.
        assert_eq!(resolve(&objs, &st(500)), Some(id_400), "500 → 400 object");
    }

    // ─── @font-face with inline `src` (#1) ─────────────────────────────────────

    /// A real fontTools-produced WOFF2 of a 2-glyph subset (.notdef + 'A') of
    /// JetBrains Mono (monospace, units-per-em 1000) — base64 of the same bytes
    /// `crate::font::webfont`'s tests reconstruct. Used here as the payload of a
    /// `data:font/woff2;base64,…` `@font-face` src, and (after reconstruction) of
    /// a `data:font/ttf;base64,…` one.
    const TINY_WOFF2_B64: &str = "d09GMgABAAAAAAjMAAsAAAAAEjwAAAh9AAJN0wAAAAAAAAAAAAAAAAAAAAAAAAAABmAANAiBKAmcDAqBVIFRATYCJAMGCwYABCAMgVYbbhFRlGtSDfDFgXk+01nT2aisg0VzsH4cP/KhLKVUlN3N/3+d5X3vg8DAGiB5ADVIsoyL5JndDSCXhEWVkyqVHEKnSnrmoslWOSuD5na3gadAaNbGR1S1V4f/R1uvxqICkpNc5QVIEyDDhN5SwusfkoKSd7xF/Yt6uupozKaKzaa0aslyKL5l4iitJP+gHZaR3CvE9/9d67XvvuQTumoC4QFtndtmlpKdwux+BFIFPp5QArEjMrr1ta7Wo43hepeheBaw+qeA5CY9DwXwk9wEcGBbgC7BAgV1DFjY1iXcdH8FQBtllMX/F78EbGnk4WbxKH+f5EJrXiB03hagPknChQTllKPIHNOoDcJB77Z0VKV5Ex4FooPSn9EFaS7aNdh/4vf9fv/JM32qbMWh9xwp6EObEQbRPbTftvDpEXH11bP3A0/gVN4U6YfnSN78gL77K7O0yvhLPWo+9QxHe2GKKVce7EGIEeyAqq0XkFSrJwi49ZhgWLSYEND0SMxdOeMrclSt3nlu5X2klzB/OxVxh7lqE/L6dOGeHlan+HMrwlwtyfmze7cE03zJ6eyquoGj+WFZHO1JlGQTkRSYm1IcrkCvGjSJ9jWy/LWM6zg3hw36IEdElJO/c4DGYW4aR6/JOpiDrLlq1l6KuilsE+O1h+LC8JyZQ1O7gY6an2xOsnWlGtff2C5eoZWMWgr8NnAQ5wEcz+BAryPNH2rub9p/1p6n5YC6x3MlV5QytQJlKaWg8g5xaF4SCtoIGW0SBvcZwx3TPcRYoJrQLHc1lFXd6LgAqsh2KIKyZaDqWIjajYCPRs3uf6jZ5/oDWe7nug/tpGQrsYjgLkMZIkF2rpYwgMV3F7CoyXxrlrsJDHmDO0eoC1WM3YQFaSOSPDyXcoViKSKoBRaq6xzfyMZiTUh14+9E5aHYYGWy4zHdm9JAFRlVm3moaWJ4ED1CqcddJWBaBiJQ9jqZmSSicBgOCQzbUFIV6qs8SoyJONjnGCyWd2Iihc49B1zV1ok0oCqR4vfgaBYp90KaguNIFyYycFmMeY16LuXKeaiRYdV8mZFxt0QWP/yWu/xel9dI/NhpIkctuZ9/+HV0QhxFbsE8OrKr3/1Z2+2h5mSygCoc3lMN0o8JLZgQsc+dpFqR4fOTlnTb9G40qLntgCHecLaWhoUUd6TNXiossJGI8Y4dyNkaIvAXUp3fWgFnOvjgt0zWXRuS7iQcVV31WefSWcmcuXTG3blA+YeGWs8PJU/l6qsl4aCj8G6iw85EnczUxYK6WVIPK+plTX1sUD+bNMBW3hRMBpdF1x9+y6YB+rhidk94gAhH/i47HTEq/86YizOksqwlUHsyKjOojCjCxSAMQzAMwzAMwygMYzCMwzABwySseAYalZZmG1c6q1ZGCqzEWkSVGb2tmAOzeTo7u4l5SDtJtMjabbu5jxxlAgdowT0U9eTAgdZdifndrnR+kz8oJbUXWTRyXPES9Gnuc5dxxkxzkO6+ocyP8iuC8xiHLrUv7XZf0vnynpEVaNSuDBmg2n9HiVUM18bEGrwQ4iHIN11HZ3Fe16AdGHI4vpufPPKdNeUHIRbIfVlTnb/8TmIDwKHDHRnIgN68x6VCjO8+nGdcGue13wo2Qxr6OqHdQ9xUXHadRXCTH0m7aMF2h3JxpbibsKl5KNQp2raiecnWVEH5ZLqkZJ0MxTbtM8FpoTH1bjdkuEdqJiGLbdvtYNjcwmG1UtpmVWPMaPM1VSCHVtFQMRciS2tNZEQLm+duOiC9spLY4sdSklAlw8bb3hHbIWyMf2tUwv7mH49vFcjN7RuCk3nQaDHAHvlCiSMTHZRJ372/uAU5rMZ7JGZc7SeWcpNKieyQFbcei+0Vc1UA08DJn7ylg3kfTkrWWIY+p3PvJsqI9U/CfvpK+BGHOuIq2MhEG9bAZs4uVenZds5r8EZoDsyFonoIjo7ahfs1jbh78ZWStjaNGux2Gj/wBrrdFw4ctf4ldpqsyqXuZQeOdjtEnUmNJa+0Jw8b73JG76W45tRo7VbsNhuwzy5c+sFFVrjK/Tm/SPPkqjgqnF4LRHBCS1Po8kKbldzXg1MfcmfcxpNDl3Ms6PGU2gkPORt5vdeBRgcfE/mjCQSYTBCHPRxShsLRBCJMjaPeGwCqLWKCKB5DICGIkskAISWI0jEEMoJMNl3UOYc651HnAuqmWGMvl1RN5WgCFaZANW+oiRpqoo6aaKAmmqiJFmqijZrooFbf9VrYEmKgR3FCZZB92M2A8RYFonTotYijURjFOIw0Scd2zJjKbczktj13QtAChCKXarNXBkKvtwgfG+ljK33spI+99HGQPo7SxymihLMs4SJLuMoSbrKEuyzhIUvFT+9149ttg1D3vK5i1/2bQ8We4s5OZSrUXR2K4aP3PbCD0zhEMyIlIhUie5A/DtA/73kIsBTCtGIqIa+4MX4+oWKXxs7bp+2TZ2OnX/naefnl7n7/6ve33zS14eXu9jbnFz763//hd2dnP/1pvUg58e7qr1HmVG9v+/+4CAAIZizJgukgABbgayGIxBkIQZdKIkFHANPQCgIio3THVDCRIAk6CAkmydcBWDukBBOt02HAMmHAyGQy2uqaAgHgQ1+UP3ytfmlq6z8zJf4FgF/+Pd4PAH9fsfgZFj73N2oGELn/EnC4iaCFoT/rNfnSrtXNZmQgrsz8sriAZwB1WTJOvfqBd/H58WaL6NmdkJ5Ft7mtBaPyNlCeLypMEu4RxA/LhlwxR81OQ9nkKxzn/ezrGT2gfreOVz7OtTPtRNtq6wAKsZYy6QiOytz2PQig1hqmx481w+DxJo2u4vGcd020TVDb6WmZ+gbvm4+BJDbtypOe6vDQQfwo7+U1Pa67lUpOvsso5DMthjR9Q12lUasBAAA=";

    /// Decode the shared WOFF2 fixture once.
    fn tiny_woff2_bytes() -> Vec<u8> {
        base64_decode(TINY_WOFF2_B64).expect("decode TINY_WOFF2 base64")
    }

    /// Minimal standard-alphabet base64 encoder (no line wrapping) — used to turn
    /// the reconstructed sfnt into a `data:font/ttf;base64,…` URI for the test.
    fn b64(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                T[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                T[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    /// `MeasureBook::width` for one run with an explicit family.
    fn width_with(book: &MeasureBook, family: &str, text: &str) -> f64 {
        let style = Style {
            font_family: family.into(),
            font_size: 16.0,
            font_weight: 400,
            ..Style::default()
        };
        book.width(text, &style)
    }

    #[test]
    fn font_face_inline_ttf_data_uri_registers_and_renders_from_inline_face() {
        // Reconstruct a real sfnt (JetBrains Mono subset) and serve it as a
        // `data:font/ttf;base64,…` @font-face. An element using that family must
        // render from the inline face, not the bundled fallback.
        let sfnt = crate::font::webfont::sfnt_from_web_font(&tiny_woff2_bytes())
            .expect("reconstruct sfnt from the WOFF2 fixture");
        let uri = format!("data:font/ttf;base64,{}", b64(&sfnt));
        let html = format!(
            "<style>@font-face {{ font-family: Inline; src: url({uri}); }}</style>\
             <p style=\"font-family:Inline\">A</p>"
        );
        let nodes = dom::parse(&html);

        // One inline face is produced, for the declared family.
        let combined = fonts_with_inline_faces(&nodes, &[]);
        assert_eq!(
            combined.len(),
            1,
            "exactly one inline @font-face registered"
        );
        assert_eq!(combined[0].family, "inline", "family lower-cased");
        assert_eq!(combined[0].weight, 400, "default weight");
        assert!(!combined[0].italic, "default upright");
        // The registered bytes are a parseable sfnt with the 'A' glyph.
        let face = TrueTypeFont::parse(&combined[0].ttf).expect("inline face parses");
        assert_eq!(face.gid_for_unicode('A' as u32), Some(1), "'A' is glyph 1");

        // The 'A' advance from the inline (monospace) face differs from the
        // bundled fallback's — proof the run actually measures with the inline
        // face, not the fallback.
        let book_inline = MeasureBook::new(&combined);
        let book_fallback = MeasureBook::new(&[]);
        let w_inline = width_with(&book_inline, "Inline", "A");
        let w_fallback = width_with(&book_fallback, "Inline", "A");
        assert!(w_inline > 0.0, "inline face yields a real advance");
        assert!(
            (w_inline - w_fallback).abs() > 0.1,
            "inline-face advance ({w_inline}) differs from the bundled fallback ({w_fallback})"
        );

        // And the full render path produces a valid PDF with the inline face.
        let pdf = render(&html, &[], 612.0, 792.0, 36.0);
        assert!(pdf.starts_with(b"%PDF-"), "valid PDF header");
    }

    #[test]
    fn font_face_inline_woff2_data_uri_resolves_via_sfnt_from_web_font() {
        // A WOFF2 src is decompressed + glyf-detransformed by
        // `sfnt_from_web_font` before registration.
        let uri = format!("data:font/woff2;base64,{TINY_WOFF2_B64}");
        let html = format!(
            "<style>@font-face {{ font-family: Inline; src: url({uri}) format('woff2'); }}</style>\
             <p style=\"font-family:Inline\">A</p>"
        );
        let nodes = dom::parse(&html);

        let combined = fonts_with_inline_faces(&nodes, &[]);
        assert_eq!(combined.len(), 1, "WOFF2 @font-face registered");
        let face = TrueTypeFont::parse(&combined[0].ttf).expect("reconstructed sfnt parses");
        assert_eq!(face.num_glyphs(), 2, "2-glyph subset reconstructed");
        assert_eq!(face.gid_for_unicode('A' as u32), Some(1), "'A' present");

        // Renders from the inline face (advance differs from the fallback).
        let book_inline = MeasureBook::new(&combined);
        let w_inline = width_with(&book_inline, "Inline", "A");
        let w_fallback = width_with(&MeasureBook::new(&[]), "Inline", "A");
        assert!(
            (w_inline - w_fallback).abs() > 0.1,
            "WOFF2 inline-face advance differs from the fallback"
        );
    }

    #[test]
    fn font_face_inline_weight_and_style_pick_the_right_face() {
        // `font-weight: bold` + `font-style: italic` on the rule must carry onto
        // the registered face so the CSS weight/style matcher selects it.
        let uri = format!("data:font/woff2;base64,{TINY_WOFF2_B64}");
        let html = format!(
            "<style>@font-face {{ font-family: Inline; font-weight: bold; \
             font-style: italic; src: url({uri}); }}</style>\
             <p style=\"font-family:Inline\">A</p>"
        );
        let nodes = dom::parse(&html);
        let combined = fonts_with_inline_faces(&nodes, &[]);
        assert_eq!(combined.len(), 1, "one face");
        assert_eq!(combined[0].weight, 700, "bold → 700");
        assert!(combined[0].italic, "italic captured");

        // A bold+italic run for the family resolves to this provided face.
        let book = MeasureBook::new(&combined);
        let style = Style {
            font_family: "Inline".into(),
            font_size: 16.0,
            font_weight: 700,
            italic: true,
            bold: true,
            ..Style::default()
        };
        let (_face, matched) = book
            .provided_match(&style)
            .expect("the bold-italic inline face matches");
        assert_eq!(matched, 700, "matched the 700 inline face");
    }

    #[test]
    fn font_face_numeric_weight_is_parsed() {
        // A numeric `font-weight` (here 300) is registered as-is.
        let uri = format!("data:font/woff2;base64,{TINY_WOFF2_B64}");
        let html = format!(
            "<style>@font-face {{ font-family: Inline; font-weight: 300; \
             src: url({uri}); }}</style><p style=\"font-family:Inline\">A</p>"
        );
        let nodes = dom::parse(&html);
        let combined = fonts_with_inline_faces(&nodes, &[]);
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].weight, 300, "numeric weight 300 preserved");
    }

    #[test]
    fn font_face_external_url_src_is_ignored() {
        // A non-`data:` src is never fetched, so no face is registered — the
        // engine stays zero-network.
        let html = "<style>@font-face { font-family: Inline; \
                    src: url(https://example.com/Inline.woff2) format('woff2'); }</style>\
                    <p style=\"font-family:Inline\">A</p>";
        let nodes = dom::parse(html);
        let combined = fonts_with_inline_faces(&nodes, &[]);
        assert!(
            combined.is_empty(),
            "external url() src yields no inline face: {combined:?}"
        );
        // Still renders (text falls back to the bundled face).
        let pdf = render(html, &[], 612.0, 792.0, 36.0);
        assert!(
            pdf.starts_with(b"%PDF-"),
            "valid PDF even with no usable face"
        );
    }

    #[test]
    fn font_face_first_usable_src_wins_and_caller_fonts_follow() {
        // `src` lists an unfetchable external URL first, then a usable `data:`
        // URI: the data: one is the registered face. Caller fonts follow it.
        let uri = format!("data:font/woff2;base64,{TINY_WOFF2_B64}");
        let html = format!(
            "<style>@font-face {{ font-family: Inline; \
             src: url(https://example.com/x.woff2) format('woff2'), url({uri}) format('woff2'); }}\
             </style><p style=\"font-family:Inline\">A</p>"
        );
        let nodes = dom::parse(&html);
        let host = ProvidedFont {
            family: "Host".into(),
            weight: 400,
            italic: false,
            ttf: bundled::FALLBACK_TTF.to_vec(),
        };
        let combined = fonts_with_inline_faces(&nodes, std::slice::from_ref(&host));
        // Inline face first (Inline), caller font after (Host).
        assert_eq!(combined.len(), 2, "inline + caller face");
        assert_eq!(combined[0].family, "inline", "inline @font-face first");
        assert_eq!(combined[1].family, "Host", "caller font follows");
    }
}
