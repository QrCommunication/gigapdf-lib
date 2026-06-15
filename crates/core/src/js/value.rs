//! The JavaScript value model and lexical environments.
//!
//! Objects form a shared, mutable graph (closures capture environments,
//! prototypes are shared), so they live behind `Rc<RefCell<…>>` ([`Gc`]).
//! WebAssembly is single-threaded, so `Rc`/`RefCell` (not `Arc`/`Mutex`) are
//! the right tools and there is no real garbage collector — reference cycles
//! leak, which is acceptable for the short-lived script runs that drive a
//! single HTML render.
//!
//! This module holds the data model plus the *pure* abstract operations
//! (`to_boolean`, `num_to_str`, strict equality, `typeof`); operations that may
//! call user code (`to_primitive`, `to_number` on objects, abstract `==`) live
//! in [`super::interp`] because they need the interpreter.

use super::ast;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A reference-counted, interior-mutable object handle.
pub type Gc = Rc<RefCell<Obj>>;

/// A JavaScript value.
#[derive(Debug, Clone)]
pub enum Value {
    /// `undefined`.
    Undefined,
    /// `null`.
    Null,
    /// A boolean.
    Bool(bool),
    /// A double-precision number.
    Num(f64),
    /// A string.
    Str(Rc<str>),
    /// An object (including arrays and functions).
    Object(Gc),
}

impl Value {
    /// Construct a string value from anything string-like.
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(Rc::from(s.into().as_str()))
    }

    /// `true` if this is `undefined` or `null`.
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undefined | Value::Null)
    }

    /// Borrow the object handle if this is an object.
    pub fn as_object(&self) -> Option<&Gc> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// `true` if this value is callable (a function object).
    pub fn is_callable(&self) -> bool {
        matches!(self, Value::Object(o) if matches!(o.borrow().kind, ObjKind::Function(_)))
    }
}

/// An object: a property map, a `[[Prototype]]`, and an internal kind.
///
/// `Debug` is implemented by hand and is **shallow** (class, kind tag and own
/// key names only): the object graph is cyclic by design (e.g.
/// `Object.prototype.constructor → Object → .prototype → Object.prototype`), so
/// a derived, recursive `Debug` would overflow the stack.
pub struct Obj {
    /// `[[Prototype]]`.
    pub proto: Option<Gc>,
    /// Own properties, in insertion order (JS preserves string-key order).
    pub props: Vec<(String, PropDesc)>,
    /// The internal kind (plain / array / function).
    pub kind: ObjKind,
    /// `[[Extensible]]` — `false` after `Object.freeze`/`preventExtensions`.
    pub extensible: bool,
    /// The `[[Class]]` tag used by `Object.prototype.toString`.
    pub class: &'static str,
}

impl Obj {
    /// A bare plain object with the given prototype.
    pub fn plain(proto: Option<Gc>) -> Obj {
        Obj {
            proto,
            props: Vec::new(),
            kind: ObjKind::Plain,
            extensible: true,
            class: "Object",
        }
    }

    /// Find an own property by key.
    pub fn get_own(&self, key: &str) -> Option<&PropDesc> {
        self.props.iter().find(|(k, _)| k == key).map(|(_, p)| p)
    }

    /// Insert or overwrite an own data/accessor property (preserving order).
    pub fn set_own(&mut self, key: &str, prop: PropDesc) {
        if let Some(slot) = self.props.iter_mut().find(|(k, _)| k == key) {
            slot.1 = prop;
        } else {
            self.props.push((key.to_string(), prop));
        }
    }

    /// Convenience: insert an own data property.
    pub fn set_data(&mut self, key: &str, value: Value) {
        self.set_own(key, PropDesc::Data(value));
    }

    /// Remove an own property; returns `true` if it existed.
    pub fn remove_own(&mut self, key: &str) -> bool {
        if let Some(i) = self.props.iter().position(|(k, _)| k == key) {
            self.props.remove(i);
            true
        } else {
            false
        }
    }
}

impl core::fmt::Debug for Obj {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match &self.kind {
            ObjKind::Plain => "Plain".to_string(),
            ObjKind::Array(e) => format!("Array(len={})", e.len()),
            ObjKind::Function(_) => "Function".to_string(),
        };
        let keys: Vec<&str> = self.props.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("Obj")
            .field("class", &self.class)
            .field("kind", &kind)
            .field("keys", &keys)
            .finish()
    }
}

/// The internal kind of an object.
#[derive(Debug)]
pub enum ObjKind {
    /// An ordinary object.
    Plain,
    /// An array — dense element storage plus the ordinary property map.
    Array(Vec<Value>),
    /// A callable function.
    Function(Callable),
}

/// A property descriptor: a data value or an accessor pair.
#[derive(Debug, Clone)]
pub enum PropDesc {
    /// A data property.
    Data(Value),
    /// An accessor property (`get`/`set` are function objects).
    Accessor {
        /// The getter, if any.
        get: Option<Gc>,
        /// The setter, if any.
        set: Option<Gc>,
    },
}

