//! DOM bindings: run a document's inline `<script>`s against a live DOM, then
//! serialize the (mutated) tree back to HTML — so the [`crate::html`] renderer
//! reflects script-driven content **without a headless browser**.
//!
//! The DOM is mirrored as JavaScript objects sharing three prototypes
//! (element / text / document), installed on [`Interp::dom_protos`]. Element
//! objects carry internal slots (`__tag`, `__attrs`, `__kids`, `__style`,
//! `__parent`) and the usual surface API: `textContent`, `innerHTML`, `id`,
//! `className`, `classList`, `style`, `getAttribute`/`setAttribute`,
//! `appendChild`/`removeChild`, `children`, plus `document.getElementById`,
//! `getElementsByTagName`, `createElement`/`createTextNode`, `body`, `title`
//! and a `querySelector(All)` engine (`tag`, `#id`, `.class`, descendant
//! combinator and comma groups).
//!
//! Coverage is the practical core used by templating scripts; a script error is
//! caught and rendering continues with whatever DOM state was produced.

use super::interp::{Eval, Interp};
use super::value::*;
use crate::html::dom::Node;
use std::rc::Rc;

/// Execute the inline `<script>`s in `html` and return the resulting HTML.
///
/// If there is no `<script>`, the input is returned unchanged (zero cost).
pub fn run_inline_scripts(html: &str) -> String {
    if !html.to_ascii_lowercase().contains("<script") {
        return html.to_string();
    }
    let nodes = crate::html::dom::parse(html);
    let mut it = Interp::new();
    let (ep, tp, dp) = build_protos(&it);
    it.dom_protos = vec![ep, tp, dp];

    let roots: Vec<Value> = nodes.iter().map(|n| build_node(&it, n)).collect();
    let document = make_document(&it, &roots);
    it.global.borrow_mut().set_data("document", document);
    let window = Value::Object(it.global.clone());
    it.global.borrow_mut().set_data("window", window);

    let mut scripts = Vec::new();
    for r in &roots {
        collect_scripts(r, &mut scripts);
    }
    for src in scripts {
        if let Ok(prog) = super::parser::parse(&src) {
            let _ = it.run(&prog); // a script error must not abort rendering
        }
    }

    let mut out = String::new();
    for r in &roots {
        serialize_node(r, &mut out);
    }
    out
}

// ---- internal-slot helpers -------------------------------------------------

fn arg(a: &[Value], i: usize) -> Value {
    a.get(i).cloned().unwrap_or(Value::Undefined)
}

fn slot(v: &Value, key: &str) -> Value {
    if let Value::Object(o) = v {
        if let Some(PropDesc::Data(d)) = o.borrow().get_own(key) {
            return d.clone();
        }
    }
    Value::Undefined
}

fn set_slot(v: &Value, key: &str, val: Value) {
    if let Value::Object(o) = v {
        o.borrow_mut().set_data(key, val);
    }
}

fn is_element(v: &Value) -> bool {
    matches!(slot(v, "nodeType"), Value::Num(n) if n == 1.0)
}

fn is_text(v: &Value) -> bool {
    matches!(slot(v, "nodeType"), Value::Num(n) if n == 3.0)
}

fn tag_of(v: &Value) -> Option<String> {
    match slot(v, "__tag") {
        Value::Str(s) => Some(s.to_string()),
        _ => None,
    }
}

fn text_of(v: &Value) -> String {
    match slot(v, "__text") {
        Value::Str(s) => s.to_string(),
        _ => String::new(),
    }
}

fn kids(v: &Value) -> Vec<Value> {
    if let Value::Object(o) = &slot(v, "__kids") {
        if let ObjKind::Array(e) = &o.borrow().kind {
            return e.clone();
        }
    }
    Vec::new()
}

fn push_kid(parent: &Value, child: &Value) {
    if let Value::Object(o) = &slot(parent, "__kids") {
        if let ObjKind::Array(e) = &mut o.borrow_mut().kind {
            e.push(child.clone());
        }
    }
    set_slot(child, "__parent", parent.clone());
}

