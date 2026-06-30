//! Google Fonts request helpers — pure, zero-dependency.
//!
//! The WASM sandbox has no network stack, so the engine never performs HTTP.
//! Instead it *computes what to fetch* and *parses what the host fetched*; the
//! host (browser/Node) does the actual download and hands the bytes back to
//! [`Document::embed_truetype_font`](crate::Document::embed_truetype_font).
//!
//! Flow:
//! 1. [`css_url`] → the CSS2 API URL for a family/weight/style.
//! 2. Host fetches it **with a legacy `User-Agent`** (so Google returns TTF
//!    `src` URLs, not WOFF2) — e.g. `Mozilla/5.0 (Windows NT 10.0)`.
//! 3. [`parse_css_font_url`] → the `fonts.gstatic.com` font URL from that CSS.
//! 4. Host fetches that URL (validate the host with [`is_gstatic_url`] —
//!    anti-SSRF) and passes the TTF bytes to the engine for embedding.

/// Build the Google Fonts CSS2 API URL for `family` at `weight` (e.g. 400, 700)
/// and `italic`. Spaces in the family become `+`.
pub fn css_url(family: &str, weight: u16, italic: bool) -> String {
    let fam = family.trim().replace(' ', "+");
    let ital = italic as u8;
    format!("https://fonts.googleapis.com/css2?family={fam}:ital,wght@{ital},{weight}&display=swap")
}

/// True if `url` points at the Google Fonts static host (the only host the
/// engine vouches for; the host should refuse to fetch anything else — SSRF).
pub fn is_gstatic_url(url: &str) -> bool {
    url.starts_with("https://fonts.gstatic.com/")
}

/// Extract the first `fonts.gstatic.com` font URL from a Google Fonts CSS2
/// response (`src: url(...)`). Returns `None` if none is present or it is not on
/// the trusted host.
pub fn parse_css_font_url(css: &str) -> Option<String> {
    let mut rest = css;
    while let Some(open) = rest.find("url(") {
        let after = &rest[open + 4..];
        if let Some(close) = after.find(')') {
            let raw = after[..close].trim().trim_matches(['\'', '"']);
            if is_gstatic_url(raw) {
                return Some(raw.to_string());
            }
            rest = &after[close + 1..];
        } else {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_css2_url() {
        assert_eq!(
            css_url("Open Sans", 700, true),
            "https://fonts.googleapis.com/css2?family=Open+Sans:ital,wght@1,700&display=swap"
        );
        assert_eq!(
            css_url("Roboto", 400, false),
            "https://fonts.googleapis.com/css2?family=Roboto:ital,wght@0,400&display=swap"
        );
    }

    #[test]
    fn extracts_gstatic_url_and_rejects_others() {
        let css = "@font-face{font-family:'Roboto';\
            src:url(https://fonts.gstatic.com/s/roboto/v30/abc.ttf) format('truetype');}";
        assert_eq!(
            parse_css_font_url(css).as_deref(),
            Some("https://fonts.gstatic.com/s/roboto/v30/abc.ttf")
        );

        // An attacker-controlled src must not be returned.
        let evil = "src:url(https://evil.example/font.ttf)";
        assert_eq!(parse_css_font_url(evil), None);
        assert!(!is_gstatic_url("http://fonts.gstatic.com/x")); // not https
    }

    #[test]
    fn skips_non_gstatic_then_finds_gstatic() {
        let css = "src:url(data:font/woff2;base64,AAAA);\
                   src:url('https://fonts.gstatic.com/s/x/y.ttf')";
        assert_eq!(
            parse_css_font_url(css).as_deref(),
            Some("https://fonts.gstatic.com/s/x/y.ttf")
        );
    }

    #[test]
    fn unterminated_url_open_paren_breaks_cleanly() {
        // A `url(` with no closing `)` hits the break arm → None, no panic.
        assert_eq!(
            parse_css_font_url("src:url(https://fonts.gstatic.com/x"),
            None
        );
        // No url( at all → None.
        assert_eq!(parse_css_font_url("body{color:red}"), None);
    }
}