/// A native (Rust) function implementation.
///
/// Signature: `(interpreter, this, args) -> result`.
pub type NativeFn = fn(&mut super::interp::Interp, Value, &[Value]) -> super::interp::Eval<Value>;

/// What backs a function object.
#[derive(Debug, Clone)]
pub enum Callable {
    /// A user function/arrow defined in source.
    User {
        /// The AST definition.
        def: Rc<ast::Func>,
        /// The captured (closure) environment.
        env: Env,
        /// `true` for arrow functions (lexical `this`).
        is_arrow: bool,
        /// For arrows, the `this` captured at definition time.
        captured_this: Option<Box<Value>>,
        /// The `[[HomeObject]]` (the prototype/object the method lives on),
        /// used to resolve `super.x`. `None` for plain functions.
        home: Option<Box<Value>>,
    },
    /// A built-in implemented in Rust.
    Native {
        /// Display name.
        name: String,
        /// The implementation.
        f: NativeFn,
    },
    /// A bound function (`Function.prototype.bind`): a target plus a fixed
    /// `this` and leading arguments.
    Bound {
        /// The wrapped function value.
        target: Box<Value>,
        /// The bound `this`.
        bound_this: Box<Value>,
        /// Arguments prepended to every call.
        bound_args: Vec<Value>,
    },
}

// ---- environments ----------------------------------------------------------

/// A lexical scope handle.
pub type Env = Rc<RefCell<Scope>>;

/// A single lexical scope.
#[derive(Debug)]
pub struct Scope {
    /// Bindings declared directly in this scope.
    pub vars: HashMap<String, Binding>,
    /// The enclosing scope.
    pub parent: Option<Env>,
    /// `true` if this is a function (or global) scope — the target for
    /// `var`/function hoisting.
    pub is_function_scope: bool,
    /// The `this` value bound at this scope (set on non-arrow function entry).
    pub this_val: Option<Value>,
    /// The `[[HomeObject]]` for `super.x` (set on non-arrow method entry).
    pub home: Option<Value>,
    /// The function value being executed (set on non-arrow entry; lets
    /// `super(...)` reach the parent constructor via its `[[Prototype]]`).
    pub current_fn: Option<Value>,
}

/// A variable binding.
#[derive(Debug)]
pub struct Binding {
    /// The current value.
    pub value: Value,
    /// `false` for `const`.
    pub mutable: bool,
}

/// Create a new scope.
pub fn new_scope(parent: Option<Env>, is_function_scope: bool) -> Env {
    Rc::new(RefCell::new(Scope {
        vars: HashMap::new(),
        parent,
        is_function_scope,
        this_val: None,
        home: None,
        current_fn: None,
    }))
}

/// Resolve the `[[HomeObject]]` for `super` by walking to the nearest function
/// (non-arrow) scope.
pub fn scope_home(env: &Env) -> Option<Value> {
    let mut cur = env.clone();
    loop {
        if cur.borrow().this_val.is_some() {
            return cur.borrow().home.clone();
        }
        let parent = cur.borrow().parent.clone();
        match parent {
            Some(p) => cur = p,
            None => return None,
        }
    }
}

/// Resolve the currently-executing function value (for `super(...)`).
pub fn scope_current_fn(env: &Env) -> Option<Value> {
    let mut cur = env.clone();
    loop {
        if cur.borrow().this_val.is_some() {
            return cur.borrow().current_fn.clone();
        }
        let parent = cur.borrow().parent.clone();
        match parent {
            Some(p) => cur = p,
            None => return None,
        }
    }
}

/// Look up `name`, walking outward through parent scopes.
pub fn scope_get(env: &Env, name: &str) -> Option<Value> {
    let mut cur = env.clone();
    loop {
        if let Some(b) = cur.borrow().vars.get(name) {
            return Some(b.value.clone());
        }
        let parent = cur.borrow().parent.clone();
        match parent {
            Some(p) => cur = p,
            None => return None,
        }
    }
}