fn get_attr(v: &Value, name: &str) -> Option<String> {
    if let Value::Object(o) = &slot(v, "__attrs") {
        if let Some(PropDesc::Data(Value::Str(s))) = o.borrow().get_own(&name.to_lowercase()) {
            return Some(s.to_string());
        }
    }
    None
}

fn set_attr(v: &Value, name: &str, val: &str) {
    if let Value::Object(o) = &slot(v, "__attrs") {
        o.borrow_mut()
            .set_data(&name.to_lowercase(), Value::str(val));
    }
}

fn same_node(a: &Value, b: &Value) -> bool {
    matches!((a, b), (Value::Object(x), Value::Object(y)) if Rc::ptr_eq(x, y))
}

// ---- node construction -----------------------------------------------------

fn build_node(it: &Interp, node: &Node) -> Value {
    match node {
        Node::Text(s) => make_text(it, s),
        Node::Element(el) => {
            let v = make_element(it, &el.tag, &el.attrs);
            let children: Vec<Value> = el.children.iter().map(|c| build_node(it, c)).collect();
            for c in &children {
                set_slot(c, "__parent", v.clone());
            }
            set_slot(&v, "__kids", it.new_array(children));
            v
        }
    }
}

fn make_element(it: &Interp, tag: &str, attrs: &[(String, String)]) -> Value {
    let proto = it.dom_protos.first().cloned();
    let o = it.new_object(proto);
    let attrs_obj = it.new_object(Some(it.object_proto.clone()));
    for (k, v) in attrs {
        attrs_obj
            .borrow_mut()
            .set_data(&k.to_lowercase(), Value::str(v.clone()));
    }
    let empty = it.new_array(Vec::new());
    {
        let mut b = o.borrow_mut();
        b.class = "HTMLElement";
        let upper = tag.to_uppercase();
        b.set_data("nodeType", Value::Num(1.0));
        b.set_data("tagName", Value::str(upper.clone()));
        b.set_data("nodeName", Value::str(upper));
        b.set_data("__tag", Value::str(tag.to_lowercase()));
        b.set_data("__attrs", Value::Object(attrs_obj));
        b.set_data("__kids", empty);
    }
    Value::Object(o)
}

fn make_text(it: &Interp, text: &str) -> Value {
    let proto = it.dom_protos.get(1).cloned();
    let o = it.new_object(proto);
    {
        let mut b = o.borrow_mut();
        b.class = "Text";
        b.set_data("nodeType", Value::Num(3.0));
        b.set_data("nodeName", Value::str("#text"));
        b.set_data("__text", Value::str(text));
    }
    Value::Object(o)
}

fn make_document(it: &Interp, roots: &[Value]) -> Value {
    let proto = it.dom_protos.get(2).cloned();
    let o = it.new_object(proto);
    let roots_arr = it.new_array(roots.to_vec());
    {
        let mut b = o.borrow_mut();
        b.class = "HTMLDocument";
        b.set_data("nodeType", Value::Num(9.0));
        b.set_data("__roots", roots_arr);
    }
    Value::Object(o)
}

// ---- prototype installation ------------------------------------------------

fn accessor(it: &Interp, proto: &Gc, name: &str, get: NativeFn, set: Option<NativeFn>) {
    let g = it.native_fn(name, get).as_object().cloned();
    let s = set.and_then(|f| it.native_fn(name, f).as_object().cloned());
    proto
        .borrow_mut()
        .set_own(name, PropDesc::Accessor { get: g, set: s });
}

