//! Inline-`<script>` execution for the HTML→PDF renderer, on the **Boa** engine.
//!
//! [`run_inline_scripts`] parses a document, runs its inline scripts against a
//! live DOM, and re-serialises the mutated tree back to HTML — so the renderer
//! needs no headless browser even for script-driven pages.
//!
//! ## Design — a JavaScript DOM polyfill, not Rust bindings
//!
//! Boa is a JS interpreter, so the DOM lives where it is most natural: in
//! JavaScript. The Rust side only:
//! 1. parses the HTML with [`crate::html::dom::parse`] (the same tokenizer the
//!    renderer uses) into a node forest;
//! 2. encodes that forest as JSON — which is valid JS literal syntax, so it is
//!    injected directly (no `JSON.parse`);
//! 3. runs each inline script in its own Boa `eval` (shared global, per-script
//!    error isolation, exactly like a browser running successive `<script>`s);
//! 4. asks the polyfill to serialise the tree back to HTML.
//!
//! The Rust↔Boa boundary is therefore two strings (JSON in, HTML out) with no
//! `NativeFunction` glue. The serialiser in [`POLYFILL`] mirrors the engine's
//! own (`escape_text`/`escape_attr`/kebab-cased inline style, `<script>`
//! stripped, void elements un-closed) so output is byte-compatible.

use crate::html::dom::Node;
use boa_engine::{Context, Source};

/// Execute the inline `<script>`s in `html` and return the resulting HTML.
///
/// If there is no `<script>`, the input is returned unchanged (zero cost). A
/// script that throws never aborts rendering: the DOM mutated up to the throw
/// is kept and later scripts still run. Any internal failure falls back to the
/// original HTML, so this function never loses the document.
pub fn run_inline_scripts(html: &str) -> String {
    if !html.to_ascii_lowercase().contains("<script") {
        return html.to_string();
    }
    let nodes = crate::html::dom::parse(html);

    // 1. Encode the forest as a JS literal (JSON is a JS-expression subset).
    let mut json = String::from("[");
    for (i, n) in nodes.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        node_to_json(n, &mut json);
    }
    json.push(']');

    // 2. Gather inline scripts (no `src`) in document order.
    let mut scripts = Vec::new();
    collect_scripts(&nodes, &mut scripts);

    // 3. Build ONE program so every DOM mutation and the final serialisation run
    //    in a single `eval`. Boa's GC reclaims DOM nodes that are only reachable
    //    through the `document` object between *separate* evals, so the tree must
    //    never cross an eval boundary. Per-script isolation is preserved without
    //    extra evals: each script is wrapped in `try`/`catch` (a throw must not
    //    abort later scripts) and syntactically pre-validated (one malformed
    //    script must not break the whole program, which is parsed as a unit).
    let mut prog = String::with_capacity(POLYFILL.len() + json.len() + 256);
    prog.push_str(POLYFILL);
    prog.push_str("\nglobalThis.document = __gpInit(");
    prog.push_str(&json);
    prog.push_str(");\nglobalThis.window = globalThis;\n");
    {
        let mut vctx = Context::default();
        for src in &scripts {
            if is_valid_js(&mut vctx, src) {
                prog.push_str("try {\n");
                prog.push_str(src);
                prog.push_str("\n} catch (e) {}\n");
            }
        }
    }
    prog.push_str("__gpSerialize()\n");

    let mut ctx = Context::default();
    match ctx.eval(Source::from_bytes(prog.as_str())) {
        Ok(v) => v
            .to_string(&mut ctx)
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_else(|_| html.to_string()),
        Err(_) => html.to_string(), // polyfill/serialiser bug → never lose the document
    }
}

/// Is `src` syntactically valid JavaScript? Compiles it with `new Function`
/// (which parses but does not execute) inside a throwaway context. Used to drop
/// malformed scripts before they are concatenated into the single program.
fn is_valid_js(ctx: &mut Context, src: &str) -> bool {
    let mut lit = String::new();
    json_str(src, &mut lit);
    let probe =
        format!("(function(){{ try {{ new Function({lit}); return true; }} catch (e) {{ return false; }} }})()");
    matches!(ctx.eval(Source::from_bytes(probe.as_str())), Ok(v) if v.as_boolean() == Some(true))
}