/// Whether a binding for `name` exists in any enclosing scope.
pub fn scope_has(env: &Env, name: &str) -> bool {
    let mut cur = env.clone();
    loop {
        if cur.borrow().vars.contains_key(name) {
            return true;
        }
        let parent = cur.borrow().parent.clone();
        match parent {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

/// The outcome of assigning to a scope binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOutcome {
    /// The binding was found and updated.
    Set,
    /// No binding exists in any enclosing scope (the caller may create an
    /// implicit global in sloppy mode).
    NotFound,
    /// The binding exists but is `const`.
    Const,
}

/// Assign to an existing binding, walking outward through parent scopes.
pub fn scope_set(env: &Env, name: &str, value: Value) -> SetOutcome {
    let mut cur = env.clone();
    loop {
        {
            let mut b = cur.borrow_mut();
            if let Some(binding) = b.vars.get_mut(name) {
                if !binding.mutable {
                    return SetOutcome::Const;
                }
                binding.value = value;
                return SetOutcome::Set;
            }
        }
        let parent = cur.borrow().parent.clone();
        match parent {
            Some(p) => cur = p,
            None => return SetOutcome::NotFound,
        }
    }
}

/// Declare a binding in the current (innermost) scope.
pub fn scope_declare(env: &Env, name: &str, value: Value, mutable: bool) {
    env.borrow_mut()
        .vars
        .insert(name.to_string(), Binding { value, mutable });
}

/// Assign (creating if needed) a `var` in the nearest function scope,
/// overwriting any hoisted `undefined` — used when a `var x = …` initializer
/// executes.
pub fn scope_var_set(env: &Env, name: &str, value: Value) {
    let mut cur = env.clone();
    loop {
        let is_fn = cur.borrow().is_function_scope;
        if is_fn || cur.borrow().parent.is_none() {
            cur.borrow_mut()
                .vars
                .insert(name.to_string(), Binding { value, mutable: true });
            return;
        }
        let parent = cur.borrow().parent.clone();
        match parent {
            Some(p) => cur = p,
            None => return,
        }
    }
}

/// Hoist a `var`/function binding into the nearest function scope *without*
/// clobbering an existing binding.
pub fn scope_declare_var(env: &Env, name: &str, value: Value) {
    let mut cur = env.clone();
    loop {
        let is_fn = cur.borrow().is_function_scope;
        if is_fn || cur.borrow().parent.is_none() {
            cur.borrow_mut()
                .vars
                .entry(name.to_string())
                .or_insert(Binding {
                    value,
                    mutable: true,
                });
            return;
        }
        let parent = cur.borrow().parent.clone();
        match parent {
            Some(p) => cur = p,
            None => return,
        }
    }
}

/// Resolve `this` by walking to the nearest scope that binds it.
pub fn scope_this(env: &Env) -> Value {
    let mut cur = env.clone();
    loop {
        if let Some(t) = &cur.borrow().this_val {
            return t.clone();
        }
        let parent = cur.borrow().parent.clone();
        match parent {
            Some(p) => cur = p,
            None => return Value::Undefined,
        }
    }
}

// ---- pure abstract operations ---------------------------------------------

/// `ToBoolean` (ECMA-262 §7.1.2).
pub fn to_boolean(v: &Value) -> bool {
    match v {
        Value::Undefined | Value::Null => false,
        Value::Bool(b) => *b,
        Value::Num(n) => *n != 0.0 && !n.is_nan(),
        Value::Str(s) => !s.is_empty(),
        Value::Object(_) => true,
    }
}

/// `typeof` string for a value.
pub fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Undefined => "undefined",
        Value::Null => "object",
        Value::Bool(_) => "boolean",
        Value::Num(_) => "number",
        Value::Str(_) => "string",
        Value::Object(o) => {
            let b = o.borrow();
            if matches!(b.kind, ObjKind::Function(_)) {
                "function"
            } else if b.class == "Symbol" {
                "symbol"
            } else {
                "object"
            }
        }
    }
}

/// Number → string, matching JavaScript's `Number::toString` for the common
/// cases (integers without a trailing `.0`, `NaN`/`Infinity` spellings).
pub fn num_to_str(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if n == 0.0 {
        return "0".to_string(); // also collapses -0
    }
    // Rust's shortest round-tripping Display matches JS for finite magnitudes in
    // the normal range; exponential formatting for extremes differs slightly.
    let s = format!("{n}");
    s
}

/// Parse a string to a number the way `Number(str)` does (trim, empty → 0, hex
/// prefix, otherwise a float parse; failure → `NaN`).
pub fn str_to_num(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    if let Some(oct) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        return u64::from_str_radix(oct, 8).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    if let Some(bin) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        return u64::from_str_radix(bin, 2).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    match t {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ => t.parse::<f64>().unwrap_or(f64::NAN),
    }
}

/// `ToInt32` (ECMA-262 §7.1.6) — for bitwise operators.
pub fn to_int32(n: f64) -> i32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let m = n.trunc();
    let modulo = m.rem_euclid(4294967296.0); // 2^32
    if modulo >= 2147483648.0 {
        (modulo - 4294967296.0) as i64 as i32
    } else {
        modulo as i64 as i32
    }
}

/// `ToUint32` (ECMA-262 §7.1.7) — for the unsigned right shift.
pub fn to_uint32(n: f64) -> u32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    n.trunc().rem_euclid(4294967296.0) as i64 as u32
}

/// Strict equality `===` (ECMA-262 §7.2.16).
pub fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Undefined, Value::Undefined) => true,
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Num(x), Value::Num(y)) => x == y, // NaN ≠ NaN, +0 == -0
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Object(x), Value::Object(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}