fn build_protos(it: &Interp) -> (Gc, Gc, Gc) {
    let ep = it.new_object(Some(it.object_proto.clone()));
    let tp = it.new_object(Some(it.object_proto.clone()));
    let dp = it.new_object(Some(it.object_proto.clone()));

    // Element accessors.
    accessor(it, &ep, "textContent", el_text_get, Some(el_text_set));
    accessor(it, &ep, "innerHTML", el_inner_get, Some(el_inner_set));
    accessor(it, &ep, "outerHTML", el_outer_get, None);
    accessor(it, &ep, "id", el_id_get, Some(el_id_set));
    accessor(it, &ep, "className", el_class_get, Some(el_class_set));
    accessor(it, &ep, "children", el_children_get, None);
    accessor(it, &ep, "childNodes", el_childnodes_get, None);
    accessor(it, &ep, "firstChild", el_first_child_get, None);
    accessor(it, &ep, "parentNode", el_parent_get, None);
    accessor(it, &ep, "style", el_style_get, None);
    accessor(it, &ep, "classList", el_classlist_get, None);
    // Element methods.
    it.define_method(&ep, "getAttribute", el_get_attribute);
    it.define_method(&ep, "setAttribute", el_set_attribute);
    it.define_method(&ep, "hasAttribute", el_has_attribute);
    it.define_method(&ep, "removeAttribute", el_remove_attribute);
    it.define_method(&ep, "appendChild", el_append_child);
    it.define_method(&ep, "removeChild", el_remove_child);
    it.define_method(&ep, "getElementsByTagName", el_get_by_tag);
    it.define_method(&ep, "querySelector", el_query);
    it.define_method(&ep, "querySelectorAll", el_query_all);

    // Text accessors.
    accessor(it, &tp, "textContent", text_value_get, Some(text_value_set));
    accessor(it, &tp, "nodeValue", text_value_get, Some(text_value_set));

    // Document methods & accessors.
    it.define_method(&dp, "getElementById", doc_get_by_id);
    it.define_method(&dp, "getElementsByTagName", doc_get_by_tag);
    it.define_method(&dp, "querySelector", doc_query);
    it.define_method(&dp, "querySelectorAll", doc_query_all);
    it.define_method(&dp, "createElement", doc_create_element);
    it.define_method(&dp, "createTextNode", doc_create_text_node);
    accessor(it, &dp, "body", doc_body_get, None);
    accessor(it, &dp, "documentElement", doc_root_get, None);
    accessor(it, &dp, "title", doc_title_get, Some(doc_title_set));

    (ep, tp, dp)
}

// ---- element accessors -----------------------------------------------------

fn collect_text(v: &Value) -> String {
    if is_text(v) {
        return text_of(v);
    }
    let mut s = String::new();
    for k in kids(v) {
        s.push_str(&collect_text(&k));
    }
    s
}

fn el_text_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    Ok(Value::str(collect_text(&this)))
}

fn el_text_set(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let txt = it.to_string_v(&arg(a, 0))?;
    let tn = make_text(it, &txt);
    set_slot(&tn, "__parent", this.clone());
    set_slot(&this, "__kids", it.new_array(vec![tn]));
    Ok(Value::Undefined)
}

fn el_inner_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    let mut out = String::new();
    for k in kids(&this) {
        serialize_node(&k, &mut out);
    }
    Ok(Value::str(out))
}

fn el_inner_set(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let html = it.to_string_v(&arg(a, 0))?;
    let parsed = crate::html::dom::parse(&html);
    let children: Vec<Value> = parsed.iter().map(|n| build_node(it, n)).collect();
    for c in &children {
        set_slot(c, "__parent", this.clone());
    }
    set_slot(&this, "__kids", it.new_array(children));
    Ok(Value::Undefined)
}

fn el_outer_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    let mut out = String::new();
    serialize_node(&this, &mut out);
    Ok(Value::str(out))
}

fn el_id_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    Ok(Value::str(get_attr(&this, "id").unwrap_or_default()))
}
fn el_id_set(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let v = it.to_string_v(&arg(a, 0))?;
    set_attr(&this, "id", &v);
    Ok(Value::Undefined)
}
fn el_class_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    Ok(Value::str(get_attr(&this, "class").unwrap_or_default()))
}
fn el_class_set(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let v = it.to_string_v(&arg(a, 0))?;
    set_attr(&this, "class", &v);
    Ok(Value::Undefined)
}

