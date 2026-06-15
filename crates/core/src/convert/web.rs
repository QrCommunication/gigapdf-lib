//! HTML export — standalone, self-contained, zero-dependency.
//!
//! Each page becomes a sized `<div>`; text runs are absolutely-positioned
//! `<span>`s carrying the recovered font/weight/style/colour, images are
//! inlined as `data:` URIs, and shapes are bordered boxes. Real, selectable
//! content — not a page raster.

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
}