/// Append a JSON-escaped string literal (`"…"`) to `out`.
fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // U+2028/2029 are legal in ES2019+ string literals, but escape the
            // remaining C0 controls so the literal always parses.
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Encode one node as `{"x":text}` (text) or `{"t":tag,"a":[[k,v]…],"c":[…]}`.
fn node_to_json(node: &Node, out: &mut String) {
    match node {
        Node::Text(t) => {
            out.push_str("{\"x\":");
            json_str(t, out);
            out.push('}');
        }
        Node::Element(el) => {
            out.push_str("{\"t\":");
            json_str(&el.tag, out);
            out.push_str(",\"a\":[");
            for (i, (k, v)) in el.attrs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('[');
                json_str(k, out);
                out.push(',');
                json_str(v, out);
                out.push(']');
            }
            out.push_str("],\"c\":[");
            for (i, ch) in el.children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                node_to_json(ch, out);
            }
            out.push_str("]}");
        }
    }
}

/// Collect the text of every inline `<script>` (no `src`) in document order.
fn collect_scripts(nodes: &[Node], out: &mut Vec<String>) {
    for n in nodes {
        let Node::Element(el) = n else { continue };
        if el.tag == "script" {
            if el.attr("src").is_none() {
                let mut t = String::new();
                for c in &el.children {
                    if let Node::Text(s) = c {
                        t.push_str(s);
                    }
                }
                out.push(t);
            }
            continue;
        }
        collect_scripts(&el.children, out);
    }
}

