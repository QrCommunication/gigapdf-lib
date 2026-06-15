//! A pragmatic HTML tokenizer + tree builder.
//!
//! This is not a byte-for-byte WHATWG parser, but it handles the constructs
//! real documents use: nested elements, single/double/unquoted attributes,
//! character entities, void elements, raw-text elements (`<script>`/`<style>`),
//! comments and a few implied end-tags (so `<p>a<p>b` and `<li>…<li>…` close as
//! a browser would). It never panics on malformed input — it does its best and
//! moves on.

/// A parsed DOM node.
#[derive(Debug, Clone)]
pub enum Node {
    Element(Element),
    Text(String),
}

/// An HTML element: lowercased tag, attributes, children.
#[derive(Debug, Clone, Default)]
pub struct Element {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    /// Case-insensitive attribute lookup.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Void (self-closing) elements that never have children.
fn is_void(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Elements whose content is raw text (no nested markup) until their end tag.
fn is_raw_text(tag: &str) -> bool {
    matches!(tag, "script" | "style")
}

/// When opening `open`, auto-close a currently-open `cur` per simplified HTML
/// rules (paragraphs, list items, table cells, etc.).
fn implies_close(cur: &str, open: &str) -> bool {
    match cur {
        "p" => matches!(
            open,
            "p" | "div"
                | "ul"
                | "ol"
                | "table"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "blockquote"
                | "pre"
                | "hr"
                | "section"
                | "article"
                | "header"
                | "footer"
        ),
        "li" => open == "li",
        "dt" | "dd" => matches!(open, "dt" | "dd"),
        "td" | "th" => matches!(open, "td" | "th" | "tr"),
        "tr" => matches!(open, "tr"),
        "option" => matches!(open, "option"),
        _ => false,
    }
}

/// Parse an HTML fragment/document into a node forest.
pub fn parse(html: &str) -> Vec<Node> {
    let bytes = html.as_bytes();
    let mut pos = 0usize;
    // Stack of elements under construction; the root holds top-level nodes.
    let mut stack: Vec<Element> = vec![Element {
        tag: "#root".into(),
        ..Default::default()
    }];

    while pos < bytes.len() {
        if bytes[pos] == b'<' {
            // Comment / doctype / CDATA → skip.
            if html[pos..].starts_with("<!--") {
                pos = html[pos..]
                    .find("-->")
                    .map(|i| pos + i + 3)
                    .unwrap_or(bytes.len());
                continue;
            }
            if bytes.get(pos + 1) == Some(&b'!') || bytes.get(pos + 1) == Some(&b'?') {
                pos = html[pos..]
                    .find('>')
                    .map(|i| pos + i + 1)
                    .unwrap_or(bytes.len());
                continue;
            }
            // End tag.
            if bytes.get(pos + 1) == Some(&b'/') {
                let end = html[pos..]
                    .find('>')
                    .map(|i| pos + i)
                    .unwrap_or(bytes.len());
                let name = html[pos + 2..end].trim().to_ascii_lowercase();
                close_tag(&mut stack, &name);
                pos = (end + 1).min(bytes.len());
                continue;
            }
            // Start tag — find the matching '>' (naive; fine for real docs).
            if let Some(gt) = html[pos..].find('>') {
                let raw = &html[pos + 1..pos + gt];
                let self_closing = raw.ends_with('/');
                let raw = raw.trim_end_matches('/').trim();
                if let Some((tag, attrs)) = parse_tag(raw) {
                    // Implied end-tags.
                    while let Some(top) = stack.last() {
                        if top.tag != "#root" && implies_close(&top.tag, &tag) {
                            let done = stack.pop().unwrap();
                            push_child(&mut stack, Node::Element(done));
                        } else {
                            break;
                        }
                    }

                    pos = pos + gt + 1;

                    if is_raw_text(&tag) && !self_closing {
                        // Capture raw text up to the closing tag.
                        let close = format!("</{tag}");
                        let end = lower_find(&html[pos..], &close)
                            .map(|i| pos + i)
                            .unwrap_or(bytes.len());
                        let text = html[pos..end].to_string();
                        let mut el = Element {
                            tag: tag.clone(),
                            attrs,
                            ..Default::default()
                        };
                        if tag == "style" || tag == "script" {
                            el.children.push(Node::Text(text));
                        }
                        push_child(&mut stack, Node::Element(el));
                        // Advance past the end tag.
                        pos = html[end..]
                            .find('>')
                            .map(|i| end + i + 1)
                            .unwrap_or(bytes.len());
                        continue;
                    }

                    let el = Element {
                        tag: tag.clone(),
                        attrs,
                        ..Default::default()
                    };
                    if is_void(&tag) || self_closing {
                        push_child(&mut stack, Node::Element(el));
                    } else {
                        stack.push(el);
                    }
                    continue;
                }
            }
            // A stray '<' — treat as text.
            push_text(&mut stack, "<");
            pos += 1;
        } else {
            // Text run up to the next '<'.
            let end = html[pos..]
                .find('<')
                .map(|i| pos + i)
                .unwrap_or(bytes.len());
            let text = decode_entities(&html[pos..end]);
            push_text(&mut stack, &text);
            pos = end;
        }
    }

    // Close anything left open.
    while stack.len() > 1 {
        let done = stack.pop().unwrap();
        push_child(&mut stack, Node::Element(done));
    }
    stack.pop().map(|root| root.children).unwrap_or_default()
}

/// Close the nearest open element named `name`, folding intervening unclosed
/// elements into it (lenient recovery).
fn close_tag(stack: &mut Vec<Element>, name: &str) {
    let idx = stack.iter().rposition(|e| e.tag == name);
    if let Some(idx) = idx {
        if idx == 0 {
            return; // never pop the root
        }
        while stack.len() > idx {
            let done = stack.pop().unwrap();
            push_child(stack, Node::Element(done));
        }
    }
}

fn push_child(stack: &mut [Element], node: Node) {
    if let Some(top) = stack.last_mut() {
        top.children.push(node);
    }
}

fn push_text(stack: &mut [Element], text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(top) = stack.last_mut() {
        top.children.push(Node::Text(text.to_string()));
    }
}

/// Split a start-tag's inner text into `(tag, attrs)`.
fn parse_tag(raw: &str) -> Option<(String, Vec<(String, String)>)> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut chars = raw.char_indices().peekable();
    // Tag name.
    let start = 0;
    let mut name_end = raw.len();
    for (i, c) in raw.char_indices() {
        if c.is_whitespace() || c == '/' {
            name_end = i;
            break;
        }
    }
    let _ = &mut chars;
    let tag = raw[start..name_end].to_ascii_lowercase();
    if tag.is_empty() || !tag.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }

