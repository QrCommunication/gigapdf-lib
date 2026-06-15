//! Reverse conversions: `TXT / HTML / RTF / DOCX / ODT / PPTX / XLSX / ODS → PDF`.
//!
//! Every source reduces to a list of text paragraphs (and, for slides/sheets,
//! page-break sections); [`flow_to_pdf`] lays them onto pages with the
//! [`PdfBuilder`](super::build::PdfBuilder). Office files are ZIP-of-XML, so we
//! read the relevant part (via [`super::zip::read_zip`]) and recover paragraphs
//! by replacing block-boundary tags with newlines and stripping the rest — which
//! works for both the engine's own exports and simple external files.
//!
//! This is a text-faithful conversion (all content, reading order, pagination),
//! not a pixel-perfect re-layout — the honest zero-dependency scope.

use super::build::{PdfBuilder, StdFont};

// ─────────────────────────────── text helpers ──────────────────────────────

/// Decode the XML/HTML entities our exporters (and common tools) emit.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let (decoded, len) = if tail.starts_with("&amp;") {
            ('&', 5)
        } else if tail.starts_with("&lt;") {
            ('<', 4)
        } else if tail.starts_with("&gt;") {
            ('>', 4)
        } else if tail.starts_with("&quot;") {
            ('"', 6)
        } else if tail.starts_with("&apos;") {
            ('\'', 6)
        } else if tail.starts_with("&#") {
            // Numeric entity &#NN; or &#xHH;
            if let Some(semi) = tail.find(';') {
                let body = &tail[2..semi];
                let code =
                    if let Some(hex) = body.strip_prefix('x').or_else(|| body.strip_prefix('X')) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        body.parse::<u32>().ok()
                    };
                match code.and_then(char::from_u32) {
                    Some(c) => (c, semi + 1),
                    None => ('&', 1),
                }
            } else {
                ('&', 1)
            }
        } else {
            ('&', 1)
        };
        out.push(decoded);
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// Recover paragraphs from XML: each `boundary` tag becomes a paragraph break,
/// each `cell_sep` tag a space; all other tags are stripped and entities
/// decoded. Robust for OOXML/ODF/HTML alike.
fn paragraphs_from_xml(xml: &str, boundaries: &[&str], cell_sep: &[&str]) -> Vec<String> {
    let mut s = xml.to_string();
    for tag in cell_sep {
        s = s.replace(tag, " \u{0}"); // keep a space but not a break
    }
    for tag in boundaries {
        s = s.replace(tag, "\n");
    }
    let mut text = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = unescape(&text).replace('\u{0}', "");
    text.split('\n')
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect()
}