fn el_children_get(it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    let elems: Vec<Value> = kids(&this).into_iter().filter(is_element).collect();
    Ok(it.new_array(elems))
}
fn el_childnodes_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    Ok(slot(&this, "__kids"))
}
fn el_first_child_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    Ok(kids(&this).into_iter().next().unwrap_or(Value::Null))
}
fn el_parent_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    let p = slot(&this, "__parent");
    Ok(if matches!(p, Value::Undefined) {
        Value::Null
    } else {
        p
    })
}

fn el_style_get(it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    let cur = slot(&this, "__style");
    if let Value::Object(_) = cur {
        return Ok(cur);
    }
    let o = Value::Object(it.new_object(Some(it.object_proto.clone())));
    set_slot(&this, "__style", o.clone());
    Ok(o)
}

fn el_classlist_get(it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    let o = it.new_object(Some(it.object_proto.clone()));
    let list = Value::Object(o.clone());
    set_slot(&list, "__el", this);
    it.define_method(&o, "add", classlist_add);
    it.define_method(&o, "remove", classlist_remove);
    it.define_method(&o, "contains", classlist_contains);
    it.define_method(&o, "toggle", classlist_toggle);
    Ok(list)
}

fn class_tokens(el: &Value) -> Vec<String> {
    get_attr(el, "class")
        .unwrap_or_default()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

fn classlist_add(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let el = slot(&this, "__el");
    let name = it.to_string_v(&arg(a, 0))?;
    let mut tokens = class_tokens(&el);
    if !tokens.contains(&name) {
        tokens.push(name);
    }
    set_attr(&el, "class", &tokens.join(" "));
    Ok(Value::Undefined)
}
fn classlist_remove(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let el = slot(&this, "__el");
    let name = it.to_string_v(&arg(a, 0))?;
    let tokens: Vec<String> = class_tokens(&el)
        .into_iter()
        .filter(|t| t != &name)
        .collect();
    set_attr(&el, "class", &tokens.join(" "));
    Ok(Value::Undefined)
}
fn classlist_contains(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let el = slot(&this, "__el");
    let name = it.to_string_v(&arg(a, 0))?;
    Ok(Value::Bool(class_tokens(&el).contains(&name)))
}
fn classlist_toggle(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let el = slot(&this, "__el");
    let name = it.to_string_v(&arg(a, 0))?;
    let mut tokens = class_tokens(&el);
    let present = tokens.contains(&name);
    if present {
        tokens.retain(|t| t != &name);
    } else {
        tokens.push(name);
    }
    set_attr(&el, "class", &tokens.join(" "));
    Ok(Value::Bool(!present))
}

// ---- element methods -------------------------------------------------------

fn el_get_attribute(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let name = it.to_string_v(&arg(a, 0))?;
    Ok(match get_attr(&this, &name) {
        Some(s) => Value::str(s),
        None => Value::Null,
    })
}
fn el_set_attribute(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let name = it.to_string_v(&arg(a, 0))?;
    let val = it.to_string_v(&arg(a, 1))?;
    set_attr(&this, &name, &val);
    Ok(Value::Undefined)
}
fn el_has_attribute(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let name = it.to_string_v(&arg(a, 0))?;
    Ok(Value::Bool(get_attr(&this, &name).is_some()))
}
fn el_remove_attribute(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let name = it.to_string_v(&arg(a, 0))?;
    if let Value::Object(o) = &slot(&this, "__attrs") {
        o.borrow_mut().remove_own(&name.to_lowercase());
    }
    Ok(Value::Undefined)
}
fn el_append_child(_it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let child = arg(a, 0);
    push_kid(&this, &child);
    Ok(child)
}
fn el_remove_child(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let child = arg(a, 0);
    let remaining: Vec<Value> = kids(&this)
        .into_iter()
        .filter(|k| !same_node(k, &child))
        .collect();
    set_slot(&this, "__kids", it.new_array(remaining));
    Ok(child)
}
fn el_get_by_tag(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let tag = it.to_string_v(&arg(a, 0))?.to_lowercase();
    let mut out = Vec::new();
    for k in kids(&this) {
        collect_by_tag(&k, &tag, &mut out);
    }
    Ok(it.new_array(out))
}
fn el_query(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let sel = it.to_string_v(&arg(a, 0))?;
    let res = query(&kids(&this), &sel);
    Ok(res.into_iter().next().unwrap_or(Value::Null))
}
fn el_query_all(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let sel = it.to_string_v(&arg(a, 0))?;
    Ok(it.new_array(query(&kids(&this), &sel)))
}

// ---- text-node accessors ---------------------------------------------------

fn text_value_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    Ok(Value::str(text_of(&this)))
}
fn text_value_set(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let s = it.to_string_v(&arg(a, 0))?;
    set_slot(&this, "__text", Value::str(s));
    Ok(Value::Undefined)
}