/// The DOM polyfill, in plain JavaScript. Defines `__gpInit(forest)` (builds the
/// document and installs `__gpSerialize`) plus the element/document API the
/// renderer's scripts use. Kept faithful to [`crate::js::dom`]'s serialiser.
const POLYFILL: &str = r##"
(function () {
  var VOID = {area:1,base:1,br:1,col:1,embed:1,hr:1,img:1,input:1,link:1,meta:1,param:1,source:1,track:1,wbr:1};

  function El(tag) { this.nodeType = 1; this.tagName = String(tag).toLowerCase(); this.childNodes = []; this._a = {}; this._s = {}; this.parentNode = null; }
  function Txt(t) { this.nodeType = 3; this._t = String(t); this.parentNode = null; }

  function kebab(name) {
    var o = "";
    for (var i = 0; i < name.length; i++) { var c = name[i]; if (c >= "A" && c <= "Z") { o += "-" + c.toLowerCase(); } else { o += c; } }
    return o;
  }
  function escapeText(s) { return String(s).split("&").join("&amp;").split("<").join("&lt;").split(">").join("&gt;"); }
  function escapeAttr(s) { return String(s).split("&").join("&amp;").split('"').join("&quot;"); }

  function styleString(el) {
    var parts = [];
    for (var k in el._s) { if (!el._s.hasOwnProperty(k)) continue; var v = el._s[k]; if (v !== "" && v != null) parts.push(kebab(k) + ": " + v); }
    return parts.join("; ");
  }

  function collectText(node, acc) {
    if (node.nodeType === 3) { acc.push(node._t); return; }
    for (var i = 0; i < node.childNodes.length; i++) collectText(node.childNodes[i], acc);
  }

  function serialize(node) {
    if (node.nodeType === 3) return escapeText(node._t);
    if (node.nodeType !== 1) return "";
    var tag = node.tagName;
    if (tag === "script") return "";
    var pairs = [];
    for (var k in node._a) { if (node._a.hasOwnProperty(k)) pairs.push([k, String(node._a[k])]); }
    var style = styleString(node);
    if (style !== "") {
      var found = false;
      for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === "style") { pairs[i][1] = style; found = true; break; } }
      if (!found) pairs.push(["style", style]);
    }
    var out = "<" + tag;
    for (var i = 0; i < pairs.length; i++) out += " " + pairs[i][0] + '="' + escapeAttr(pairs[i][1]) + '"';
    out += ">";
    if (VOID[tag]) return out;
    for (var i = 0; i < node.childNodes.length; i++) out += serialize(node.childNodes[i]);
    return out + "</" + tag + ">";
  }

  // ---- a minimal HTML parser, used by innerHTML= and __gpInit -----------------
  function parseHTML(str) {
    var nodes = [], stack = [], i = 0, n = str.length;
    function host() { return stack.length ? stack[stack.length - 1].childNodes : nodes; }
    function attach(node) { node.parentNode = stack.length ? stack[stack.length - 1] : null; host().push(node); }
    var attrRe = /([\w:.-]+)(\s*=\s*("([^"]*)"|'([^']*)'|([^\s>]+)))?/g;
    while (i < n) {
      if (str[i] === "<") {
        if (str[i + 1] === "/") {
          var j = str.indexOf(">", i); if (j < 0) break;
          var tag = str.slice(i + 2, j).trim().toLowerCase();
          for (var k = stack.length - 1; k >= 0; k--) { if (stack[k].tagName === tag) { stack.length = k; break; } }
          i = j + 1; continue;
        }
        var j = str.indexOf(">", i); if (j < 0) break;
        var raw = str.slice(i + 1, j).trim(); var self = false;
        if (raw[raw.length - 1] === "/") { self = true; raw = raw.slice(0, -1).trim(); }
        var sp = raw.search(/\s/), tname, attrs;
        if (sp < 0) { tname = raw.toLowerCase(); attrs = ""; } else { tname = raw.slice(0, sp).toLowerCase(); attrs = raw.slice(sp + 1); }
        var e = new El(tname);
        attrRe.lastIndex = 0; var m;
        while ((m = attrRe.exec(attrs))) {
          if (!m[1]) continue;
          var val = m[4] !== undefined ? m[4] : m[5] !== undefined ? m[5] : m[6] !== undefined ? m[6] : "";
          setAttrRaw(e, m[1], val);
        }
        attach(e);
        if (!self && !VOID[tname]) stack.push(e);
        i = j + 1; continue;
      }
      var j = str.indexOf("<", i); if (j < 0) j = n;
      var text = str.slice(i, j);
      if (text.length) attach(new Txt(decodeEntities(text)));
      i = j;
    }
    return nodes;
  }
  function decodeEntities(s) { return s.split("&lt;").join("<").split("&gt;").join(">").split("&quot;").join('"').split("&#39;").join("'").split("&amp;").join("&"); }

  // setAttribute, special-casing style="" so it round-trips through _s.
  function setAttrRaw(el, name, val) {
    if (name === "style") {
      var decls = String(val).split(";");
      for (var i = 0; i < decls.length; i++) { var c = decls[i].indexOf(":"); if (c > 0) el._s[decls[i].slice(0, c).trim()] = decls[i].slice(c + 1).trim(); }
      return;
    }
    el._a[name] = String(val);
  }

  // ---- build the live tree from the Rust-encoded forest ----------------------
  function build(j) {
    if (j.x !== undefined) return new Txt(j.x);
    var e = new El(j.t);
    var a = j.a || [];
    for (var i = 0; i < a.length; i++) setAttrRaw(e, a[i][0], a[i][1]);
    var c = j.c || [];
    for (var i = 0; i < c.length; i++) { var ch = build(c[i]); ch.parentNode = e; e.childNodes.push(ch); }
    return e;
  }

  // ---- element API -----------------------------------------------------------
  Object.defineProperty(El.prototype, "textContent", {
    get: function () { var acc = []; collectText(this, acc); return acc.join(""); },
    set: function (v) { var t = new Txt(v); t.parentNode = this; this.childNodes = [t]; }
  });
  Object.defineProperty(El.prototype, "innerHTML", {
    get: function () { var o = ""; for (var i = 0; i < this.childNodes.length; i++) o += serialize(this.childNodes[i]); return o; },
    set: function (v) { var kids = parseHTML(String(v)); for (var i = 0; i < kids.length; i++) kids[i].parentNode = this; this.childNodes = kids; }
  });
  Object.defineProperty(El.prototype, "id", { get: function () { return this._a.id != null ? String(this._a.id) : ""; }, set: function (v) { this._a.id = String(v); } });
  Object.defineProperty(El.prototype, "className", { get: function () { return this._a["class"] != null ? String(this._a["class"]) : ""; }, set: function (v) { this._a["class"] = String(v); } });
  Object.defineProperty(El.prototype, "style", { get: function () { return this._s; } });
  Object.defineProperty(El.prototype, "children", { get: function () { var r = []; for (var i = 0; i < this.childNodes.length; i++) if (this.childNodes[i].nodeType === 1) r.push(this.childNodes[i]); return r; } });
  Object.defineProperty(El.prototype, "firstChild", { get: function () { return this.childNodes[0] || null; } });

  El.prototype.getAttribute = function (n) { return this._a[n] != null ? String(this._a[n]) : null; };
  El.prototype.setAttribute = function (n, v) { setAttrRaw(this, n, v); };
  El.prototype.hasAttribute = function (n) { return this._a[n] != null; };
  El.prototype.removeAttribute = function (n) { delete this._a[n]; };
  El.prototype.appendChild = function (c) { if (c.parentNode) c.parentNode.removeChild(c); c.parentNode = this; this.childNodes.push(c); return c; };
  El.prototype.removeChild = function (c) { var idx = this.childNodes.indexOf(c); if (idx >= 0) { this.childNodes.splice(idx, 1); c.parentNode = null; } return c; };
  El.prototype.getElementsByTagName = function (t) { var r = []; collectByTag(this.childNodes, String(t).toLowerCase(), r); return r; };
  El.prototype.querySelector = function (s) { var c = []; allElements(this.childNodes, c); return firstMatch(c, s); };
  El.prototype.querySelectorAll = function (s) { var c = []; allElements(this.childNodes, c); return allMatches(c, s); };

  var classList = {
    add: function () { var t = tokens(this._el); for (var i = 0; i < arguments.length; i++) if (t.indexOf(arguments[i]) < 0) t.push(arguments[i]); this._el._a["class"] = t.join(" "); },
    remove: function () { var t = tokens(this._el); for (var i = 0; i < arguments.length; i++) { var k = t.indexOf(arguments[i]); if (k >= 0) t.splice(k, 1); } this._el._a["class"] = t.join(" "); },
    contains: function (c) { return tokens(this._el).indexOf(c) >= 0; },
    toggle: function (c) { if (this.contains(c)) { this.remove(c); return false; } this.add(c); return true; }
  };
  function tokens(el) { var s = el._a["class"]; if (!s) return []; return String(s).split(/\s+/).filter(function (x) { return x.length; }); }
  Object.defineProperty(El.prototype, "classList", { get: function () { var o = Object.create(classList); o._el = this; return o; } });

  // ---- tree walking ----------------------------------------------------------
  function allElements(list, out) { for (var i = 0; i < list.length; i++) { var n = list[i]; if (n.nodeType === 1) { out.push(n); allElements(n.childNodes, out); } } }
  function collectByTag(list, tag, out) { for (var i = 0; i < list.length; i++) { var n = list[i]; if (n.nodeType === 1) { if (tag === "*" || n.tagName === tag) out.push(n); collectByTag(n.childNodes, tag, out); } } }
  function findTag(list, tag) { for (var i = 0; i < list.length; i++) { var n = list[i]; if (n.nodeType === 1) { if (n.tagName === tag) return n; var d = findTag(n.childNodes, tag); if (d) return d; } } return null; }

  // ---- CSS selector engine ---------------------------------------------------
  function parseCompound(sel, st) {
    var c = { tag: null, id: null, classes: [], attrs: [] }, any = false, n = sel.length;
    while (st.i < n) {
      var ch = sel[st.i];
      if (ch === "#") { st.i++; var s = st.i; while (st.i < n && /[\w-]/.test(sel[st.i])) st.i++; c.id = sel.slice(s, st.i); any = true; }
      else if (ch === ".") { st.i++; var s = st.i; while (st.i < n && /[\w-]/.test(sel[st.i])) st.i++; c.classes.push(sel.slice(s, st.i)); any = true; }
      else if (ch === "[") { st.i++; var s = st.i; while (st.i < n && sel[st.i] !== "]") st.i++; var body = sel.slice(s, st.i); st.i++;
        var eq = body.indexOf("="); if (eq < 0) c.attrs.push({ name: body.trim(), val: null });
        else { var nm = body.slice(0, eq).trim(); var vv = body.slice(eq + 1).trim(); if ((vv[0] === '"' && vv[vv.length - 1] === '"') || (vv[0] === "'" && vv[vv.length - 1] === "'")) vv = vv.slice(1, -1); c.attrs.push({ name: nm, val: vv }); }
        any = true; }
      else if (/[\w*-]/.test(ch)) { var s = st.i; while (st.i < n && /[\w*-]/.test(sel[st.i])) st.i++; c.tag = sel.slice(s, st.i).toLowerCase(); any = true; }
      else break;
    }
    return any ? c : null;
  }
  function parseSelector(sel) {
    sel = sel.trim(); var st = { i: 0 }, n = sel.length;
    var first = parseCompound(sel, st); if (!first) return null;
    var compounds = [first], combinators = [];
    while (st.i < n) {
      var comb = " ", sawWs = false;
      while (st.i < n && /\s/.test(sel[st.i])) { st.i++; sawWs = true; }
      if (st.i < n && (sel[st.i] === ">" || sel[st.i] === "+" || sel[st.i] === "~")) { comb = sel[st.i]; st.i++; while (st.i < n && /\s/.test(sel[st.i])) st.i++; }
      else if (!sawWs) break;
      var comp = parseCompound(sel, st); if (!comp) break;
      combinators.push(comb); compounds.push(comp);
    }
    return { compounds: compounds, combinators: combinators };
  }
  function matchCompound(el, c) {
    if (!el || el.nodeType !== 1) return false;
    if (c.tag && c.tag !== "*" && el.tagName !== c.tag) return false;
    if (c.id != null && el._a.id !== c.id) return false;
    if (c.classes.length) { var t = tokens(el); for (var i = 0; i < c.classes.length; i++) if (t.indexOf(c.classes[i]) < 0) return false; }
    for (var i = 0; i < c.attrs.length; i++) { var a = c.attrs[i]; if (el._a[a.name] == null) return false; if (a.val != null && String(el._a[a.name]) !== a.val) return false; }
    return true;
  }
  function prevElSib(el) { var p = el.parentNode; if (!p) return null; var cs = p.childNodes, idx = cs.indexOf(el); for (var k = idx - 1; k >= 0; k--) if (cs[k].nodeType === 1) return cs[k]; return null; }
  function matchesSel(el, parsed) {
    var cs = parsed.compounds, comb = parsed.combinators, i = cs.length - 1;
    if (!matchCompound(el, cs[i])) return false;
    var cur = el;
    for (i = i - 1; i >= 0; i--) {
      var k = comb[i], target = cs[i];
      if (k === ">") { cur = cur.parentNode; if (!matchCompound(cur, target)) return false; }
      else if (k === " ") { cur = cur ? cur.parentNode : null; var ok = false; while (cur) { if (matchCompound(cur, target)) { ok = true; break; } cur = cur.parentNode; } if (!ok) return false; }
      else if (k === "+") { var p = prevElSib(cur); if (!matchCompound(p, target)) return false; cur = p; }
      else if (k === "~") { var p = prevElSib(cur), ok = false; while (p) { if (matchCompound(p, target)) { ok = true; break; } p = prevElSib(p); } if (!ok) return false; cur = p; }
    }
    return true;
  }
  function anyMatch(el, sel) { var groups = String(sel).split(","); for (var g = 0; g < groups.length; g++) { var parsed = parseSelector(groups[g]); if (parsed && matchesSel(el, parsed)) return true; } return false; }
  function firstMatch(cands, sel) { for (var i = 0; i < cands.length; i++) if (anyMatch(cands[i], sel)) return cands[i]; return null; }
  function allMatches(cands, sel) { var r = []; for (var i = 0; i < cands.length; i++) if (anyMatch(cands[i], sel)) r.push(cands[i]); return r; }

  // ---- document --------------------------------------------------------------
  globalThis.__gpInit = function (forest) {
    var roots = [];
    for (var i = 0; i < forest.length; i++) roots.push(build(forest[i]));
    function findById(list, id) { for (var i = 0; i < list.length; i++) { var n = list[i]; if (n.nodeType === 1) { if (n._a.id === id) return n; var d = findById(n.childNodes, id); if (d) return d; } } return null; }
    var doc = {
      nodeType: 9,
      getElementById: function (id) { return findById(roots, String(id)); },
      getElementsByTagName: function (t) { var r = []; collectByTag(roots, String(t).toLowerCase(), r); return r; },
      createElement: function (t) { return new El(t); },
      createTextNode: function (t) { return new Txt(t); },
      querySelector: function (s) { var c = []; allElements(roots, c); return firstMatch(c, s); },
      querySelectorAll: function (s) { var c = []; allElements(roots, c); return allMatches(c, s); }
    };
    Object.defineProperty(doc, "body", { get: function () { return findTag(roots, "body"); } });
    Object.defineProperty(doc, "documentElement", { get: function () { return findTag(roots, "html") || (roots[0] && roots[0].nodeType === 1 ? roots[0] : null); } });
    Object.defineProperty(doc, "title", {
      get: function () { var t = findTag(roots, "title"); return t ? t.textContent : ""; },
      set: function (v) { var t = findTag(roots, "title"); if (t) t.textContent = v; }
    });
    globalThis.__gpSerialize = function () { var o = ""; for (var i = 0; i < roots.length; i++) o += serialize(roots[i]); return o; };
    return doc;
  };
})();
"##;

