//! Office Math (OMML, `m:oMath` / `m:oMathPara`) → readable linear Unicode math.
//!
//! Word stores equations as **OMML** (the `m:` namespace). The DOCX body walker
//! ([`super::office_import`]) used to drop the whole `m:oMath` subtree on its
//! `_ => {}` fallthrough, so equation content vanished silently
//! ([issue #37](../../issues/37)). This module recovers that content by
//! **linearizing** the OMML element set to a single readable Unicode-math string
//! — `√(…)`, superscript/subscript digits, `(num)/(den)` fractions, `∑`/`∫`
//! n-aries, delimiters, functions, accents, limits and matrices — so the maths
//! survives into the model/render/export as ordinary text runs.
//!
//! This is **deliberately not** a 2D visual math layout engine: there is no
//! stacked fraction bar, no radical vinculum, no positioned super/subscripts or
//! over/under limits. True visual typesetting needs a maths layout engine and is
//! out of scope here (noted as deferred in `docs/CONVERSIONS.md`). The promise of
//! this module is narrower and complete: **no sub-expression is ever dropped** —
//! every OMML element is lowered (unknown elements recurse into their children so
//! their text is never lost).
//!
//! The DOCX parser ([`super::office_import`]) is a flat streaming tokenizer with
//! no node tree, so it first buffers the `m:oMath` subtree into the small
//! [`OmmlNode`] tree defined here, then calls [`lower_omml`] on it. Keeping
//! [`OmmlNode`] parser-agnostic lets the lowering be a clean recursive
//! tree walk, and keeps this module decoupled from the importer's internals.

use crate::model::{CharStyle, Inline, InlineRun};

/// A parsed OMML element: its **local** tag name (namespace prefix stripped, e.g.
/// `oMath`, `f`, `r`, `t`), its attributes (local name → value) and its ordered
/// children. A run's literal text (`m:t`) is held as a `#text` pseudo-child so the
/// tree is uniform (text nodes carry their content in [`OmmlNode::text`]).
#[derive(Debug, Clone, Default)]
pub struct OmmlNode {
    /// Local tag name (`""` for a synthetic text node).
    pub tag: String,
    /// Attributes by **local** name (e.g. `m:val` is keyed `val`).
    pub attrs: Vec<(String, String)>,
    /// Literal text content (only set on a text node, `tag == ""`).
    pub text: String,
    /// Child elements / text nodes, in document order.
    pub children: Vec<OmmlNode>,
}

impl OmmlNode {
    /// A new element node with the given local tag.
    pub fn element(tag: impl Into<String>) -> Self {
        OmmlNode {
            tag: tag.into(),
            ..OmmlNode::default()
        }
    }

    /// A synthetic text node carrying `text`.
    pub fn text_node(text: impl Into<String>) -> Self {
        OmmlNode {
            text: text.into(),
            ..OmmlNode::default()
        }
    }

    /// `true` for a text node (no tag).
    fn is_text(&self) -> bool {
        self.tag.is_empty()
    }

    /// The first direct child whose local tag is `tag`.
    fn child(&self, tag: &str) -> Option<&OmmlNode> {
        self.children.iter().find(|c| c.tag == tag)
    }

    /// Every direct child whose local tag is `tag`, in order.
    fn children_named<'a>(&'a self, tag: &'a str) -> impl Iterator<Item = &'a OmmlNode> + 'a {
        self.children.iter().filter(move |c| c.tag == tag)
    }

    /// The value of a property attribute, read OMML-style: a property element
    /// `<m:chr m:val="…"/>` nested under `tag` — e.g. `self.prop_val("chr")` on an
    /// `m:naryPr` returns the n-ary operator. Falls back to an attribute named
    /// `tag` directly on this element for robustness.
    fn prop_val(&self, tag: &str) -> Option<&str> {
        if let Some(child) = self.child(tag) {
            if let Some(v) = child.attrs.iter().find(|(k, _)| k == "val") {
                return Some(v.1.as_str());
            }
        }
        self.attrs
            .iter()
            .find(|(k, _)| k == tag)
            .map(|(_, v)| v.as_str())
    }
}