// ---- document methods ------------------------------------------------------

fn document_roots(doc: &Value) -> Vec<Value> {
    if let Value::Object(o) = &slot(doc, "__roots") {
        if let ObjKind::Array(e) = &o.borrow().kind {
            return e.clone();
        }
    }
    Vec::new()
}

fn find_by_id(node: &Value, id: &str) -> Option<Value> {
    if is_element(node) {
        if get_attr(node, "id").as_deref() == Some(id) {
            return Some(node.clone());
        }
        for k in kids(node) {
            if let Some(f) = find_by_id(&k, id) {
                return Some(f);
            }
        }
    }
    None
}

fn doc_get_by_id(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let id = it.to_string_v(&arg(a, 0))?;
    for r in document_roots(&this) {
        if let Some(f) = find_by_id(&r, &id) {
            return Ok(f);
        }
    }
    Ok(Value::Null)
}

fn collect_by_tag(node: &Value, tag: &str, out: &mut Vec<Value>) {
    if is_element(node) {
        if tag == "*" || tag_of(node).as_deref() == Some(tag) {
            out.push(node.clone());
        }
        for k in kids(node) {
            collect_by_tag(&k, tag, out);
        }
    }
}

fn doc_get_by_tag(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let tag = it.to_string_v(&arg(a, 0))?.to_lowercase();
    let mut out = Vec::new();
    for r in document_roots(&this) {
        collect_by_tag(&r, &tag, &mut out);
    }
    Ok(it.new_array(out))
}

fn doc_query(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let sel = it.to_string_v(&arg(a, 0))?;
    let res = query(&document_roots(&this), &sel);
    Ok(res.into_iter().next().unwrap_or(Value::Null))
}
fn doc_query_all(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let sel = it.to_string_v(&arg(a, 0))?;
    Ok(it.new_array(query(&document_roots(&this), &sel)))
}
fn doc_create_element(it: &mut Interp, _this: Value, a: &[Value]) -> Eval<Value> {
    let tag = it.to_string_v(&arg(a, 0))?;
    Ok(make_element(it, &tag, &[]))
}
fn doc_create_text_node(it: &mut Interp, _this: Value, a: &[Value]) -> Eval<Value> {
    let text = it.to_string_v(&arg(a, 0))?;
    Ok(make_text(it, &text))
}

fn find_first_tag(roots: &[Value], tag: &str) -> Option<Value> {
    let mut out = Vec::new();
    for r in roots {
        collect_by_tag(r, tag, &mut out);
    }
    out.into_iter().next()
}

fn doc_body_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    Ok(find_first_tag(&document_roots(&this), "body").unwrap_or(Value::Null))
}
fn doc_root_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    let roots = document_roots(&this);
    Ok(find_first_tag(&roots, "html")
        .or_else(|| roots.into_iter().find(is_element))
        .unwrap_or(Value::Null))
}
fn doc_title_get(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    match find_first_tag(&document_roots(&this), "title") {
        Some(t) => Ok(Value::str(collect_text(&t))),
        None => Ok(Value::str("")),
    }
}
fn doc_title_set(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let title = it.to_string_v(&arg(a, 0))?;
    if let Some(t) = find_first_tag(&document_roots(&this), "title") {
        let tn = make_text(it, &title);
        set_slot(&t, "__kids", it.new_array(vec![tn]));
    }
    Ok(Value::Undefined)
}