#[cfg(test)]
mod tests {
    use super::run_inline_scripts;

    #[test]
    fn no_script_is_unchanged() {
        let html = "<p>hello</p>";
        assert_eq!(run_inline_scripts(html), html);
    }

    #[test]
    fn set_text_content() {
        let html = "<body><span id=\"x\">old</span><script>document.getElementById('x').textContent = 'new';</script></body>";
        let out = run_inline_scripts(html);
        assert!(out.contains(">new<"), "got: {out}");
        assert!(!out.contains("old"), "got: {out}");
        assert!(!out.contains("<script"), "script should be stripped: {out}");
    }

    #[test]
    fn create_and_append() {
        let html = "<body><ul id=\"list\"></ul><script>\
            var ul = document.getElementById('list');\
            for (var i = 1; i <= 3; i++) { var li = document.createElement('li'); li.textContent = 'item ' + i; ul.appendChild(li); }\
            </script></body>";
        let out = run_inline_scripts(html);
        assert_eq!(out.matches("<li>").count(), 3, "got: {out}");
        assert!(out.contains("item 2"), "got: {out}");
    }

    #[test]
    fn inner_html_and_query() {
        let html = "<body><div class=\"box\"></div><script>\
            document.querySelector('.box').innerHTML = '<b>bold</b><i>it</i>';\
            </script></body>";
        let out = run_inline_scripts(html);
        assert!(out.contains("<b>bold</b>"), "got: {out}");
        assert!(out.contains("<i>it</i>"), "got: {out}");
    }