/// Greedy word-wrap to at most `max_chars` per line (rough, char-count based).
fn wrap(text: &str, max_chars: usize) -> Vec<String> {
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let add = if line.is_empty() { 0 } else { 1 };
        if line.chars().count() + add + word.chars().count() > max_chars && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        // A single over-long word: hard-split it.
        if word.chars().count() > max_chars {
            for chunk in chunk_chars(word, max_chars) {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                lines.push(chunk);
            }
        } else {
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn chunk_chars(s: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars.chunks(n).map(|c| c.iter().collect()).collect()
}

// ─────────────────────────────── layout → PDF ──────────────────────────────

/// Flow `sections` of paragraphs onto US-Letter pages. Each section after the
/// first starts on a new page (slides → one per slide; sheets → one per sheet).
pub fn flow_to_pdf(sections: &[Vec<String>]) -> Vec<u8> {
    const W: f64 = 612.0;
    const H: f64 = 792.0;
    const MARGIN: f64 = 56.0;
    const SIZE: f64 = 11.0;
    let line_h = SIZE * 1.4;
    let max_chars = ((W - 2.0 * MARGIN) / (SIZE * 0.5)).floor().max(8.0) as usize;

    let mut b = PdfBuilder::new();
    let mut page = b.add_page(W, H);
    let mut y = MARGIN;
    let mut page_has_content = false;

    for (si, section) in sections.iter().enumerate() {
        if si > 0 && page_has_content {
            page = b.add_page(W, H);
            y = MARGIN;
            page_has_content = false;
        }
        for para in section {
            for line in wrap(para, max_chars) {
                if y + line_h > H - MARGIN {
                    page = b.add_page(W, H);
                    y = MARGIN;
                }
                b.text(
                    page,
                    MARGIN,
                    y,
                    SIZE,
                    &line,
                    StdFont::Helvetica,
                    [0.0, 0.0, 0.0],
                );
                y += line_h;
                page_has_content = true;
            }
            y += line_h * 0.4; // paragraph spacing
        }
    }
    b.finish()
}

// ─────────────────────────────── sources → PDF ─────────────────────────────

/// Plain text → PDF (one paragraph per line; blank lines add spacing).
pub fn txt_to_pdf(text: &str) -> Vec<u8> {
    let paras: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
    flow_to_pdf(&[paras])
}

/// HTML → PDF (text-faithful: paragraph/line tags become breaks).
pub fn html_to_pdf(html: &str) -> Vec<u8> {
    let paras = paragraphs_from_xml(
        html,
        &[
            "</p>", "</div>", "</span>", "</h1>", "</h2>", "</h3>", "</li>", "<br>", "<br/>",
            "<br />",
        ],
        &[],
    );
    flow_to_pdf(&[paras])
}

/// DOCX → PDF.
pub fn docx_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    let xml = zip
        .get("word/document.xml")
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    flow_to_pdf(&[paragraphs_from_xml(&xml, &["</w:p>"], &[])])
}

/// ODT → PDF.
pub fn odt_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    let xml = zip
        .get("content.xml")
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    flow_to_pdf(&[paragraphs_from_xml(&xml, &["</text:p>", "</text:h>"], &[])])
}

/// PPTX → PDF (one page per slide).
pub fn pptx_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    // Slides in numeric order: slide1.xml, slide2.xml, …
    let mut slides: Vec<(usize, String)> = zip
        .iter()
        .filter(|(k, _)| k.starts_with("ppt/slides/slide") && k.ends_with(".xml"))
        .filter_map(|(k, v)| {
            let n: usize = k["ppt/slides/slide".len()..k.len() - 4].parse().ok()?;
            Some((n, String::from_utf8_lossy(v).into_owned()))
        })
        .collect();
    slides.sort_by_key(|(n, _)| *n);
    let mut sections: Vec<Vec<String>> = slides
        .iter()
        .map(|(_, xml)| paragraphs_from_xml(xml, &["</a:p>"], &[]))
        .collect();
    if sections.is_empty() {
        sections.push(Vec::new());
    }
    flow_to_pdf(&sections)
}

/// XLSX → PDF (one page per sheet; cells space-separated per row).
pub fn xlsx_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    let mut sheets: Vec<(usize, String)> = zip
        .iter()
        .filter(|(k, _)| k.starts_with("xl/worksheets/sheet") && k.ends_with(".xml"))
        .filter_map(|(k, v)| {
            let n: usize = k["xl/worksheets/sheet".len()..k.len() - 4].parse().ok()?;
            Some((n, String::from_utf8_lossy(v).into_owned()))
        })
        .collect();
    sheets.sort_by_key(|(n, _)| *n);
    let mut sections: Vec<Vec<String>> = sheets
        .iter()
        .map(|(_, xml)| paragraphs_from_xml(xml, &["</row>"], &["</c>"]))
        .collect();
    if sections.is_empty() {
        sections.push(Vec::new());
    }
    flow_to_pdf(&sections)
}

/// ODS → PDF (rows as paragraphs, cells space-separated).
pub fn ods_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    let xml = zip
        .get("content.xml")
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    flow_to_pdf(&[paragraphs_from_xml(
        &xml,
        &["</table:table-row>"],
        &["</table:table-cell>"],
    )])
}

