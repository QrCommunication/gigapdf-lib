//! Page geometry for the HTML→PDF renderer: named paper sizes (ISO A/B series
//! and the common US sizes), per-side margins, and the [`RenderOptions`] bundle
//! that carries headers/footers and automatic page numbering.

use std::collections::BTreeMap;

/// Resolve a named paper size to `(width, height)` in **PDF points** (portrait).
///
/// Accepts the ISO A-series `a0`…`a6`, ISO B `b4`/`b5`, and US `letter`,
/// `legal`, `tabloid`/`ledger`, `executive` (case-insensitive). A `-landscape`
/// (or `-l`) suffix — or a `landscape ` prefix — swaps the axes. Returns `None`
/// for an unknown name so the caller can fall back.
///
/// ```
/// # use gigapdf_core::html::page_size;
/// let (w, h) = page_size("A4").unwrap();
/// assert!((w - 595.27).abs() < 0.1 && (h - 841.89).abs() < 0.1);
/// let (lw, lh) = page_size("a4-landscape").unwrap();
/// assert!(lw > lh, "landscape is wider than tall");
/// ```
pub fn page_size(name: &str) -> Option<(f64, f64)> {
    let raw = name.trim().to_ascii_lowercase();
    let (base, landscape) = if let Some(b) = raw
        .strip_suffix("-landscape")
        .or_else(|| raw.strip_suffix(" landscape"))
        .or_else(|| raw.strip_suffix("-l"))
    {
        (b.trim().to_string(), true)
    } else if let Some(b) = raw
        .strip_prefix("landscape ")
        .or_else(|| raw.strip_prefix("landscape-"))
    {
        (b.trim().to_string(), true)
    } else {
        (raw.clone(), false)
    };

    // 1mm = 72/25.4 pt; 1in = 72 pt.
    const MM: f64 = 72.0 / 25.4;
    let mm = |w: f64, h: f64| (w * MM, h * MM);
    let inch = |w: f64, h: f64| (w * 72.0, h * 72.0);

    let (w, h) = match base.as_str() {
        "a0" => mm(841.0, 1189.0),
        "a1" => mm(594.0, 841.0),
        "a2" => mm(420.0, 594.0),
        "a3" => mm(297.0, 420.0),
        "a4" => mm(210.0, 297.0),
        "a5" => mm(148.0, 210.0),
        "a6" => mm(105.0, 148.0),
        "b4" => mm(250.0, 353.0),
        "b5" => mm(176.0, 250.0),
        "letter" | "us-letter" => inch(8.5, 11.0),
        "legal" | "us-legal" => inch(8.5, 14.0),
        "tabloid" | "ledger" => inch(11.0, 17.0),
        "executive" => inch(7.25, 10.5),
        _ => return None,
    };
    Some(if landscape { (h, w) } else { (w, h) })
}

/// Per-side page margins in points.
#[derive(Debug, Clone, Copy)]
pub struct Margins {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl Margins {
    /// The same margin on every side.
    pub fn uniform(m: f64) -> Self {
        Self {
            top: m,
            right: m,
            bottom: m,
            left: m,
        }
    }

    /// Vertical (`top`/`bottom`) and horizontal (`left`/`right`) margins.
    pub fn symmetric(vertical: f64, horizontal: f64) -> Self {
        Self {
            top: vertical,
            right: horizontal,
            bottom: vertical,
            left: horizontal,
        }
    }
}

impl Default for Margins {
    fn default() -> Self {
        Self::uniform(36.0) // 0.5"
    }
}

/// Everything the renderer needs about the page: size, margins, and optional
/// running header/footer with `{{page}}` / `{{pages}}` substitution.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub page_w: f64,
    pub page_h: f64,
    pub margins: Margins,
    /// HTML painted in the **top margin** of every page. `{{page}}` and
    /// `{{pages}}` are replaced with the current / total page number.
    pub header: Option<String>,
    /// HTML painted in the **bottom margin** of every page (same tokens).
    pub footer: Option<String>,
    /// Distance from the top edge to the header block's top (within the top
    /// margin). Default `18pt`.
    pub header_offset: f64,
    /// Distance from the bottom edge to the footer block's bottom (within the
    /// bottom margin). Default `18pt`.
    pub footer_offset: f64,
    /// Number assigned to the first page for the `{{page}}` token. Default `1`.
    pub start_page_number: u32,
    /// Host-fetched external resources, keyed by the exact URL referenced in the
    /// HTML (`<img src>`, later CSS `url(...)`). The engine itself never touches
    /// the network: the host downloads each URL listed by
    /// [`needed_resources`](crate::html::needed_resources) and supplies the bytes
    /// here, so external images render with browser fidelity while staying
    /// zero-dependency. `data:` URIs are decoded inline and need no entry.
    pub resources: BTreeMap<String, Vec<u8>>,
}