    #[test]
    fn set_attribute_and_style() {
        let html = "<body><a id=\"k\">x</a><script>\
            var a = document.getElementById('k');\
            a.setAttribute('href', '/go'); a.style.color = 'red'; a.className = 'on';\
            </script></body>";
        let out = run_inline_scripts(html);
        assert!(out.contains("href=\"/go\""), "got: {out}");
        assert!(out.contains("color: red"), "got: {out}");
        assert!(out.contains("class=\"on\""), "got: {out}");
    }

    #[test]
    fn lazy_generator_builds_dom() {
        let html = "<body><ul id=\"list\"></ul><script>\
            function* count(){ let i = 1; while (true) { yield i; i = i + 1; } }\
            const g = count();\
            const ul = document.getElementById('list');\
            for (let k = 0; k < 3; k++) {\
                const li = document.createElement('li');\
                li.textContent = 'Item ' + g.next().value;\
                ul.appendChild(li);\
            }\
            </script></body>";
        let out = run_inline_scripts(html);
        assert!(
            out.contains("Item 1") && out.contains("Item 2") && out.contains("Item 3"),
            "lazy generator built three list items: {out}"
        );
    }

    #[test]
    fn extended_selectors() {
        let html = "<body><div id=\"wrap\"><p class=\"a\">P1</p><p>P2</p><span data-x=\"yes\">S</span></div>\
            <script>\
            document.body.setAttribute('r1', String(document.querySelectorAll('#wrap > p').length));\
            document.body.setAttribute('r2', document.querySelector('p.a + p').textContent);\
            document.body.setAttribute('r3', document.querySelector('[data-x=yes]').textContent);\
            document.body.setAttribute('r4', String(document.querySelectorAll('div span').length));\
            </script></body>";
        let out = run_inline_scripts(html);
        assert!(out.contains("r1=\"2\""), "child combinator `>`: {out}");
        assert!(out.contains("r2=\"P2\""), "adjacent sibling `+`: {out}");
        assert!(out.contains("r3=\"S\""), "attribute selector `[a=v]`: {out}");
        assert!(out.contains("r4=\"1\""), "descendant: {out}");
    }

    #[test]
    fn script_error_does_not_abort() {
        let html = "<body><p>ok</p><script>throw new Error('boom'); document.body;</script></body>";
        let out = run_inline_scripts(html);
        assert!(out.contains("<p>ok</p>"), "got: {out}");
    }

    #[test]
    fn multiple_scripts_share_global_but_isolate_errors() {
        // Script 1 throws after setting a global; script 2 still runs and sees it.
        let html = "<body><span id=\"s\">_</span><script>globalThis.shared = 'hi'; throw new Error('x');</script>\
            <script>document.getElementById('s').textContent = globalThis.shared;</script></body>";
        let out = run_inline_scripts(html);
        assert!(out.contains(">hi<"), "second script ran with shared global: {out}");
    }
}