/// Lower an OMML element (`m:oMath`, `m:oMathPara`, or any inner element) to
/// readable linear Unicode-math text runs.
///
/// The full OMML element set is handled — fractions, radicals, sub/superscripts,
/// n-aries, delimiters, functions, accents, bars, limits, matrices and the
/// transparent group wrappers — and **no sub-expression is ever dropped**: an
/// unrecognized element recurses into its children so its text always survives
/// (see the module docs for the precise coverage and the visual-layout
/// deferral).
///
/// The result is a single coalesced [`Inline::Run`] (default char style) per call
/// for a non-empty equation, or an empty `Vec` for empty input.
pub fn lower_omml(node: &OmmlNode) -> Vec<Inline> {
    let s = linearize(node);
    if s.is_empty() {
        return Vec::new();
    }
    vec![Inline::Run(InlineRun {
        text: s,
        style: CharStyle::default(),
        source_index: None,
    })]
}

/// Linearize an OMML element to a Unicode-math string (the recursive core of
/// [`lower_omml`]). Dispatch is on the element's **local** tag.
fn linearize(node: &OmmlNode) -> String {
    if node.is_text() {
        return node.text.clone();
    }
    match node.tag.as_str() {
        // ── Wrappers that contribute their children's text directly ──────────
        // The equation roots, the math run (`m:r` whose `m:t` carries the text),
        // and the structural argument slots all just lower their content.
        "oMath" | "oMathPara" | "r" | "e" | "num" | "den" | "fName" | "lim"
        // Transparent group wrappers: lower their `m:e` content unchanged.
        | "groupChr" | "box" | "borderBox" | "phant" => children_text(node),

        // The literal math text. OMML math-font runs already carry the actual
        // Unicode math-italic/Greek/operator codepoints, so they are preserved
        // verbatim (text nodes return their content).
        "t" => children_text(node),

        // ── Fraction: m:f → (num)/(den) ──────────────────────────────────────
        "f" => {
            let num = slot(node, "num");
            let den = slot(node, "den");
            format!("{}/{}", paren(&num), paren(&den))
        }

        // ── Radical: m:rad → √(e), ∛/∜ for deg 3/4, else (deg)√(e) ───────────
        "rad" => {
            let body = slot(node, "e");
            let deg = slot(node, "deg");
            let deg = deg.trim();
            match deg {
                "" => format!("√({body})"),
                "3" => format!("∛({body})"),
                "4" => format!("∜({body})"),
                _ => format!("({deg})√({body})"),
            }
        }

        // ── Superscript: m:sSup / m:sup → base + Unicode super, else base^(exp)
        // `m:sSup` carries the script in its `m:sup` slot; the abbreviated
        // `m:sup` element carries it in its own `m:e`.
        "sSup" => {
            let base = slot(node, "e");
            let sup = slot(node, "sup");
            format!("{base}{}", superscript(&sup))
        }
        "sup" => {
            let base = slot(node, "e");
            let sup = slot(node, "sup");
            // A bare `m:sup` puts the script in `m:e` (no separate `m:sup` slot).
            let script = if sup.is_empty() { base.clone() } else { sup };
            let base = if script == base { String::new() } else { base };
            format!("{base}{}", superscript(&script))
        }

        // ── Subscript: m:sSub / m:sub → base + Unicode sub, else base_(sub) ───
        "sSub" => {
            let base = slot(node, "e");
            let sub = slot(node, "sub");
            format!("{base}{}", subscript(&sub))
        }
        "sub" => {
            let base = slot(node, "e");
            let sub = slot(node, "sub");
            let script = if sub.is_empty() { base.clone() } else { sub };
            let base = if script == base { String::new() } else { base };
            format!("{base}{}", subscript(&script))
        }

        // ── Sub+super: m:sSubSup → base_(sub)^(sup) (Unicode when mappable) ──
        "sSubSup" => {
            let base = slot(node, "e");
            let sub = slot(node, "sub");
            let sup = slot(node, "sup");
            format!("{base}{}{}", subscript(&sub), superscript(&sup))
        }

        // ── N-ary operator: m:nary → chr_(sub)^(sup) e ───────────────────────
        // e.g. ∑_(i=1)^(n) …, ∫_(a)^(b) …. Default operator ∫ when none given.
        "nary" => {
            let chr = node
                .child("naryPr")
                .and_then(|pr| pr.prop_val("chr"))
                .unwrap_or("∫");
            let sub = slot(node, "sub");
            let sup = slot(node, "sup");
            let body = slot(node, "e");
            let mut out = String::from(chr);
            out.push_str(&subscript(&sub));
            out.push_str(&superscript(&sup));
            if !body.is_empty() {
                out.push(' ');
                out.push_str(&body);
            }
            out
        }

        // ── Delimiters: m:d → begChr … sepChr … endChr (default ()) ──────────
        "d" => {
            let pr = node.child("dPr");
            let beg = pr.and_then(|p| p.prop_val("begChr")).unwrap_or("(");
            let end = pr.and_then(|p| p.prop_val("endChr")).unwrap_or(")");
            let sep = pr.and_then(|p| p.prop_val("sepChr")).unwrap_or("|");
            let parts: Vec<String> = node.children_named("e").map(slot_content).collect();
            format!("{beg}{}{end}", parts.join(sep))
        }

        // ── Function: m:func → fName(e) (sin/cos/log/lim…) ───────────────────
        "func" => {
            let name = slot(node, "fName");
            let body = slot(node, "e");
            format!("{name}({body})")
        }

        // ── Accent: m:acc → base + combining/standalone accent char ──────────
        "acc" => {
            let base = slot(node, "e");
            // Default OMML accent is the combining circumflex (U+0302).
            let chr = node
                .child("accPr")
                .and_then(|pr| pr.prop_val("chr"))
                .unwrap_or("\u{0302}");
            format!("{base}{chr}")
        }

        // ── Bar: m:bar → over/underbar (combining ̄ / ̲, position from m:pos) ─
        "bar" => {
            let base = slot(node, "e");
            let pos = node
                .child("barPr")
                .and_then(|pr| pr.prop_val("pos"))
                .unwrap_or("top");
            // Combining overline (U+0305) vs combining low line (U+0332).
            let combining = if pos == "bot" { '\u{0332}' } else { '\u{0305}' };
            format!("{base}{combining}")
        }

        // ── Lower/upper limit: m:limLow → e_(lim), m:limUpp → e^(lim) ─────────
        "limLow" => {
            let base = slot(node, "e");
            let lim = slot(node, "lim");
            format!("{base}{}", subscript(&lim))
        }
        "limUpp" => {
            let base = slot(node, "e");
            let lim = slot(node, "lim");
            format!("{base}{}", superscript(&lim))
        }

        // ── Matrix: m:m → [r1c1, r1c2; r2c1, r2c2] ───────────────────────────
        "m" => {
            let rows: Vec<String> = node
                .children_named("mr")
                .map(|row| {
                    row.children_named("e")
                        .map(slot_content)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .collect();
            format!("[{}]", rows.join("; "))
        }

        // ── Equation array: m:eqArr → its rows, ; separated ──────────────────
        "eqArr" => node
            .children_named("e")
            .map(slot_content)
            .collect::<Vec<_>>()
            .join("; "),

        // ── Property containers carry no displayable text ────────────────────
        // (`m:naryPr`, `m:dPr`, `m:fPr`, `m:rPr`, `m:ctrlPr`, `m:accPr`, …). They
        // describe formatting; their values are read via `prop_val` by the owner.
        t if t.ends_with("Pr") => String::new(),

        // ── Unknown element: never drop content — recurse into children ──────
        _ => children_text(node),
    }
}

/// Concatenate the linearized text of every child, in order.
fn children_text(node: &OmmlNode) -> String {
    node.children.iter().map(linearize).collect()
}

/// Lower a named **argument slot** of `node` (`m:e`, `m:num`, `m:sub`, `m:sup`,
/// `m:lim`, `m:deg`, `m:fName`, …) to its text: find the slot child and lower its
/// *contents* ([`slot_content`]). Reading a slot this way — rather than
/// re-dispatching the slot element through [`linearize`] — is essential for the
/// slots whose names collide with standalone construct tags (`m:sub`/`m:sup`),
/// which would otherwise be (mis)handled as their own subscript/superscript
/// construct and yield nothing. A missing slot lowers to the empty string.
fn slot(node: &OmmlNode, name: &str) -> String {
    node.child(name).map(slot_content).unwrap_or_default()
}

/// Lower the *contents* of a slot element (its children), bypassing a dispatch on
/// the slot's own tag. A slot holds an arbitrary OMML sub-expression (runs,
/// nested fractions, …), each child lowered in order.
fn slot_content(slot: &OmmlNode) -> String {
    children_text(slot)
}

/// Parenthesize `s` only when it spans more than one token (so a simple `a/b`
/// stays `a/b` but `(x+1)/(y)` is grouped). A part that is already a single
/// bracketed/delimited group is left alone.
fn paren(s: &str) -> String {
    if is_atomic(s) {
        s.to_string()
    } else {
        format!("({s})")
    }
}

/// `true` when `s` is a single math token: a lone (possibly multi-char) run with
/// no operator/space that would need grouping, or an already-bracketed group.
fn is_atomic(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return true;
    }
    // Already wrapped in matching brackets spanning the whole string.
    if is_wrapped(s) {
        return true;
    }
    // No structural operator/space ⇒ a single token (e.g. `x`, `42`, `αβ`).
    !s.chars()
        .any(|c| c.is_whitespace() || matches!(c, '+' | '-' | '*' | '/' | '=' | '±' | '∓' | '·'))
}

/// `true` if `s` begins with an opening bracket whose matching close is the final
/// character (so the whole string is one balanced group, e.g. `(x+1)` / `[a; b]`).
fn is_wrapped(s: &str) -> bool {
    let bytes = s.as_bytes();
    let (open, close) = match bytes.first() {
        Some(b'(') => (b'(', b')'),
        Some(b'[') => (b'[', b']'),
        Some(b'{') => (b'{', b'}'),
        _ => return false,
    };
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate() {
        if b == open {
            depth += 1;
        } else if b == close {
            depth -= 1;
            if depth == 0 {
                return i == bytes.len() - 1;
            }
        }
    }
    false
}

/// Render `s` as Unicode superscript when **every** character maps; otherwise
/// fall back to the linear `^(s)` form. An empty script yields the empty string.
fn superscript(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    match map_all(s, sup_char) {
        Some(mapped) => mapped,
        None => format!("^({s})"),
    }
}

/// Render `s` as Unicode subscript when every character maps; otherwise `_(s)`.
fn subscript(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    match map_all(s, sub_char) {
        Some(mapped) => mapped,
        None => format!("_({s})"),
    }
}

/// Map every char of `s` through `f`; `Some(mapped)` only if **all** chars map,
/// else `None` (so the caller can fall back to a linear `^(…)`/`_(…)` form).
fn map_all(s: &str, f: fn(char) -> Option<char>) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        out.push(f(c)?);
    }
    Some(out)
}