impl RenderOptions {
    /// A4-style defaults at an explicit size: uniform 36pt margins, no
    /// header/footer, page numbering from 1.
    pub fn new(page_w: f64, page_h: f64) -> Self {
        Self {
            page_w,
            page_h,
            margins: Margins::default(),
            header: None,
            footer: None,
            header_offset: 18.0,
            footer_offset: 18.0,
            start_page_number: 1,
            resources: BTreeMap::new(),
        }
    }

    /// Options for a named paper size (`"A4"`, `"a3-landscape"`, `"letter"`, …).
    /// Unknown names fall back to A4 portrait.
    pub fn for_size(name: &str) -> Self {
        let (w, h) = page_size(name).unwrap_or((595.2756, 841.8898));
        Self::new(w, h)
    }
}

/// Replace the running-head tokens `{{page}}` and `{{pages}}` (whitespace inside
/// the braces tolerated) in a header/footer snippet.
pub(crate) fn substitute_tokens(s: &str, page: u32, pages: u32) -> String {
    let mut out = s.to_string();
    for (tok, val) in [("page", page), ("pages", pages), ("total", pages)] {
        // Match `{{tok}}` with optional inner spaces, without a regex engine.
        let needle_compact = format!("{{{{{tok}}}}}");
        out = out.replace(&needle_compact, &val.to_string());
        let needle_spaced = format!("{{{{ {tok} }}}}");
        out = out.replace(&needle_spaced, &val.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_series_sizes_and_landscape() {
        let (w, h) = page_size("A4").unwrap();
        assert!(
            (w - 595.276).abs() < 0.1 && (h - 841.890).abs() < 0.1,
            "A4 {w}x{h}"
        );
        let (w3, h3) = page_size("a3").unwrap();
        assert!(h3 > h && w3 > w, "A3 is bigger than A4");
        let (lw, lh) = page_size("A4-landscape").unwrap();
        assert!(lw > lh && (lw - h).abs() < 0.1, "landscape swaps axes");
        assert!(page_size("letter").unwrap() == (612.0, 792.0));
        assert!(page_size("legal").unwrap() == (612.0, 1008.0));
        assert!(page_size("nope").is_none());
    }

    #[test]
    fn token_substitution() {
        assert_eq!(
            substitute_tokens("Page {{page}} / {{pages}}", 2, 7),
            "Page 2 / 7"
        );
        assert_eq!(substitute_tokens("{{ page }}", 3, 9), "3");
        assert_eq!(substitute_tokens("no tokens", 1, 1), "no tokens");
    }

    #[test]
    fn margins_helpers() {
        let u = Margins::uniform(10.0);
        assert_eq!((u.top, u.right, u.bottom, u.left), (10.0, 10.0, 10.0, 10.0));
        let s = Margins::symmetric(20.0, 30.0);
        assert_eq!((s.top, s.right, s.bottom, s.left), (20.0, 30.0, 20.0, 30.0));
    }

    #[test]
    fn every_named_size_resolves() {
        for name in [
            "a0",
            "a1",
            "a2",
            "a3",
            "a4",
            "a5",
            "a6",
            "b4",
            "b5",
            "letter",
            "us-letter",
            "legal",
            "us-legal",
            "tabloid",
            "ledger",
            "executive",
        ] {
            let (w, h) = page_size(name).unwrap_or_else(|| panic!("{name} should resolve"));
            assert!(w > 0.0 && h > 0.0 && h >= w, "{name} portrait {w}x{h}");
        }
    }

    #[test]
    fn landscape_prefix_and_short_suffix() {
        let (pw, ph) = page_size("a4").unwrap();
        // `landscape ` prefix.
        let (lw, lh) = page_size("landscape a4").unwrap();
        assert_eq!((lw, lh), (ph, pw));
        // `landscape-` prefix.
        let (lw2, lh2) = page_size("landscape-a4").unwrap();
        assert_eq!((lw2, lh2), (ph, pw));
        // `-l` short suffix.
        let (lw3, lh3) = page_size("a4-l").unwrap();
        assert_eq!((lw3, lh3), (ph, pw));
        // Case-insensitive.
        assert_eq!(page_size("LETTER"), page_size("letter"));
    }

    #[test]
    fn render_options_for_size_and_fallback() {
        let letter = RenderOptions::for_size("letter");
        assert_eq!((letter.page_w, letter.page_h), (612.0, 792.0));
        assert_eq!(letter.start_page_number, 1);
        assert!(letter.header.is_none() && letter.footer.is_none());
        // Unknown name → A4 portrait fallback.
        let unknown = RenderOptions::for_size("not-a-size");
        assert!((unknown.page_w - 595.2756).abs() < 0.01);
        assert!((unknown.page_h - 841.8898).abs() < 0.01);
    }

    #[test]
    fn substitute_total_token_alias() {
        // `{{total}}` is an alias for the page count.
        assert_eq!(substitute_tokens("p {{total}}", 1, 5), "p 5");
    }
}