/// ODP → PDF (one page per slide; text runs from `draw:text-box`).
pub fn odp_to_pdf(bytes: &[u8]) -> Vec<u8> {
    let zip = super::zip::read_zip(bytes);
    let xml = zip
        .get("content.xml")
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    // Slides are `<draw:page>`; their text lives in `<text:p>` runs.
    let sections: Vec<Vec<String>> = xml
        .split("<draw:page")
        .skip(1)
        .map(|slide| paragraphs_from_xml(slide, &["</text:p>"], &[]))
        .collect();
    let sections = if sections.is_empty() {
        vec![Vec::new()]
    } else {
        sections
    };
    flow_to_pdf(&sections)
}

/// Auto-detect an Office container and convert to PDF. Returns `None` if the
/// bytes are not a recognized OOXML/ODF archive.
pub fn office_to_pdf(bytes: &[u8]) -> Option<Vec<u8>> {
    let zip = super::zip::read_zip(bytes);
    if zip.contains_key("word/document.xml") {
        Some(docx_to_pdf(bytes))
    } else if zip.contains_key("ppt/presentation.xml") {
        Some(pptx_to_pdf(bytes))
    } else if zip.contains_key("xl/workbook.xml") {
        Some(xlsx_to_pdf(bytes))
    } else if let Some(mimetype) = zip.get("mimetype") {
        let mt = String::from_utf8_lossy(mimetype);
        if mt.contains("opendocument.text") {
            Some(odt_to_pdf(bytes))
        } else if mt.contains("opendocument.spreadsheet") {
            Some(ods_to_pdf(bytes))
        } else if mt.contains("opendocument.presentation") {
            Some(odp_to_pdf(bytes))
        } else {
            None
        }
    } else {
        None
    }
}

// ─────────────────────────────── RTF (both ways) ───────────────────────────

/// Escape a string for an RTF body (`\`, `{`, `}` and non-ASCII via `\uN?`).
fn rtf_escape(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            c if (c as u32) < 0x80 => out.push(c),
            c => {
                // RTF \uN uses a signed 16-bit code unit + an ASCII fallback char.
                let code = c as u32;
                if code <= 0xFFFF {
                    let signed = if code > 0x7FFF {
                        code as i32 - 0x10000
                    } else {
                        code as i32
                    };
                    out.push_str(&format!("\\u{signed}?"));
                } else {
                    out.push('?');
                }
            }
        }
    }
}

/// Export paragraphs to an RTF document.
pub fn to_rtf(paragraphs: &[String]) -> Vec<u8> {
    let mut s = String::from("{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Helvetica;}}\\fs22\n");
    for (i, para) in paragraphs.iter().enumerate() {
        if i > 0 {
            s.push_str("\\par\n");
        }
        rtf_escape(para, &mut s);
    }
    s.push_str("}\n");
    s.into_bytes()
}