/// Unicode superscript form of `c` (digits, sign/relation/parens, and the common
/// letters), or `None` when there is no superscript codepoint.
fn sup_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' | '\u{2212}' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        ' ' => ' ',
        _ => return None,
    })
}

/// Unicode subscript form of `c` (digits, sign/relation/parens, and the common
/// lowercase letters that have subscript codepoints), or `None`.
fn sub_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' | '\u{2212}' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        ' ' => ' ',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `<tag>children…</tag>`.
    fn el(tag: &str, children: Vec<OmmlNode>) -> OmmlNode {
        OmmlNode {
            tag: tag.to_string(),
            children,
            ..OmmlNode::default()
        }
    }

    /// A math run `m:r` carrying literal text `m:t`.
    fn run(text: &str) -> OmmlNode {
        el("r", vec![el("t", vec![OmmlNode::text_node(text)])])
    }

    /// A wrapper element `m:e`/`m:num`/… holding one run of text.
    fn slot(tag: &str, text: &str) -> OmmlNode {
        el(tag, vec![run(text)])
    }

    fn lower_text(node: &OmmlNode) -> String {
        match lower_omml(node).into_iter().next() {
            Some(Inline::Run(r)) => r.text,
            _ => String::new(),
        }
    }

    #[test]
    fn run_text_preserved() {
        assert_eq!(lower_text(&run("x+1")), "x+1");
    }

    #[test]
    fn fraction_single_tokens_stays_simple() {
        let f = el("f", vec![slot("num", "a"), slot("den", "b")]);
        assert_eq!(lower_text(&f), "a/b");
    }

    #[test]
    fn fraction_multi_token_parts_parenthesized() {
        let f = el("f", vec![slot("num", "x+1"), slot("den", "y")]);
        assert_eq!(lower_text(&f), "(x+1)/y");
    }

    #[test]
    fn radical_plain_degree_three_and_four() {
        assert_eq!(lower_text(&el("rad", vec![slot("e", "2")])), "√(2)");
        assert_eq!(
            lower_text(&el("rad", vec![slot("deg", "3"), slot("e", "x")])),
            "∛(x)"
        );
        assert_eq!(
            lower_text(&el("rad", vec![slot("deg", "4"), slot("e", "x")])),
            "∜(x)"
        );
        assert_eq!(
            lower_text(&el("rad", vec![slot("deg", "5"), slot("e", "x")])),
            "(5)√(x)"
        );
    }

    #[test]
    fn superscript_unicode_then_fallback() {
        let sq = el("sSup", vec![slot("e", "x"), slot("sup", "2")]);
        assert_eq!(lower_text(&sq), "x²");
        // A non-mappable exponent falls back to the linear caret form.
        let fb = el("sSup", vec![slot("e", "x"), slot("sup", "α")]);
        assert_eq!(lower_text(&fb), "x^(α)");
    }

    #[test]
    fn subscript_unicode_then_fallback() {
        let s = el("sSub", vec![slot("e", "x"), slot("sub", "1")]);
        assert_eq!(lower_text(&s), "x₁");
        let fb = el("sSub", vec![slot("e", "a"), slot("sub", "β")]);
        assert_eq!(lower_text(&fb), "a_(β)");
    }

    #[test]
    fn subsup_combined() {
        let x = el(
            "sSubSup",
            vec![slot("e", "x"), slot("sub", "i"), slot("sup", "2")],
        );
        assert_eq!(lower_text(&x), "xᵢ²");
    }

    #[test]
    fn nary_sum_with_limits() {
        // ∑_(i=1)^(n) with a default-less operator from m:naryPr/m:chr.
        let pr = OmmlNode {
            tag: "naryPr".to_string(),
            children: vec![OmmlNode {
                tag: "chr".to_string(),
                attrs: vec![("val".to_string(), "∑".to_string())],
                ..OmmlNode::default()
            }],
            ..OmmlNode::default()
        };
        let sum = el(
            "nary",
            vec![pr, slot("sub", "i=1"), slot("sup", "n"), slot("e", "k")],
        );
        // "i=1" maps fully to subscripts; "n" to a superscript.
        assert_eq!(lower_text(&sum), "∑ᵢ₌₁ⁿ k");
    }

    #[test]
    fn nary_default_operator_is_integral() {
        let int = el(
            "nary",
            vec![slot("sub", "a"), slot("sup", "b"), slot("e", "x")],
        );
        assert_eq!(lower_text(&int), "∫ₐᵇ x");
    }

    #[test]
    fn delimiters_default_and_custom() {
        let d = el("d", vec![slot("e", "x")]);
        assert_eq!(lower_text(&d), "(x)");
        // Custom begin/end/sep with two args.
        let pr = OmmlNode {
            tag: "dPr".to_string(),
            children: vec![
                OmmlNode {
                    tag: "begChr".to_string(),
                    attrs: vec![("val".to_string(), "[".to_string())],
                    ..OmmlNode::default()
                },
                OmmlNode {
                    tag: "endChr".to_string(),
                    attrs: vec![("val".to_string(), "]".to_string())],
                    ..OmmlNode::default()
                },
            ],
            ..OmmlNode::default()
        };
        let d2 = el("d", vec![pr, slot("e", "a"), slot("e", "b")]);
        assert_eq!(lower_text(&d2), "[a|b]");
    }

    #[test]
    fn function_name_wraps_argument() {
        let f = el("func", vec![slot("fName", "sin"), slot("e", "x")]);
        assert_eq!(lower_text(&f), "sin(x)");
    }

    #[test]
    fn accent_and_bar() {
        // Default accent = combining circumflex.
        let acc = el("acc", vec![slot("e", "x")]);
        assert_eq!(lower_text(&acc), "x\u{0302}");
        // Overbar by default.
        let bar = el("bar", vec![slot("e", "x")]);
        assert_eq!(lower_text(&bar), "x\u{0305}");
    }

    #[test]
    fn limits_lower_and_upper() {
        let low = el("limLow", vec![slot("e", "lim"), slot("lim", "0")]);
        assert_eq!(lower_text(&low), "lim₀");
        let upp = el("limUpp", vec![slot("e", "x"), slot("lim", "n")]);
        assert_eq!(lower_text(&upp), "xⁿ");
    }

    #[test]
    fn matrix_rows_and_cells() {
        let row1 = el("mr", vec![slot("e", "a"), slot("e", "b")]);
        let row2 = el("mr", vec![slot("e", "c"), slot("e", "d")]);
        let m = el("m", vec![row1, row2]);
        assert_eq!(lower_text(&m), "[a, b; c, d]");
    }

    #[test]
    fn unknown_element_recurses_into_children() {
        // A made-up wrapper must still surface its inner run text.
        let weird = el("totallyUnknown", vec![run("kept")]);
        assert_eq!(lower_text(&weird), "kept");
    }

    #[test]
    fn empty_equation_yields_no_runs() {
        assert!(lower_omml(&el("oMath", vec![])).is_empty());
    }

    #[test]
    fn group_wrappers_are_transparent() {
        let g = el("groupChr", vec![run("y")]);
        assert_eq!(lower_text(&g), "y");
        let b = el("box", vec![run("z")]);
        assert_eq!(lower_text(&b), "z");
    }

    #[test]
    fn nested_fraction_with_superscript_and_radical() {
        // x²/√(y) — a fraction whose numerator is x² and denominator √(y). Both
        // parts are single atomic tokens (no bare operator), so neither is
        // re-parenthesized: the linear form stays readable.
        let sq = el("sSup", vec![slot("e", "x"), slot("sup", "2")]);
        let rad = el("rad", vec![slot("e", "y")]);
        let num = el("num", vec![sq]);
        let den = el("den", vec![rad]);
        let f = el("f", vec![num, den]);
        assert_eq!(lower_text(&f), "x²/√(y)");
    }

    #[test]
    fn fraction_parenthesizes_when_a_part_has_an_operator() {
        // A numerator carrying a binary operator is grouped; a single-token
        // denominator is not.
        let num = el("num", vec![run("a+b")]);
        let den = el("den", vec![run("c")]);
        let f = el("f", vec![num, den]);
        assert_eq!(lower_text(&f), "(a+b)/c");
    }

    // ── sup/sub char tables (exhaustive) ─────────────────────────────────────

    #[test]
    fn superscript_maps_full_table() {
        // Digits, sign/relation/parens, and every supported letter map to Unicode.
        assert_eq!(superscript("0123456789"), "⁰¹²³⁴⁵⁶⁷⁸⁹");
        assert_eq!(superscript("+-=()"), "⁺⁻⁼⁽⁾");
        assert_eq!(superscript("\u{2212}"), "⁻"); // minus sign maps too
        assert_eq!(
            superscript("niabcdefghjklmoprstuvwxyz"),
            "ⁿⁱᵃᵇᶜᵈᵉᶠᵍʰʲᵏˡᵐᵒᵖʳˢᵗᵘᵛʷˣʸᶻ"
        );
        assert_eq!(superscript(" "), " ");
        // Any non-mappable char ⇒ whole string falls back to linear form.
        assert_eq!(superscript("q"), "^(q)"); // 'q' has no superscript codepoint
        assert_eq!(superscript(""), "");
    }

    #[test]
    fn subscript_maps_full_table() {
        assert_eq!(subscript("0123456789"), "₀₁₂₃₄₅₆₇₈₉");
        assert_eq!(subscript("+-=()"), "₊₋₌₍₎");
        assert_eq!(subscript("\u{2212}"), "₋");
        assert_eq!(subscript("aehijklmnoprstuvx"), "ₐₑₕᵢⱼₖₗₘₙₒₚᵣₛₜᵤᵥₓ");
        assert_eq!(subscript(" "), " ");
        assert_eq!(subscript("b"), "_(b)"); // 'b' has no subscript codepoint
        assert_eq!(subscript(""), "");
    }

    // ── is_atomic / is_wrapped ───────────────────────────────────────────────

    #[test]
    fn radical_body_grouping_via_is_atomic() {
        // A single token is left bare; an operator-bearing body is grouped.
        // (paren() is exercised through `f`, atomic-ness through these shapes.)
        let f_atomic = el("f", vec![slot("num", "x"), slot("den", "y")]);
        assert_eq!(lower_text(&f_atomic), "x/y");
        // Already-bracketed group is treated as atomic (no double-wrap).
        let f_wrapped = el("f", vec![slot("num", "(a+b)"), slot("den", "c")]);
        assert_eq!(lower_text(&f_wrapped), "(a+b)/c");
        // A bracket group that does NOT span the whole string is not atomic.
        let f_partial = el("f", vec![slot("num", "(a+b)+c"), slot("den", "d")]);
        assert_eq!(lower_text(&f_partial), "((a+b)+c)/d");
        // Square and curly brackets also count as wrapped.
        let f_sq = el("f", vec![slot("num", "[a; b]"), slot("den", "c")]);
        assert_eq!(lower_text(&f_sq), "[a; b]/c");
    }

    // ── bare sup / sub (script in m:e) ───────────────────────────────────────

    #[test]
    fn bare_sup_and_sub_put_script_in_e() {
        // `m:sup` with only an `m:e` slot: the e content is the script itself.
        let sup = el("sup", vec![slot("e", "2")]);
        assert_eq!(lower_text(&sup), "²");
        let sub = el("sub", vec![slot("e", "0")]);
        assert_eq!(lower_text(&sub), "₀");
        // With both base (e) and a sup slot, base precedes the script.
        let sup2 = el("sup", vec![slot("e", "x"), slot("sup", "2")]);
        assert_eq!(lower_text(&sup2), "x²");
    }

    // ── equation array ───────────────────────────────────────────────────────

    #[test]
    fn eq_array_joins_rows_with_semicolons() {
        let arr = el(
            "eqArr",
            vec![slot("e", "x=1"), slot("e", "y=2"), slot("e", "z=3")],
        );
        assert_eq!(lower_text(&arr), "x=1; y=2; z=3");
    }
}