    let mut attrs = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = name_end;
    while i < bytes.len() {
        // Skip whitespace.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Attribute name.
        let nstart = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let aname = raw[nstart..i].to_ascii_lowercase();
        // Skip whitespace before '='.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut aval = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let q = bytes[i];
                i += 1;
                let vstart = i;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                aval = decode_entities(&raw[vstart..i.min(raw.len())]);
                i = (i + 1).min(bytes.len());
            } else {
                let vstart = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                aval = decode_entities(&raw[vstart..i]);
            }
        }
        if !aname.is_empty() {
            attrs.push((aname, aval));
        }
    }
    Some((tag, attrs))
}

/// Decode the common named + numeric character references.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi) = s[i..].find(';').filter(|&j| j <= 32) {
                let ent = &s[i + 1..i + semi];
                let decoded = if let Some(num) = ent.strip_prefix('#') {
                    let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        num.parse::<u32>().ok()
                    };
                    code.and_then(char::from_u32)
                } else {
                    match ent {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        "nbsp" => Some('\u{00A0}'),
                        "copy" => Some('©'),
                        "reg" => Some('®'),
                        "trade" => Some('™'),
                        "mdash" => Some('—'),
                        "ndash" => Some('–'),
                        "hellip" => Some('…'),
                        "laquo" => Some('«'),
                        "raquo" => Some('»'),
                        "eacute" => Some('é'),
                        "egrave" => Some('è'),
                        "agrave" => Some('à'),
                        "ccedil" => Some('ç'),
                        "rsquo" => Some('\u{2019}'),
                        "lsquo" => Some('\u{2018}'),
                        "ldquo" => Some('\u{201C}'),
                        "rdquo" => Some('\u{201D}'),
                        _ => None,
                    }
                };
                if let Some(c) = decoded {
                    out.push(c);
                    i += semi + 1;
                    continue;
                }
            }
            out.push('&');
            i += 1;
        } else {
            // Copy one UTF-8 char.
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&s[i..(i + ch_len).min(s.len())]);
            i += ch_len;
        }
    }
    out
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Case-insensitive substring search.
fn lower_find(haystack: &str, needle_lower: &str) -> Option<usize> {
    let h = haystack.to_ascii_lowercase();
    h.find(needle_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_element(nodes: &[Node]) -> &Element {
        nodes
            .iter()
            .find_map(|n| match n {
                Node::Element(e) => Some(e),
                _ => None,
            })
            .expect("element")
    }

    #[test]
    fn parses_nested_elements_and_attrs() {
        let nodes = parse(r#"<div class="a" id='b'><p>Hello <b>world</b></p></div>"#);
        let div = first_element(&nodes);
        assert_eq!(div.tag, "div");
        assert_eq!(div.attr("class"), Some("a"));
        assert_eq!(div.attr("id"), Some("b"));
        let p = first_element(&div.children);
        assert_eq!(p.tag, "p");
    }

    #[test]
    fn decodes_entities() {
        let nodes = parse("<p>a &amp; b &lt; c &#233;</p>");
        let p = first_element(&nodes);
        let text: String = p
            .children
            .iter()
            .filter_map(|n| match n {
                Node::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "a & b < c é");
    }

    #[test]
    fn implied_paragraph_close() {
        let nodes = parse("<p>one<p>two");
        let paras: Vec<_> = nodes
            .iter()
            .filter(|n| matches!(n, Node::Element(e) if e.tag == "p"))
            .collect();
        assert_eq!(paras.len(), 2, "second <p> implicitly closes the first");
    }

    #[test]
    fn style_is_raw_text() {
        let nodes = parse("<style>p { color: red; }</style><p>x</p>");
        let style = first_element(&nodes);
        assert_eq!(style.tag, "style");
        assert!(matches!(&style.children[0], Node::Text(t) if t.contains("color: red")));
    }

    #[test]
    fn never_panics_on_garbage() {
        let _ = parse("<<<>>> <p class=>< unclosed <b>");
        let _ = parse("");
    }
}