/// Extract plain text paragraphs from an RTF document (minimal control-word
/// parser: handles groups, `\par`, `\'xx` hex bytes, `\uN` unicode, skips other
/// control words and the font/color tables).
fn rtf_to_paragraphs(rtf: &str) -> Vec<String> {
    let bytes = rtf.as_bytes();
    let mut paras = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    let mut skip_group_depth: Option<i32> = None;
    let mut depth = 0i32;
    let mut uc_count = 1i64; // `\ucN`: fallback chars to skip after each `\uN`

    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                if let Some(d) = skip_group_depth {
                    if depth <= d {
                        skip_group_depth = None;
                    }
                }
                depth -= 1;
                i += 1;
            }
            b'\\' => {
                // Control word / symbol.
                if i + 1 < bytes.len() && !bytes[i + 1].is_ascii_alphanumeric() {
                    match bytes[i + 1] {
                        b'\'' if i + 3 < bytes.len() => {
                            let hex = &rtf[i + 2..i + 4];
                            if let Ok(b) = u8::from_str_radix(hex, 16) {
                                if skip_group_depth.is_none() {
                                    // WinAnsi byte → char.
                                    cur.push(b as char);
                                }
                            }
                            i += 4;
                        }
                        b'\\' | b'{' | b'}' => {
                            if skip_group_depth.is_none() {
                                cur.push(bytes[i + 1] as char);
                            }
                            i += 2;
                        }
                        _ => i += 2,
                    }
                    continue;
                }
                // Alphabetic control word + optional numeric parameter.
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let word = &rtf[start..j];
                let mut k = j;
                let mut neg = false;
                if k < bytes.len() && bytes[k] == b'-' {
                    neg = true;
                    k += 1;
                }
                let num_start = k;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                let param: Option<i64> =
                    rtf[num_start..k]
                        .parse()
                        .ok()
                        .map(|n: i64| if neg { -n } else { n });
                let mut fallback_skip = 0i64;
                match word {
                    "par" => {
                        if skip_group_depth.is_none() {
                            paras.push(std::mem::take(&mut cur));
                        }
                    }
                    "uc" => {
                        if let Some(n) = param {
                            uc_count = n.max(0);
                        }
                    }
                    "u" => {
                        if let Some(n) = param {
                            if skip_group_depth.is_none() {
                                let code = if n < 0 {
                                    (n + 0x10000) as u32
                                } else {
                                    n as u32
                                };
                                if let Some(ch) = char::from_u32(code) {
                                    cur.push(ch);
                                }
                            }
                            fallback_skip = uc_count;
                        }
                    }
                    "fonttbl" | "colortbl" | "stylesheet" | "info" | "pict" | "object" => {
                        skip_group_depth = Some(depth);
                    }
                    _ => {}
                }
                // A single space after a control word is its delimiter — consume it.
                if k < bytes.len() && bytes[k] == b' ' {
                    k += 1;
                }
                // Skip the `\uc`-count fallback characters that follow a `\uN`.
                for _ in 0..fallback_skip {
                    if k >= bytes.len() {
                        break;
                    }
                    let mut adv = 1;
                    while k + adv < bytes.len() && (bytes[k + adv] & 0xC0) == 0x80 {
                        adv += 1;
                    }
                    k += adv;
                }
                i = k;
            }
            b'\r' | b'\n' => i += 1,
            _ => {
                if skip_group_depth.is_none() {
                    cur.push(c as char);
                }
                i += 1;
            }
        }
    }
    if !cur.trim().is_empty() {
        paras.push(cur);
    }
    paras
        .into_iter()
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .collect()
}

/// RTF → PDF.
pub fn rtf_to_pdf(rtf: &str) -> Vec<u8> {
    flow_to_pdf(&[rtf_to_paragraphs(rtf)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opens(pdf: &[u8]) -> crate::Document {
        crate::Document::open(pdf).expect("valid PDF")
    }

    #[test]
    fn unescape_decodes_entities() {
        assert_eq!(
            unescape("a &amp; b &lt;c&gt; &#65; &#x42;"),
            "a & b <c> A B"
        );
    }

    #[test]
    fn xml_paragraphs_strip_tags_and_split() {
        let xml = "<w:p><w:r><w:t>Hello</w:t></w:r></w:p><w:p><w:t>World &amp; co</w:t></w:p>";
        let paras = paragraphs_from_xml(xml, &["</w:p>"], &[]);
        assert_eq!(paras, vec!["Hello".to_string(), "World & co".to_string()]);
    }

    #[test]
    fn txt_to_pdf_is_valid_and_has_text() {
        let pdf = txt_to_pdf("First line\nSecond line\nThird");
        let doc = opens(&pdf);
        assert!(doc.page_count() >= 1);
        let text = doc.to_text();
        assert!(
            text.contains("Second line"),
            "text round-trips into the PDF"
        );
    }

    #[test]
    fn rtf_round_trips_text() {
        let rtf = to_rtf(&["Café déjà".to_string(), "Second \\ {brace}".to_string()]);
        let s = String::from_utf8(rtf).unwrap();
        assert!(s.starts_with("{\\rtf1"));
        let back = rtf_to_paragraphs(&s);
        assert_eq!(
            back,
            vec!["Café déjà".to_string(), "Second \\ {brace}".to_string()]
        );
    }

    #[test]
    fn long_paragraph_wraps_across_lines() {
        let lines = wrap(&"word ".repeat(60), 40);
        assert!(lines.len() > 1, "wrapped into multiple lines");
        assert!(lines.iter().all(|l| l.chars().count() <= 40));
    }
}