// ---- selector engine -------------------------------------------------------
//
// Supports comma groups, the descendant (` `), child (`>`), next-sibling (`+`)
// and subsequent-sibling (`~`) combinators, plus `tag`, `#id`, `.class`, `*`
// and attribute selectors (`[a]`, `[a=v]`, `[a^=v]`, `[a$=v]`, `[a*=v]`,
// `[a~=v]`, `[a|=v]`). Matching is performed right-to-left from the subject.

/// How an attribute value must relate to the selector value.
#[derive(Clone, Copy, PartialEq)]
enum AttrOp {
    Exact,
    Prefix,
    Suffix,
    Contains,
    Word,
    Dash,
}

/// One `[attr…]` condition.
struct AttrSel {
    name: String,
    op: Option<AttrOp>,
    value: String,
}

/// A compound selector (tag + id + classes + attribute conditions).
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<AttrSel>,
}

/// A combinator relating a compound to the one on its left.
#[derive(Clone, Copy, PartialEq)]
enum Comb {
    Descendant,
    Child,
    NextSibling,
    Subsequent,
}

/// One step of a complex selector.
struct Step {
    comb: Comb,
    compound: Compound,
}

fn read_name(chars: &[char], i: &mut usize) -> String {
    let mut s = String::new();
    while *i < chars.len() {
        let c = chars[*i];
        if c.is_alphanumeric() || c == '-' || c == '_' {
            s.push(c);
            *i += 1;
        } else {
            break;
        }
    }
    s
}

fn read_attr(chars: &[char], i: &mut usize) -> AttrSel {
    // `i` is just past `[`.
    let name = read_name(chars, i);
    let mut op = None;
    let mut value = String::new();
    if *i < chars.len() && chars[*i] != ']' {
        op = Some(match chars[*i] {
            '^' => {
                *i += 1;
                AttrOp::Prefix
            }
            '$' => {
                *i += 1;
                AttrOp::Suffix
            }
            '*' => {
                *i += 1;
                AttrOp::Contains
            }
            '~' => {
                *i += 1;
                AttrOp::Word
            }
            '|' => {
                *i += 1;
                AttrOp::Dash
            }
            _ => AttrOp::Exact,
        });
        if *i < chars.len() && chars[*i] == '=' {
            *i += 1;
        }
        // Optional quote.
        let quote = chars.get(*i).copied().filter(|c| *c == '"' || *c == '\'');
        if quote.is_some() {
            *i += 1;
        }
        while *i < chars.len() {
            let c = chars[*i];
            if Some(c) == quote || (quote.is_none() && c == ']') {
                break;
            }
            value.push(c);
            *i += 1;
        }
        if quote.is_some() && *i < chars.len() {
            *i += 1; // closing quote
        }
    }
    // Skip to and past `]`.
    while *i < chars.len() && chars[*i] != ']' {
        *i += 1;
    }
    if *i < chars.len() {
        *i += 1;
    }
    AttrSel {
        name: name.to_lowercase(),
        op,
        value,
    }
}

fn parse_compound(tok: &str) -> Compound {
    let mut tag = None;
    let mut id = None;
    let mut classes = Vec::new();
    let mut attrs = Vec::new();
    let chars: Vec<char> = tok.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '.' => {
                i += 1;
                classes.push(read_name(&chars, &mut i));
            }
            '#' => {
                i += 1;
                id = Some(read_name(&chars, &mut i));
            }
            '[' => {
                i += 1;
                attrs.push(read_attr(&chars, &mut i));
            }
            '*' => {
                i += 1;
                tag = Some("*".to_string());
            }
            c if c.is_alphanumeric() || c == '-' || c == '_' => {
                let name = read_name(&chars, &mut i);
                if !name.is_empty() {
                    tag = Some(name.to_lowercase());
                }
            }
            _ => i += 1,
        }
    }
    Compound {
        tag,
        id,
        classes,
        attrs,
    }
}

fn parse_selector(sel: &str) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut buf = String::new();
    let mut comb = Comb::Descendant;
    let mut depth = 0i32;
    let flush = |buf: &mut String, comb: &mut Comb, steps: &mut Vec<Step>| {
        if !buf.is_empty() {
            steps.push(Step {
                comb: *comb,
                compound: parse_compound(buf),
            });
            buf.clear();
            *comb = Comb::Descendant;
        }
    };
    for c in sel.trim().chars() {
        match c {
            '[' => {
                depth += 1;
                buf.push(c);
            }
            ']' => {
                depth -= 1;
                buf.push(c);
            }
            ' ' | '\t' | '\n' | '\r' if depth == 0 => flush(&mut buf, &mut comb, &mut steps),
            '>' if depth == 0 => {
                flush(&mut buf, &mut comb, &mut steps);
                comb = Comb::Child;
            }
            '+' if depth == 0 => {
                flush(&mut buf, &mut comb, &mut steps);
                comb = Comb::NextSibling;
            }
            '~' if depth == 0 => {
                flush(&mut buf, &mut comb, &mut steps);
                comb = Comb::Subsequent;
            }
            _ => buf.push(c),
        }
    }
    flush(&mut buf, &mut comb, &mut steps);
    steps
}

fn matches_compound(node: &Value, c: &Compound) -> bool {
    if let Some(t) = &c.tag {
        if t != "*" && tag_of(node).as_deref() != Some(t.as_str()) {
            return false;
        }
    }
    if let Some(id) = &c.id {
        if get_attr(node, "id").as_deref() != Some(id.as_str()) {
            return false;
        }
    }
    if !c.classes.is_empty() {
        let have = class_tokens(node);
        if !c.classes.iter().all(|w| have.contains(w)) {
            return false;
        }
    }
    for a in &c.attrs {
        let have = get_attr(node, &a.name);
        let ok = match &a.op {
            None => have.is_some(),
            Some(op) => match have {
                None => false,
                Some(h) => match op {
                    AttrOp::Exact => h == a.value,
                    AttrOp::Prefix => h.starts_with(&a.value),
                    AttrOp::Suffix => h.ends_with(&a.value),
                    AttrOp::Contains => h.contains(&a.value),
                    AttrOp::Word => h.split_whitespace().any(|w| w == a.value),
                    AttrOp::Dash => h == a.value || h.starts_with(&format!("{}-", a.value)),
                },
            },
        };
        if !ok {
            return false;
        }
    }
    true
}

fn parent_node(node: &Value) -> Option<Value> {
    match slot(node, "__parent") {
        Value::Object(o) => Some(Value::Object(o)),
        _ => None,
    }
}

fn prev_element_sibling(node: &Value) -> Option<Value> {
    let parent = parent_node(node)?;
    let sibs: Vec<Value> = kids(&parent).into_iter().filter(is_element).collect();
    let idx = sibs.iter().position(|s| same_node(s, node))?;
    if idx == 0 {
        None
    } else {
        Some(sibs[idx - 1].clone())
    }
}

/// Right-to-left match: does `node` satisfy `steps[..=i]` ending at the subject?
fn matches_chain(node: &Value, steps: &[Step], i: usize) -> bool {
    if !matches_compound(node, &steps[i].compound) {
        return false;
    }
    if i == 0 {
        return true;
    }
    match steps[i].comb {
        Comb::Descendant => {
            let mut p = parent_node(node);
            while let Some(a) = p {
                if matches_chain(&a, steps, i - 1) {
                    return true;
                }
                p = parent_node(&a);
            }
            false
        }
        Comb::Child => match parent_node(node) {
            Some(p) => matches_chain(&p, steps, i - 1),
            None => false,
        },
        Comb::NextSibling => match prev_element_sibling(node) {
            Some(s) => matches_chain(&s, steps, i - 1),
            None => false,
        },
        Comb::Subsequent => {
            let mut s = prev_element_sibling(node);
            while let Some(sib) = s {
                if matches_chain(&sib, steps, i - 1) {
                    return true;
                }
                s = prev_element_sibling(&sib);
            }
            false
        }
    }
}

fn collect_elements(node: &Value, out: &mut Vec<Value>) {
    if is_element(node) {
        out.push(node.clone());
        for k in kids(node) {
            collect_elements(&k, out);
        }
    }
}

/// Run a selector (comma groups, all combinators, attribute selectors) over a
/// scope, returning matches in document order.
fn query(scope: &[Value], selector: &str) -> Vec<Value> {
    let mut candidates = Vec::new();
    for n in scope {
        collect_elements(n, &mut candidates);
    }
    let mut out: Vec<Value> = Vec::new();
    for group in selector.split(',') {
        let steps = parse_selector(group);
        if steps.is_empty() {
            continue;
        }
        let last = steps.len() - 1;
        for el in &candidates {
            if matches_chain(el, &steps, last) && !out.iter().any(|y| same_node(y, el)) {
                out.push(el.clone());
            }
        }
    }
    // Document order (candidates is DFS order).
    let mut ordered = Vec::new();
    for el in &candidates {
        if out.iter().any(|y| same_node(y, el)) {
            ordered.push(el.clone());
        }
    }
    ordered
}

// ---- script collection & serialization -------------------------------------

fn collect_scripts(node: &Value, out: &mut Vec<String>) {
    if !is_element(node) {
        return;
    }
    if tag_of(node).as_deref() == Some("script") {
        // Only inline scripts (no `src`) are executed.
        if get_attr(node, "src").is_none() {
            out.push(collect_text(node));
        }
        return;
    }
    for k in kids(node) {
        collect_scripts(&k, out);
    }
}

/// Void elements that never have a closing tag.
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

fn prim_to_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        Value::Num(n) => num_to_str(*n),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// camelCase → kebab-case for CSS style property names.
fn kebab(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn build_style_string(el: &Value) -> String {
    let style = slot(el, "__style");
    let o = match &style {
        Value::Object(o) => o,
        _ => return String::new(),
    };
    let mut parts = Vec::new();
    for (k, p) in &o.borrow().props {
        if let PropDesc::Data(v) = p {
            let val = prim_to_string(v);
            if !val.is_empty() {
                parts.push(format!("{}: {}", kebab(k), val));
            }
        }
    }
    parts.join("; ")
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

fn serialize_node(v: &Value, out: &mut String) {
    if is_text(v) {
        out.push_str(&escape_text(&text_of(v)));
        return;
    }
    if !is_element(v) {
        return;
    }
    let tag = tag_of(v).unwrap_or_default();
    if tag == "script" {
        return; // executed already; never rendered
    }

    // Gather attributes (overriding `style` with the computed inline style).
    let mut attrs: Vec<(String, String)> = Vec::new();
    if let Value::Object(o) = &slot(v, "__attrs") {
        for (k, p) in &o.borrow().props {
            if let PropDesc::Data(val) = p {
                attrs.push((k.clone(), prim_to_string(val)));
            }
        }
    }
    let style = build_style_string(v);
    if !style.is_empty() {
        if let Some(slot) = attrs.iter_mut().find(|(k, _)| k == "style") {
            slot.1 = style;
        } else {
            attrs.push(("style".to_string(), style));
        }
    }

    out.push('<');
    out.push_str(&tag);
    for (k, val) in &attrs {
        out.push(' ');
        out.push_str(k);
        out.push_str("=\"");
        out.push_str(&escape_attr(val));
        out.push('"');
    }
    out.push('>');
    if is_void(&tag) {
        return;
    }
    for k in kids(v) {
        serialize_node(&k, out);
    }
    out.push_str("</");
    out.push_str(&tag);
    out.push('>');
}

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
        // An infinite generator drives DOM construction lazily through the VM:
        // only three items are pulled from `while (true)`.
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
        assert!(
            out.contains("r3=\"S\""),
            "attribute selector `[a=v]`: {out}"
        );
        assert!(out.contains("r4=\"1\""), "descendant: {out}");
    }

    #[test]
    fn script_error_does_not_abort() {
        let html = "<body><p>ok</p><script>throw new Error('boom'); document.body;</script></body>";
        let out = run_inline_scripts(html);
        assert!(out.contains("<p>ok</p>"), "got: {out}");
    }
}
