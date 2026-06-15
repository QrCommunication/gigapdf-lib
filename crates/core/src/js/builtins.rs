//! The standard library: global object, intrinsic prototypes and built-in
//! functions, all implemented in pure Rust as [`NativeFn`]s.
//!
//! Coverage is the practical core used by template/render scripts: `console`,
//! `Math`, `JSON`, `Object`, `Array.prototype.*`, `String.prototype.*`,
//! `Number`, `Function.prototype.{call,apply,bind}`, the error constructors,
//! and the global helpers (`parseInt`/`parseFloat`/`isNaN`/`isFinite`). String
//! operations are literal (no `RegExp` engine yet); generators, `Symbol`,
//! `Map`/`Set`, `Promise` and timers are not included.

use super::interp::{Eval, Interp};
use super::value::*;
use std::rc::Rc;

/// Install all intrinsics onto a freshly-created interpreter.
pub fn install(it: &mut Interp) {
    install_object(it);
    install_function_proto(it);
    install_array(it);
    install_string(it);
    install_number_boolean(it);
    install_errors(it);
    install_regexp(it);
    install_map_set(it);
    install_promise(it);
    install_generator(it);
    install_math(it);
    install_json(it);
    install_console(it);
    install_globals(it);
}

// ---- small helpers ---------------------------------------------------------

/// The `i`-th argument, or `undefined`.
fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Undefined)
}

/// Snapshot an array's elements (empty if `this` is not an array).
fn array_snapshot(this: &Value) -> Vec<Value> {
    if let Value::Object(o) = this {
        if let ObjKind::Array(e) = &o.borrow().kind {
            return e.clone();
        }
    }
    Vec::new()
}

/// Mutate an array's elements in place.
fn with_array<R>(this: &Value, f: impl FnOnce(&mut Vec<Value>) -> R) -> Option<R> {
    if let Value::Object(o) = this {
        if let ObjKind::Array(e) = &mut o.borrow_mut().kind {
            return Some(f(e));
        }
    }
    None
}

fn set_global(it: &Interp, name: &str, value: Value) {
    it.global.borrow_mut().set_data(name, value);
}

/// Create a constructor function and wire `ctor.prototype <-> proto`.
fn make_ctor(it: &Interp, name: &str, f: NativeFn, proto: &Gc) -> Value {
    let ctor = it.native_fn(name, f);
    if let Value::Object(c) = &ctor {
        c.borrow_mut()
            .set_data("prototype", Value::Object(proto.clone()));
    }
    proto.borrow_mut().set_data("constructor", ctor.clone());
    ctor
}

// ---- Object ----------------------------------------------------------------

fn install_object(it: &mut Interp) {
    let proto = it.object_proto.clone();
    it.define_method(&proto, "hasOwnProperty", |it, this, args| {
        let key = it.to_string_v(&arg(args, 0))?;
        Ok(Value::Bool(match &this {
            Value::Object(o) => {
                let b = o.borrow();
                if let ObjKind::Array(e) = &b.kind {
                    key == "length" || key.parse::<usize>().map(|i| i < e.len()).unwrap_or(false)
                } else {
                    b.get_own(&key).is_some()
                }
            }
            _ => false,
        }))
    });
    it.define_method(&proto, "toString", |_it, this, _args| {
        let tag = match &this {
            Value::Object(o) => o.borrow().class,
            _ => "Object",
        };
        Ok(Value::str(format!("[object {tag}]")))
    });
    it.define_method(&proto, "valueOf", |_it, this, _args| Ok(this));
    it.define_method(&proto, "isPrototypeOf", |_it, this, args| {
        let target = arg(args, 0);
        let proto = match &this {
            Value::Object(o) => o.clone(),
            _ => return Ok(Value::Bool(false)),
        };
        let mut cur = match &target {
            Value::Object(o) => o.borrow().proto.clone(),
            _ => None,
        };
        while let Some(c) = cur {
            if Rc::ptr_eq(&c, &proto) {
                return Ok(Value::Bool(true));
            }
            cur = c.borrow().proto.clone();
        }
        Ok(Value::Bool(false))
    });

    let ctor = make_ctor(it, "Object", obj_construct, &proto);
    if let Value::Object(c) = &ctor {
        let cc = c.clone();
        it.define_method(&cc, "keys", |it, _this, args| {
            let keys = it.enum_keys(&arg(args, 0));
            Ok(it.new_array(keys.into_iter().map(Value::str).collect()))
        });
        it.define_method(&cc, "values", |it, _this, args| {
            let src = arg(args, 0);
            let mut out = Vec::new();
            for k in it.enum_keys(&src) {
                out.push(it.get_member(&src, &k)?);
            }
            Ok(it.new_array(out))
        });
        it.define_method(&cc, "entries", |it, _this, args| {
            let src = arg(args, 0);
            let mut out = Vec::new();
            for k in it.enum_keys(&src) {
                let v = it.get_member(&src, &k)?;
                out.push(it.new_array(vec![Value::str(k), v]));
            }
            Ok(it.new_array(out))
        });
        it.define_method(&cc, "assign", |it, _this, args| {
            let target = arg(args, 0);
            for src in &args[1.min(args.len())..] {
                for k in it.enum_keys(src) {
                    let v = it.get_member(src, &k)?;
                    it.set_member(&target, &k, v)?;
                }
            }
            Ok(target)
        });
        it.define_method(&cc, "freeze", |_it, _this, args| {
            let v = arg(args, 0);
            if let Value::Object(o) = &v {
                o.borrow_mut().extensible = false;
            }
            Ok(v)
        });
        it.define_method(&cc, "create", |it, _this, args| {
            let proto = match arg(args, 0) {
                Value::Object(o) => Some(o),
                Value::Null => None,
                _ => Some(it.object_proto.clone()),
            };
            Ok(Value::Object(it.new_object(proto)))
        });
        it.define_method(&cc, "getPrototypeOf", |_it, _this, args| {
            Ok(match arg(args, 0) {
                Value::Object(o) => match o.borrow().proto.clone() {
                    Some(p) => Value::Object(p),
                    None => Value::Null,
                },
                _ => Value::Null,
            })
        });
        it.define_method(&cc, "fromEntries", |it, _this, args| {
            let obj = it.new_object(Some(it.object_proto.clone()));
            for pair in it.iterate(&arg(args, 0))? {
                let k = it.get_member(&pair, "0")?;
                let v = it.get_member(&pair, "1")?;
                let key = it.to_string_v(&k)?;
                obj.borrow_mut().set_data(&key, v);
            }
            Ok(Value::Object(obj))
        });
    }
    set_global(it, "Object", ctor);
}

fn obj_construct(it: &mut Interp, _this: Value, args: &[Value]) -> Eval<Value> {
    match arg(args, 0) {
        Value::Object(o) => Ok(Value::Object(o)),
        _ => Ok(Value::Object(it.new_object(Some(it.object_proto.clone())))),
    }
}

// ---- Function.prototype -----------------------------------------------------

fn install_function_proto(it: &mut Interp) {
    let proto = it.function_proto.clone();
    it.define_method(&proto, "call", |it, this, args| {
        let new_this = arg(args, 0);
        let rest = if args.is_empty() { &[][..] } else { &args[1..] };
        it.call(&this, new_this, rest)
    });
    it.define_method(&proto, "apply", |it, this, args| {
        let new_this = arg(args, 0);
        let list = match arg(args, 1) {
            Value::Undefined | Value::Null => Vec::new(),
            other => it.iterate(&other)?,
        };
        it.call(&this, new_this, &list)
    });
    it.define_method(&proto, "bind", |it, this, args| {
        let bound_this = arg(args, 0);
        let bound_args = if args.is_empty() {
            Vec::new()
        } else {
            args[1..].to_vec()
        };
        let obj = it.new_object(Some(it.function_proto.clone()));
        {
            let mut b = obj.borrow_mut();
            b.class = "Function";
            b.kind = ObjKind::Function(Callable::Bound {
                target: Box::new(this),
                bound_this: Box::new(bound_this),
                bound_args,
            });
        }
        Ok(Value::Object(obj))
    });
    it.define_method(&proto, "toString", |_it, _this, _args| {
        Ok(Value::str("function () { [native code] }"))
    });
}

// ---- Array -----------------------------------------------------------------

fn install_array(it: &mut Interp) {
    let proto = it.array_proto.clone();

    it.define_method(&proto, "push", |it, this, args| {
        let len = with_array(&this, |e| {
            e.extend_from_slice(args);
            e.len()
        });
        match len {
            Some(n) => Ok(Value::Num(n as f64)),
            None => it.throw_type("Array.prototype.push on non-array"),
        }
    });
    it.define_method(&proto, "pop", |_it, this, _args| {
        Ok(with_array(&this, |e| e.pop()).flatten().unwrap_or(Value::Undefined))
    });
    it.define_method(&proto, "shift", |_it, this, _args| {
        Ok(with_array(&this, |e| if e.is_empty() { None } else { Some(e.remove(0)) })
            .flatten()
            .unwrap_or(Value::Undefined))
    });
    it.define_method(&proto, "unshift", |_it, this, args| {
        let n = with_array(&this, |e| {
            for (i, a) in args.iter().enumerate() {
                e.insert(i, a.clone());
            }
            e.len()
        });
        Ok(Value::Num(n.unwrap_or(0) as f64))
    });
    it.define_method(&proto, "slice", |it, this, args| {
        let e = array_snapshot(&this);
        let (start, end) = slice_bounds(&e, args);
        Ok(it.new_array(e[start..end].to_vec()))
    });
    it.define_method(&proto, "concat", |it, this, args| {
        let mut out = array_snapshot(&this);
        for a in args {
            match a {
                Value::Object(o) if matches!(o.borrow().kind, ObjKind::Array(_)) => {
                    out.extend(array_snapshot(a));
                }
                other => out.push(other.clone()),
            }
        }
        Ok(it.new_array(out))
    });
    it.define_method(&proto, "join", |it, this, args| {
        let sep = match arg(args, 0) {
            Value::Undefined => ",".to_string(),
            v => it.to_string_v(&v)?,
        };
        let e = array_snapshot(&this);
        let mut parts = Vec::with_capacity(e.len());
        for v in &e {
            parts.push(if v.is_nullish() {
                String::new()
            } else {
                it.to_string_v(v)?
            });
        }
        Ok(Value::str(parts.join(&sep)))
    });
    it.define_method(&proto, "toString", |it, this, _args| {
        let e = array_snapshot(&this);
        let mut parts = Vec::with_capacity(e.len());
        for v in &e {
            parts.push(if v.is_nullish() {
                String::new()
            } else {
                it.to_string_v(v)?
            });
        }
        Ok(Value::str(parts.join(",")))
    });
    it.define_method(&proto, "indexOf", |it, this, args| {
        let target = arg(args, 0);
        let e = array_snapshot(&this);
        for (i, v) in e.iter().enumerate() {
            if strict_eq(v, &target) {
                return Ok(Value::Num(i as f64));
            }
        }
        let _ = it;
        Ok(Value::Num(-1.0))
    });
    it.define_method(&proto, "includes", |_it, this, args| {
        let target = arg(args, 0);
        Ok(Value::Bool(array_snapshot(&this).iter().any(|v| strict_eq(v, &target))))
    });
    it.define_method(&proto, "at", |_it, this, args| {
        let e = array_snapshot(&this);
        let n = match arg(args, 0) {
            Value::Num(n) => n as i64,
            _ => 0,
        };
        let idx = if n < 0 { e.len() as i64 + n } else { n };
        Ok(if idx >= 0 && (idx as usize) < e.len() {
            e[idx as usize].clone()
        } else {
            Value::Undefined
        })
    });
    it.define_method(&proto, "reverse", |_it, this, _args| {
        with_array(&this, |e| e.reverse());
        Ok(this)
    });
    it.define_method(&proto, "fill", |_it, this, args| {
        let v = arg(args, 0);
        with_array(&this, |e| {
            for slot in e.iter_mut() {
                *slot = v.clone();
            }
        });
        Ok(this)
    });
    it.define_method(&proto, "forEach", |it, this, args| {
        let cb = arg(args, 0);
        let e = array_snapshot(&this);
        for (i, v) in e.iter().enumerate() {
            it.call(&cb, Value::Undefined, &[v.clone(), Value::Num(i as f64), this.clone()])?;
        }
        Ok(Value::Undefined)
    });
    it.define_method(&proto, "map", |it, this, args| {
        let cb = arg(args, 0);
        let e = array_snapshot(&this);
        let mut out = Vec::with_capacity(e.len());
        for (i, v) in e.iter().enumerate() {
            out.push(it.call(&cb, Value::Undefined, &[v.clone(), Value::Num(i as f64), this.clone()])?);
        }
        Ok(it.new_array(out))
    });
    it.define_method(&proto, "filter", |it, this, args| {
        let cb = arg(args, 0);
        let e = array_snapshot(&this);
        let mut out = Vec::new();
        for (i, v) in e.iter().enumerate() {
            let keep = it.call(&cb, Value::Undefined, &[v.clone(), Value::Num(i as f64), this.clone()])?;
            if to_boolean(&keep) {
                out.push(v.clone());
            }
        }
        Ok(it.new_array(out))
    });
    it.define_method(&proto, "find", |it, this, args| {
        let cb = arg(args, 0);
        let e = array_snapshot(&this);
        for (i, v) in e.iter().enumerate() {
            let hit = it.call(&cb, Value::Undefined, &[v.clone(), Value::Num(i as f64), this.clone()])?;
            if to_boolean(&hit) {
                return Ok(v.clone());
            }
        }
        Ok(Value::Undefined)
    });
    it.define_method(&proto, "findIndex", |it, this, args| {
        let cb = arg(args, 0);
        let e = array_snapshot(&this);
        for (i, v) in e.iter().enumerate() {
            let hit = it.call(&cb, Value::Undefined, &[v.clone(), Value::Num(i as f64), this.clone()])?;
            if to_boolean(&hit) {
                return Ok(Value::Num(i as f64));
            }
        }
        Ok(Value::Num(-1.0))
    });
    it.define_method(&proto, "some", |it, this, args| {
        let cb = arg(args, 0);
        let e = array_snapshot(&this);
        for (i, v) in e.iter().enumerate() {
            let hit = it.call(&cb, Value::Undefined, &[v.clone(), Value::Num(i as f64), this.clone()])?;
            if to_boolean(&hit) {
                return Ok(Value::Bool(true));
            }
        }
        Ok(Value::Bool(false))
    });
    it.define_method(&proto, "every", |it, this, args| {
        let cb = arg(args, 0);
        let e = array_snapshot(&this);
        for (i, v) in e.iter().enumerate() {
            let hit = it.call(&cb, Value::Undefined, &[v.clone(), Value::Num(i as f64), this.clone()])?;
            if !to_boolean(&hit) {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    it.define_method(&proto, "reduce", |it, this, args| {
        let cb = arg(args, 0);
        let e = array_snapshot(&this);
        let mut idx = 0;
        let mut acc = if args.len() >= 2 {
            arg(args, 1)
        } else {
            if e.is_empty() {
                return it.throw_type("Reduce of empty array with no initial value");
            }
            idx = 1;
            e[0].clone()
        };
        while idx < e.len() {
            acc = it.call(
                &cb,
                Value::Undefined,
                &[acc, e[idx].clone(), Value::Num(idx as f64), this.clone()],
            )?;
            idx += 1;
        }
        Ok(acc)
    });
    it.define_method(&proto, "flat", |it, this, _args| {
        let mut out = Vec::new();
        for v in array_snapshot(&this) {
            match &v {
                Value::Object(o) if matches!(o.borrow().kind, ObjKind::Array(_)) => {
                    out.extend(array_snapshot(&v));
                }
                _ => out.push(v),
            }
        }
        Ok(it.new_array(out))
    });
    it.define_method(&proto, "sort", |it, this, args| {
        let cmp = arg(args, 0);
        let mut e = array_snapshot(&this);
        // Insertion sort (stable) so we can call a user comparator safely.
        for i in 1..e.len() {
            let mut j = i;
            while j > 0 {
                let order = if cmp.is_callable() {
                    let r = it.call(&cmp, Value::Undefined, &[e[j - 1].clone(), e[j].clone()])?;
                    it.to_number(&r)?
                } else {
                    let a = it.to_string_v(&e[j - 1])?;
                    let b = it.to_string_v(&e[j])?;
                    if a <= b {
                        -1.0
                    } else {
                        1.0
                    }
                };
                if order > 0.0 {
                    e.swap(j - 1, j);
                    j -= 1;
                } else {
                    break;
                }
            }
        }
        with_array(&this, |slot| *slot = e);
        Ok(this)
    });

    let ctor = make_ctor(it, "Array", array_construct, &proto);
    if let Value::Object(c) = &ctor {
        let cc = c.clone();
        it.define_method(&cc, "isArray", |_it, _this, args| {
            Ok(Value::Bool(matches!(
                arg(args, 0),
                Value::Object(o) if matches!(o.borrow().kind, ObjKind::Array(_))
            )))
        });
        it.define_method(&cc, "of", |it, _this, args| Ok(it.new_array(args.to_vec())));
        it.define_method(&cc, "from", |it, _this, args| {
            let items = it.iterate(&arg(args, 0))?;
            let mapfn = arg(args, 1);
            if mapfn.is_callable() {
                let mut out = Vec::with_capacity(items.len());
                for (i, v) in items.iter().enumerate() {
                    out.push(it.call(&mapfn, Value::Undefined, &[v.clone(), Value::Num(i as f64)])?);
                }
                Ok(it.new_array(out))
            } else {
                Ok(it.new_array(items))
            }
        });
    }
    set_global(it, "Array", ctor);
}

fn array_construct(it: &mut Interp, _this: Value, args: &[Value]) -> Eval<Value> {
    if args.len() == 1 {
        if let Value::Num(n) = args[0] {
            return Ok(it.new_array(vec![Value::Undefined; n.max(0.0) as usize]));
        }
    }
    Ok(it.new_array(args.to_vec()))
}

fn slice_bounds(e: &[Value], args: &[Value]) -> (usize, usize) {
    let len = e.len() as i64;
    let norm = |v: &Value, default: i64| -> i64 {
        match v {
            Value::Undefined => default,
            Value::Num(n) => {
                let n = *n as i64;
                if n < 0 {
                    (len + n).max(0)
                } else {
                    n.min(len)
                }
            }
            _ => default,
        }
    };
    let start = norm(&arg(args, 0), 0);
    let end = norm(&arg(args, 1), len);
    (start as usize, end.max(start) as usize)
}

// ---- String ----------------------------------------------------------------

fn install_string(it: &mut Interp) {
    let proto = it.string_proto.clone();

    macro_rules! smethod {
        ($name:expr, $f:expr) => {
            it.define_method(&proto, $name, $f);
        };
    }

    smethod!("toString", |it, this, _a| Ok(Value::str(it.to_string_v(&this)?)));
    smethod!("valueOf", |it, this, _a| Ok(Value::str(it.to_string_v(&this)?)));
    smethod!("charAt", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let i = it.to_number(&arg(a, 0))? as usize;
        Ok(Value::str(s.chars().nth(i).map(|c| c.to_string()).unwrap_or_default()))
    });
    smethod!("charCodeAt", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let i = it.to_number(&arg(a, 0))? as usize;
        Ok(match s.chars().nth(i) {
            Some(c) => Value::Num(c as u32 as f64),
            None => Value::Num(f64::NAN),
        })
    });
    smethod!("codePointAt", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let i = it.to_number(&arg(a, 0))? as usize;
        Ok(match s.chars().nth(i) {
            Some(c) => Value::Num(c as u32 as f64),
            None => Value::Undefined,
        })
    });
    smethod!("at", |it, this, a| {
        let chars: Vec<char> = it.to_string_v(&this)?.chars().collect();
        let n = it.to_number(&arg(a, 0))? as i64;
        let idx = if n < 0 { chars.len() as i64 + n } else { n };
        Ok(if idx >= 0 && (idx as usize) < chars.len() {
            Value::str(chars[idx as usize].to_string())
        } else {
            Value::Undefined
        })
    });
    smethod!("indexOf", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let needle = it.to_string_v(&arg(a, 0))?;
        Ok(Value::Num(match s.find(&needle) {
            Some(byte) => s[..byte].chars().count() as f64,
            None => -1.0,
        }))
    });
    smethod!("lastIndexOf", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let needle = it.to_string_v(&arg(a, 0))?;
        Ok(Value::Num(match s.rfind(&needle) {
            Some(byte) => s[..byte].chars().count() as f64,
            None => -1.0,
        }))
    });
    smethod!("includes", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let needle = it.to_string_v(&arg(a, 0))?;
        Ok(Value::Bool(s.contains(&needle)))
    });
    smethod!("startsWith", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let needle = it.to_string_v(&arg(a, 0))?;
        Ok(Value::Bool(s.starts_with(&needle)))
    });
    smethod!("endsWith", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let needle = it.to_string_v(&arg(a, 0))?;
        Ok(Value::Bool(s.ends_with(&needle)))
    });
    smethod!("toUpperCase", |it, this, _a| Ok(Value::str(it.to_string_v(&this)?.to_uppercase())));
    smethod!("toLowerCase", |it, this, _a| Ok(Value::str(it.to_string_v(&this)?.to_lowercase())));
    smethod!("trim", |it, this, _a| Ok(Value::str(it.to_string_v(&this)?.trim().to_string())));
    smethod!("trimStart", |it, this, _a| Ok(Value::str(it.to_string_v(&this)?.trim_start().to_string())));
    smethod!("trimEnd", |it, this, _a| Ok(Value::str(it.to_string_v(&this)?.trim_end().to_string())));
    smethod!("slice", |it, this, a| {
        let chars: Vec<char> = it.to_string_v(&this)?.chars().collect();
        let (start, end) = str_slice_bounds(chars.len(), it.to_number(&arg(a, 0))?, &arg(a, 1), it)?;
        Ok(Value::str(chars[start..end].iter().collect::<String>()))
    });
    smethod!("substring", |it, this, a| {
        let chars: Vec<char> = it.to_string_v(&this)?.chars().collect();
        let len = chars.len() as i64;
        let clamp = |n: f64| (n as i64).clamp(0, len);
        let mut s = clamp(it.to_number(&arg(a, 0))?);
        let mut e = match arg(a, 1) {
            Value::Undefined => len,
            v => clamp(it.to_number(&v)?),
        };
        if s > e {
            std::mem::swap(&mut s, &mut e);
        }
        Ok(Value::str(chars[s as usize..e as usize].iter().collect::<String>()))
    });
    smethod!("split", |it, this, a| {
        let s = it.to_string_v(&this)?;
        match arg(a, 0) {
            Value::Undefined => Ok(it.new_array(vec![Value::str(s)])),
            sep => {
                let sep = it.to_string_v(&sep)?;
                let parts: Vec<Value> = if sep.is_empty() {
                    s.chars().map(|c| Value::str(c.to_string())).collect()
                } else {
                    s.split(&sep).map(Value::str).collect()
                };
                Ok(it.new_array(parts))
            }
        }
    });
    smethod!("replace", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let from = it.to_string_v(&arg(a, 0))?;
        let to = it.to_string_v(&arg(a, 1))?;
        Ok(Value::str(s.replacen(&from, &to, 1)))
    });
    smethod!("replaceAll", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let from = it.to_string_v(&arg(a, 0))?;
        let to = it.to_string_v(&arg(a, 1))?;
        Ok(Value::str(s.replace(&from, &to)))
    });
    smethod!("repeat", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let n = it.to_number(&arg(a, 0))?;
        if n < 0.0 || !n.is_finite() {
            return it.throw_range("Invalid count value");
        }
        Ok(Value::str(s.repeat(n as usize)))
    });
    smethod!("padStart", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let target = it.to_number(&arg(a, 0))? as usize;
        let pad = match arg(a, 1) {
            Value::Undefined => " ".to_string(),
            v => it.to_string_v(&v)?,
        };
        Ok(Value::str(pad_string(&s, target, &pad, true)))
    });
    smethod!("padEnd", |it, this, a| {
        let s = it.to_string_v(&this)?;
        let target = it.to_number(&arg(a, 0))? as usize;
        let pad = match arg(a, 1) {
            Value::Undefined => " ".to_string(),
            v => it.to_string_v(&v)?,
        };
        Ok(Value::str(pad_string(&s, target, &pad, false)))
    });
    smethod!("concat", |it, this, a| {
        let mut s = it.to_string_v(&this)?;
        for v in a {
            s.push_str(&it.to_string_v(v)?);
        }
        Ok(Value::str(s))
    });

    let ctor = make_ctor(it, "String", |it, _this, args| {
        Ok(Value::str(match args.first() {
            Some(v) => it.to_string_v(v)?,
            None => String::new(),
        }))
    }, &proto);
    if let Value::Object(c) = &ctor {
        let cc = c.clone();
        it.define_method(&cc, "fromCharCode", |it, _this, args| {
            let mut s = String::new();
            for v in args {
                let n = it.to_number(v)? as u32;
                if let Some(c) = char::from_u32(n) {
                    s.push(c);
                }
            }
            Ok(Value::str(s))
        });
    }
    set_global(it, "String", ctor);
}

fn str_slice_bounds(len: usize, start: f64, end: &Value, it: &mut Interp) -> Eval<(usize, usize)> {
    let len = len as i64;
    let norm = |n: f64| -> i64 {
        let n = n as i64;
        if n < 0 {
            (len + n).max(0)
        } else {
            n.min(len)
        }
    };
    let s = norm(start);
    let e = match end {
        Value::Undefined => len,
        v => norm(it.to_number(v)?),
    };
    Ok((s as usize, e.max(s) as usize))
}

fn pad_string(s: &str, target: usize, pad: &str, at_start: bool) -> String {
    let cur = s.chars().count();
    if cur >= target || pad.is_empty() {
        return s.to_string();
    }
    let need = target - cur;
    let filler: String = pad.chars().cycle().take(need).collect();
    if at_start {
        format!("{filler}{s}")
    } else {
        format!("{s}{filler}")
    }
}

// ---- Number / Boolean ------------------------------------------------------

fn install_number_boolean(it: &mut Interp) {
    let proto = it.number_proto.clone();
    it.define_method(&proto, "toString", |it, this, a| {
        let n = it.to_number(&this)?;
        match arg(a, 0) {
            Value::Undefined => Ok(Value::str(num_to_str(n))),
            radix => {
                let r = it.to_number(&radix)? as u32;
                Ok(Value::str(int_to_radix(n, r)))
            }
        }
    });
    it.define_method(&proto, "valueOf", |it, this, _a| Ok(Value::Num(it.to_number(&this)?)));
    it.define_method(&proto, "toFixed", |it, this, a| {
        let n = it.to_number(&this)?;
        let digits = it.to_number(&arg(a, 0))? as usize;
        Ok(Value::str(format!("{n:.digits$}")))
    });

    let ctor = make_ctor(it, "Number", |it, _this, args| {
        Ok(Value::Num(match args.first() {
            Some(v) => it.to_number(v)?,
            None => 0.0,
        }))
    }, &proto);
    if let Value::Object(c) = &ctor {
        let cc = c.clone();
        cc.borrow_mut().set_data("MAX_SAFE_INTEGER", Value::Num(9_007_199_254_740_991.0));
        cc.borrow_mut().set_data("MIN_SAFE_INTEGER", Value::Num(-9_007_199_254_740_991.0));
        cc.borrow_mut().set_data("MAX_VALUE", Value::Num(f64::MAX));
        cc.borrow_mut().set_data("MIN_VALUE", Value::Num(f64::MIN_POSITIVE));
        cc.borrow_mut().set_data("EPSILON", Value::Num(f64::EPSILON));
        cc.borrow_mut().set_data("POSITIVE_INFINITY", Value::Num(f64::INFINITY));
        cc.borrow_mut().set_data("NEGATIVE_INFINITY", Value::Num(f64::NEG_INFINITY));
        cc.borrow_mut().set_data("NaN", Value::Num(f64::NAN));
        it.define_method(&cc, "isInteger", |_it, _this, a| {
            Ok(Value::Bool(matches!(arg(a, 0), Value::Num(n) if n.is_finite() && n.fract() == 0.0)))
        });
        it.define_method(&cc, "isFinite", |_it, _this, a| {
            Ok(Value::Bool(matches!(arg(a, 0), Value::Num(n) if n.is_finite())))
        });
        it.define_method(&cc, "isNaN", |_it, _this, a| {
            Ok(Value::Bool(matches!(arg(a, 0), Value::Num(n) if n.is_nan())))
        });
        it.define_method(&cc, "parseFloat", global_parse_float);
        it.define_method(&cc, "parseInt", global_parse_int);
    }
    set_global(it, "Number", ctor);

    let bproto = it.boolean_proto.clone();
    it.define_method(&bproto, "toString", |_it, this, _a| {
        Ok(Value::str(if to_boolean(&this) { "true" } else { "false" }))
    });
    it.define_method(&bproto, "valueOf", |_it, this, _a| Ok(Value::Bool(to_boolean(&this))));
    let bctor = make_ctor(it, "Boolean", |_it, _this, a| Ok(Value::Bool(to_boolean(&arg(a, 0)))), &bproto);
    set_global(it, "Boolean", bctor);
}

fn int_to_radix(n: f64, radix: u32) -> String {
    if radix == 10 || !(2..=36).contains(&radix) {
        return num_to_str(n);
    }
    if n == 0.0 {
        return "0".to_string();
    }
    let neg = n < 0.0;
    let mut v = n.abs().trunc() as u64;
    let mut digits = Vec::new();
    while v > 0 {
        let d = (v % radix as u64) as u32;
        digits.push(std::char::from_digit(d, radix).unwrap());
        v /= radix as u64;
    }
    if neg {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

// ---- Error -----------------------------------------------------------------

fn install_errors(it: &mut Interp) {
    let proto = it.error_proto.clone();
    proto.borrow_mut().set_data("name", Value::str("Error"));
    proto.borrow_mut().set_data("message", Value::str(""));
    it.define_method(&proto, "toString", |it, this, _a| {
        let name_v = it.get_member(&this, "name")?;
        let name = it.to_string_v(&name_v)?;
        let msg_v = it.get_member(&this, "message")?;
        let msg = it.to_string_v(&msg_v)?;
        Ok(Value::str(if msg.is_empty() {
            name
        } else {
            format!("{name}: {msg}")
        }))
    });

    for name in ["Error", "TypeError", "RangeError", "SyntaxError", "ReferenceError"] {
        let ctor = it.native_fn(name, error_construct);
        if let Value::Object(c) = &ctor {
            c.borrow_mut().set_data("prototype", Value::Object(proto.clone()));
            // Carry the constructor's own name so `error_construct` can read it.
            c.borrow_mut().set_data("name", Value::str(name));
        }
        set_global(it, name, ctor);
    }
}

fn error_construct(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    // `name` resolves from the callee when invoked as a constructor; default to
    // a generic Error otherwise.
    let msg = match args.first() {
        Some(Value::Undefined) | None => String::new(),
        Some(v) => it.to_string_v(v)?,
    };
    // When called via `new`, `this` is a fresh object with the Error prototype.
    if let Value::Object(o) = &this {
        let mut b = o.borrow_mut();
        b.class = "Error";
        b.set_data("message", Value::str(msg));
        return Ok(Value::Undefined); // `new` keeps `this`
    }
    Ok(it.make_error("Error", msg))
}

// ---- Math ------------------------------------------------------------------

fn install_math(it: &mut Interp) {
    let math = it.new_object(Some(it.object_proto.clone()));
    {
        let mut m = math.borrow_mut();
        m.set_data("PI", Value::Num(std::f64::consts::PI));
        m.set_data("E", Value::Num(std::f64::consts::E));
        m.set_data("LN2", Value::Num(std::f64::consts::LN_2));
        m.set_data("LN10", Value::Num(std::f64::consts::LN_10));
        m.set_data("LOG2E", Value::Num(std::f64::consts::LOG2_E));
        m.set_data("LOG10E", Value::Num(std::f64::consts::LOG10_E));
        m.set_data("SQRT2", Value::Num(std::f64::consts::SQRT_2));
        m.set_data("SQRT1_2", Value::Num(std::f64::consts::FRAC_1_SQRT_2));
    }
    macro_rules! unary {
        ($name:expr, $f:expr) => {
            it.define_method(&math, $name, |it, _t, a| {
                let x = it.to_number(&arg(a, 0))?;
                Ok(Value::Num(($f)(x)))
            });
        };
    }
    unary!("abs", f64::abs);
    unary!("floor", f64::floor);
    unary!("ceil", f64::ceil);
    unary!("trunc", f64::trunc);
    unary!("sign", f64::signum);
    unary!("sqrt", f64::sqrt);
    unary!("cbrt", f64::cbrt);
    unary!("exp", f64::exp);
    unary!("log", f64::ln);
    unary!("log2", f64::log2);
    unary!("log10", f64::log10);
    unary!("sin", f64::sin);
    unary!("cos", f64::cos);
    unary!("tan", f64::tan);
    unary!("asin", f64::asin);
    unary!("acos", f64::acos);
    unary!("atan", f64::atan);
    // `round` ties go toward +∞ in JS (not Rust's round-half-away-from-zero).
    it.define_method(&math, "round", |it, _t, a| {
        let x = it.to_number(&arg(a, 0))?;
        Ok(Value::Num((x + 0.5).floor()))
    });
    it.define_method(&math, "pow", |it, _t, a| {
        let x = it.to_number(&arg(a, 0))?;
        let y = it.to_number(&arg(a, 1))?;
        Ok(Value::Num(x.powf(y)))
    });
    it.define_method(&math, "atan2", |it, _t, a| {
        let y = it.to_number(&arg(a, 0))?;
        let x = it.to_number(&arg(a, 1))?;
        Ok(Value::Num(y.atan2(x)))
    });
    it.define_method(&math, "hypot", |it, _t, a| {
        let mut sum = 0.0;
        for v in a {
            let n = it.to_number(v)?;
            sum += n * n;
        }
        Ok(Value::Num(sum.sqrt()))
    });
    it.define_method(&math, "max", |it, _t, a| {
        let mut m = f64::NEG_INFINITY;
        for v in a {
            let n = it.to_number(v)?;
            if n.is_nan() {
                return Ok(Value::Num(f64::NAN));
            }
            if n > m {
                m = n;
            }
        }
        Ok(Value::Num(m))
    });
    it.define_method(&math, "min", |it, _t, a| {
        let mut m = f64::INFINITY;
        for v in a {
            let n = it.to_number(v)?;
            if n.is_nan() {
                return Ok(Value::Num(f64::NAN));
            }
            if n < m {
                m = n;
            }
        }
        Ok(Value::Num(m))
    });
    it.define_method(&math, "random", |it, _t, _a| Ok(Value::Num(it.next_random())));
    set_global(it, "Math", Value::Object(math));
}

// ---- JSON ------------------------------------------------------------------

fn install_json(it: &mut Interp) {
    let json = it.new_object(Some(it.object_proto.clone()));
    it.define_method(&json, "stringify", |it, _t, a| {
        let indent = match arg(a, 2) {
            Value::Num(n) => " ".repeat((n.max(0.0) as usize).min(10)),
            Value::Str(s) => s.to_string(),
            _ => String::new(),
        };
        Ok(match json_stringify(it, &arg(a, 0), &indent, 0)? {
            Some(s) => Value::str(s),
            None => Value::Undefined,
        })
    });
    it.define_method(&json, "parse", |it, _t, a| {
        let src = it.to_string_v(&arg(a, 0))?;
        json_parse(it, &src)
    });
    set_global(it, "JSON", Value::Object(json));
}

fn json_stringify(it: &mut Interp, v: &Value, indent: &str, depth: usize) -> Eval<Option<String>> {
    Ok(match v {
        Value::Undefined => None,
        Value::Null => Some("null".to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Num(n) => Some(if n.is_finite() { num_to_str(*n) } else { "null".to_string() }),
        Value::Str(s) => Some(json_quote(s)),
        Value::Object(o) => {
            if matches!(o.borrow().kind, ObjKind::Function(_)) {
                return Ok(None);
            }
            let is_array = matches!(o.borrow().kind, ObjKind::Array(_));
            let (nl, pad, pad_in) = if indent.is_empty() {
                (String::new(), String::new(), String::new())
            } else {
                (
                    "\n".to_string(),
                    indent.repeat(depth),
                    indent.repeat(depth + 1),
                )
            };
            if is_array {
                let elems = array_snapshot(v);
                if elems.is_empty() {
                    return Ok(Some("[]".to_string()));
                }
                let mut parts = Vec::new();
                for e in &elems {
                    let s = json_stringify(it, e, indent, depth + 1)?.unwrap_or_else(|| "null".to_string());
                    parts.push(format!("{pad_in}{s}"));
                }
                Some(format!("[{nl}{}{nl}{pad}]", parts.join(&format!(",{nl}"))))
            } else {
                let keys = it.enum_keys(v);
                let mut parts = Vec::new();
                for k in keys {
                    let val = it.get_member(v, &k)?;
                    if let Some(s) = json_stringify(it, &val, indent, depth + 1)? {
                        let sep = if indent.is_empty() { ":" } else { ": " };
                        parts.push(format!("{pad_in}{}{sep}{s}", json_quote(&k)));
                    }
                }
                if parts.is_empty() {
                    return Ok(Some("{}".to_string()));
                }
                Some(format!("{{{nl}{}{nl}{pad}}}", parts.join(&format!(",{nl}"))))
            }
        }
    })
}

fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_parse(it: &mut Interp, src: &str) -> Eval<Value> {
    let chars: Vec<char> = src.chars().collect();
    let mut p = JsonParser { it, chars: &chars, pos: 0 };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return p.it.throw_type("Unexpected token in JSON");
    }
    Ok(v)
}

struct JsonParser<'a> {
    it: &'a mut Interp,
    chars: &'a [char],
    pos: usize,
}

impl JsonParser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }
    fn value(&mut self) -> Eval<Value> {
        self.skip_ws();
        match self.peek() {
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('"') => Ok(Value::str(self.string()?)),
            Some('t') => self.lit("true", Value::Bool(true)),
            Some('f') => self.lit("false", Value::Bool(false)),
            Some('n') => self.lit("null", Value::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            _ => self.it.throw_type("Unexpected token in JSON"),
        }
    }
    fn lit(&mut self, word: &str, val: Value) -> Eval<Value> {
        for ch in word.chars() {
            if self.peek() != Some(ch) {
                return self.it.throw_type("Unexpected token in JSON");
            }
            self.pos += 1;
        }
        Ok(val)
    }
    fn number(&mut self) -> Eval<Value> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-')
        {
            self.pos += 1;
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        s.parse::<f64>()
            .map(Value::Num)
            .map_err(|_| self.it.throw_type::<()>("Invalid JSON number").unwrap_err())
    }
    fn string(&mut self) -> Eval<String> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return self.it.throw_type("Unterminated JSON string"),
                Some('"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some('\\') => {
                    self.pos += 1;
                    let c = self.peek().unwrap_or('"');
                    self.pos += 1;
                    match c {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{C}'),
                        'u' => {
                            let mut v: u32 = 0;
                            for _ in 0..4 {
                                if let Some(h) = self.peek().and_then(|c| c.to_digit(16)) {
                                    v = v * 16 + h;
                                    self.pos += 1;
                                }
                            }
                            if let Some(ch) = char::from_u32(v) {
                                out.push(ch);
                            }
                        }
                        other => out.push(other),
                    }
                }
                Some(c) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }
    fn array(&mut self) -> Eval<Value> {
        self.pos += 1; // [
        let mut elems = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(self.it.new_array(elems));
        }
        loop {
            elems.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                _ => return self.it.throw_type("Unexpected token in JSON array"),
            }
        }
        Ok(self.it.new_array(elems))
    }
    fn object(&mut self) -> Eval<Value> {
        self.pos += 1; // {
        let obj = self.it.new_object(Some(self.it.object_proto.clone()));
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Value::Object(obj));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some('"') {
                return self.it.throw_type("Expected JSON property name");
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(':') {
                return self.it.throw_type("Expected ':' in JSON object");
            }
            self.pos += 1;
            let val = self.value()?;
            obj.borrow_mut().set_data(&key, val);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                _ => return self.it.throw_type("Unexpected token in JSON object"),
            }
        }
        Ok(Value::Object(obj))
    }
}

// ---- console ---------------------------------------------------------------

fn install_console(it: &mut Interp) {
    let console = it.new_object(Some(it.object_proto.clone()));
    for name in ["log", "info", "warn", "error", "debug"] {
        it.define_method(&console, name, console_write);
    }
    set_global(it, "console", Value::Object(console));
}

fn console_write(it: &mut Interp, _this: Value, args: &[Value]) -> Eval<Value> {
    let mut parts = Vec::with_capacity(args.len());
    for a in args {
        parts.push(it.to_string_v(a)?);
    }
    it.output.push(parts.join(" "));
    Ok(Value::Undefined)
}

// ---- global functions ------------------------------------------------------

fn install_globals(it: &mut Interp) {
    set_global(it, "NaN", Value::Num(f64::NAN));
    set_global(it, "Infinity", Value::Num(f64::INFINITY));
    set_global(it, "globalThis", Value::Object(it.global.clone()));

    let pf = it.native_fn("parseFloat", global_parse_float);
    set_global(it, "parseFloat", pf);
    let pi = it.native_fn("parseInt", global_parse_int);
    set_global(it, "parseInt", pi);
    let isnan = it.native_fn("isNaN", |it, _t, a| {
        Ok(Value::Bool(it.to_number(&arg(a, 0))?.is_nan()))
    });
    set_global(it, "isNaN", isnan);
    let isfin = it.native_fn("isFinite", |it, _t, a| {
        Ok(Value::Bool(it.to_number(&arg(a, 0))?.is_finite()))
    });
    set_global(it, "isFinite", isfin);

    let set_timeout = it.native_fn("setTimeout", |it, _t, a| {
        let cb = arg(a, 0);
        let delay = match arg(a, 1) {
            Value::Undefined => 0.0,
            v => it.to_number(&v)?.max(0.0),
        };
        let extra = if a.len() > 2 { a[2..].to_vec() } else { Vec::new() };
        if cb.is_callable() {
            it.timers.push((delay, cb, extra));
        }
        Ok(Value::Num(it.timers.len() as f64))
    });
    set_global(it, "setTimeout", set_timeout.clone());
    set_global(it, "setInterval", set_timeout); // runs once (no real loop)
    let clear = it.native_fn("clearTimeout", |_it, _t, _a| Ok(Value::Undefined));
    set_global(it, "clearTimeout", clear.clone());
    set_global(it, "clearInterval", clear);
    let qmt = it.native_fn("queueMicrotask", |it, _t, a| {
        let cb = arg(a, 0);
        if cb.is_callable() {
            it.enqueue_microtask(Box::new(move |it| {
                it.call(&cb, Value::Undefined, &[])?;
                Ok(())
            }));
        }
        Ok(Value::Undefined)
    });
    set_global(it, "queueMicrotask", qmt);

    // Real Symbol: unique values (`typeof` → "symbol") usable as property keys.
    // Well-known symbols (`Symbol.iterator`, …) are fixed symbols with canonical
    // internal keys honoured by the iteration protocol.
    let symbol = it.native_fn("Symbol", |it, _t, a| {
        let key = it.next_symbol_key();
        let o = it.new_object(None);
        let desc = match a.first() {
            Some(v) if !v.is_nullish() => it.to_string_v(v)?,
            _ => String::new(),
        };
        {
            let mut b = o.borrow_mut();
            b.class = "Symbol";
            b.set_data("__key", Value::str(key));
            b.set_data("description", Value::str(desc));
        }
        Ok(Value::Object(o))
    });
    if let Value::Object(s) = &symbol {
        for (prop, key) in [
            ("iterator", "@@iterator"),
            ("asyncIterator", "@@asyncIterator"),
            ("hasInstance", "@@hasInstance"),
            ("toPrimitive", "@@toPrimitive"),
            ("toStringTag", "@@toStringTag"),
        ] {
            let wk = well_known_symbol(it, key);
            s.borrow_mut().set_data(prop, wk);
        }
        let registry = it.new_object(Some(it.object_proto.clone()));
        s.borrow_mut().set_data("__registry", Value::Object(registry));
        it.define_method(s, "for", symbol_for);
        it.define_method(s, "keyFor", symbol_key_for);
    }
    set_global(it, "Symbol", symbol);

    // `eval` (indirect, global scope) and the `Function` constructor.
    let eval_fn = it.native_fn("eval", |it, _t, a| match a.first() {
        Some(Value::Str(s)) => it.eval_in_global(s),
        Some(other) => Ok(other.clone()),
        None => Ok(Value::Undefined),
    });
    set_global(it, "eval", eval_fn);
    let function_ctor = it.native_fn("Function", function_construct);
    set_global(it, "Function", function_ctor);
}

fn well_known_symbol(it: &Interp, key: &str) -> Value {
    let o = it.new_object(None);
    {
        let mut b = o.borrow_mut();
        b.class = "Symbol";
        b.set_data("__key", Value::str(key));
        b.set_data(
            "description",
            Value::str(format!("Symbol.{}", key.trim_start_matches("@@"))),
        );
    }
    Value::Object(o)
}

fn symbol_for(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let key = it.to_string_v(&arg(a, 0))?;
    let registry = it.get_member(&this, "__registry")?;
    let existing = it.get_member(&registry, &key)?;
    if matches!(existing, Value::Object(_)) {
        return Ok(existing);
    }
    let sym = it.new_object(None);
    {
        let mut b = sym.borrow_mut();
        b.class = "Symbol";
        b.set_data("__key", Value::str(format!("@@for:{key}")));
        b.set_data("description", Value::str(key.clone()));
    }
    let symv = Value::Object(sym);
    it.set_member(&registry, &key, symv.clone())?;
    Ok(symv)
}

fn symbol_key_for(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let sym = arg(a, 0);
    let registry = it.get_member(&this, "__registry")?;
    for k in it.enum_keys(&registry) {
        let v = it.get_member(&registry, &k)?;
        if strict_eq(&v, &sym) {
            return Ok(Value::str(k));
        }
    }
    Ok(Value::Undefined)
}

fn function_construct(it: &mut Interp, _this: Value, a: &[Value]) -> Eval<Value> {
    if a.is_empty() {
        return it.function_from_strings(&[], "");
    }
    let body = it.to_string_v(a.last().unwrap())?;
    let mut params = Vec::new();
    for v in &a[..a.len() - 1] {
        for p in it.to_string_v(v)?.split(',') {
            let p = p.trim();
            if !p.is_empty() {
                params.push(p.to_string());
            }
        }
    }
    it.function_from_strings(&params, &body)
}

fn global_parse_float(it: &mut Interp, _this: Value, args: &[Value]) -> Eval<Value> {
    let s = it.to_string_v(&arg(args, 0))?;
    let t = s.trim_start();
    let mut end = 0;
    let bytes = t.as_bytes();
    let mut seen_dot = false;
    let mut seen_e = false;
    while end < bytes.len() {
        let c = bytes[end] as char;
        let ok = c.is_ascii_digit()
            || (c == '-' && (end == 0 || (bytes[end - 1] as char) == 'e' || (bytes[end - 1] as char) == 'E'))
            || (c == '+' && (end == 0 || (bytes[end - 1] as char) == 'e' || (bytes[end - 1] as char) == 'E'))
            || (c == '.' && !seen_dot && !seen_e)
            || ((c == 'e' || c == 'E') && !seen_e && end > 0);
        if !ok {
            break;
        }
        if c == '.' {
            seen_dot = true;
        }
        if c == 'e' || c == 'E' {
            seen_e = true;
        }
        end += 1;
    }
    Ok(Value::Num(t[..end].parse::<f64>().unwrap_or(f64::NAN)))
}

fn global_parse_int(it: &mut Interp, _this: Value, args: &[Value]) -> Eval<Value> {
    let s = it.to_string_v(&arg(args, 0))?;
    let t = s.trim();
    let mut radix = match arg(args, 1) {
        Value::Undefined => 0,
        v => it.to_number(&v)? as u32,
    };
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let rest = if (radix == 16 || radix == 0) && (rest.starts_with("0x") || rest.starts_with("0X")) {
        radix = 16;
        &rest[2..]
    } else {
        rest
    };
    if radix == 0 {
        radix = 10;
    }
    if !(2..=36).contains(&radix) {
        return Ok(Value::Num(f64::NAN));
    }
    let mut acc = 0.0_f64;
    let mut any = false;
    for c in rest.chars() {
        match c.to_digit(radix) {
            Some(d) => {
                acc = acc * radix as f64 + d as f64;
                any = true;
            }
            None => break,
        }
    }
    if !any {
        return Ok(Value::Num(f64::NAN));
    }
    Ok(Value::Num(if neg { -acc } else { acc }))
}

// ---- RegExp + regex-aware String methods -----------------------------------

use super::regex::Regex;

/// Extract `(source, flags)` if `v` is a RegExp object.
fn as_regex(v: &Value) -> Option<(String, String)> {
    if let Value::Object(o) = v {
        let b = o.borrow();
        if b.class == "RegExp" {
            let get = |k: &str| match b.get_own(k) {
                Some(PropDesc::Data(Value::Str(s))) => s.to_string(),
                _ => String::new(),
            };
            return Some((get("source"), get("flags")));
        }
    }
    None
}

fn build_match_array(it: &Interp, chars: &[char], m: &super::regex::Match, input: &str) -> Value {
    let mut parts = vec![Value::str(chars[m.start..m.end].iter().collect::<String>())];
    for g in &m.groups {
        parts.push(match g {
            Some((a, b)) => Value::str(chars[*a..*b].iter().collect::<String>()),
            None => Value::Undefined,
        });
    }
    let arr = it.new_array(parts);
    if let Value::Object(o) = &arr {
        o.borrow_mut().set_data("index", Value::Num(m.start as f64));
        o.borrow_mut().set_data("input", Value::str(input));
    }
    arr
}

fn install_regexp(it: &mut Interp) {
    let proto = it.regexp_proto.clone();
    it.define_method(&proto, "test", regexp_test);
    it.define_method(&proto, "exec", regexp_exec);
    it.define_method(&proto, "toString", |it, this, _a| {
        let src_v = it.get_member(&this, "source")?;
        let src = it.to_string_v(&src_v)?;
        let flags_v = it.get_member(&this, "flags")?;
        let flags = it.to_string_v(&flags_v)?;
        Ok(Value::str(format!("/{src}/{flags}")))
    });

    let ctor = it.native_fn("RegExp", regexp_construct);
    if let Value::Object(c) = &ctor {
        c.borrow_mut().set_data("prototype", Value::Object(proto.clone()));
    }
    proto.borrow_mut().set_data("constructor", ctor.clone());
    set_global(it, "RegExp", ctor);

    // Regex-aware String methods (override the literal-string versions).
    let sp = it.string_proto.clone();
    it.define_method(&sp, "search", str_search);
    it.define_method(&sp, "match", str_match);
    it.define_method(&sp, "matchAll", str_match_all);
    it.define_method(&sp, "replace", str_replace);
    it.define_method(&sp, "replaceAll", str_replace_all);
    it.define_method(&sp, "split", str_split);
}

fn regexp_construct(it: &mut Interp, _this: Value, args: &[Value]) -> Eval<Value> {
    let pat = arg(args, 0);
    if let Some((src, f)) = as_regex(&pat) {
        let flags = match arg(args, 1) {
            Value::Undefined => f,
            v => it.to_string_v(&v)?,
        };
        return Ok(it.make_regexp(&src, &flags));
    }
    let source = match pat {
        Value::Undefined => String::new(),
        v => it.to_string_v(&v)?,
    };
    let flags = match arg(args, 1) {
        Value::Undefined => String::new(),
        v => it.to_string_v(&v)?,
    };
    Ok(it.make_regexp(&source, &flags))
}

fn regexp_test(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let (src, flags) = match as_regex(&this) {
        Some(x) => x,
        None => return Ok(Value::Bool(false)),
    };
    let s = it.to_string_v(&arg(args, 0))?;
    let re = match Regex::new(&src, &flags) {
        Ok(r) => r,
        Err(_) => return Ok(Value::Bool(false)),
    };
    let chars: Vec<char> = s.chars().collect();
    let stateful = re.global || re.sticky;
    let start = if stateful {
        let li = it.get_member(&this, "lastIndex")?;
        it.to_number(&li)? as usize
    } else {
        0
    };
    match re.exec(&chars, start.min(chars.len())) {
        Some(m) => {
            if stateful {
                it.set_member(&this, "lastIndex", Value::Num(m.end as f64))?;
            }
            Ok(Value::Bool(true))
        }
        None => {
            if stateful {
                it.set_member(&this, "lastIndex", Value::Num(0.0))?;
            }
            Ok(Value::Bool(false))
        }
    }
}

fn regexp_exec(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let (src, flags) = match as_regex(&this) {
        Some(x) => x,
        None => return Ok(Value::Null),
    };
    let s = it.to_string_v(&arg(args, 0))?;
    let re = match Regex::new(&src, &flags) {
        Ok(r) => r,
        Err(_) => return Ok(Value::Null),
    };
    let chars: Vec<char> = s.chars().collect();
    let stateful = re.global || re.sticky;
    let start = if stateful {
        let li = it.get_member(&this, "lastIndex")?;
        it.to_number(&li)? as usize
    } else {
        0
    };
    match re.exec(&chars, start.min(chars.len())) {
        Some(m) => {
            if stateful {
                let next = if m.end == m.start { m.end + 1 } else { m.end };
                it.set_member(&this, "lastIndex", Value::Num(next as f64))?;
            }
            Ok(build_match_array(it, &chars, &m, &s))
        }
        None => {
            if stateful {
                it.set_member(&this, "lastIndex", Value::Num(0.0))?;
            }
            Ok(Value::Null)
        }
    }
}

/// Compile a regex from either a RegExp value or a plain pattern string.
fn regex_from(it: &mut Interp, v: &Value) -> Eval<Option<Regex>> {
    let (src, flags) = match as_regex(v) {
        Some(x) => x,
        None => (it.to_string_v(v)?, String::new()),
    };
    Ok(Regex::new(&src, &flags).ok())
}

fn str_search(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let s = it.to_string_v(&this)?;
    let re = match regex_from(it, &arg(a, 0))? {
        Some(r) => r,
        None => return Ok(Value::Num(-1.0)),
    };
    let chars: Vec<char> = s.chars().collect();
    Ok(Value::Num(re.exec(&chars, 0).map(|m| m.start as f64).unwrap_or(-1.0)))
}

fn str_match(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let s = it.to_string_v(&this)?;
    let (src, flags) = match as_regex(&arg(a, 0)) {
        Some(x) => x,
        None => (it.to_string_v(&arg(a, 0))?, String::new()),
    };
    let re = match Regex::new(&src, &flags) {
        Ok(r) => r,
        Err(_) => return Ok(Value::Null),
    };
    let chars: Vec<char> = s.chars().collect();
    if re.global {
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some(m) = re.exec(&chars, pos) {
            out.push(Value::str(chars[m.start..m.end].iter().collect::<String>()));
            pos = if m.end == m.start { m.end + 1 } else { m.end };
        }
        if out.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(it.new_array(out))
        }
    } else {
        match re.exec(&chars, 0) {
            Some(m) => Ok(build_match_array(it, &chars, &m, &s)),
            None => Ok(Value::Null),
        }
    }
}

fn str_match_all(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let s = it.to_string_v(&this)?;
    let re = match regex_from(it, &arg(a, 0))? {
        Some(r) => r,
        None => return Ok(it.new_array(Vec::new())),
    };
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(m) = re.exec(&chars, pos) {
        out.push(build_match_array(it, &chars, &m, &s));
        pos = if m.end == m.start { m.end + 1 } else { m.end };
    }
    Ok(it.new_array(out))
}

/// Expand `$&`, `$1`–`$99`, `` $` ``, `$'`, `$$` in a replacement template.
fn expand_replacement(tmpl: &str, chars: &[char], m: &super::regex::Match) -> String {
    let whole: String = chars[m.start..m.end].iter().collect();
    let t: Vec<char> = tmpl.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < t.len() {
        if t[i] == '$' && i + 1 < t.len() {
            match t[i + 1] {
                '$' => {
                    out.push('$');
                    i += 2;
                }
                '&' => {
                    out.push_str(&whole);
                    i += 2;
                }
                '`' => {
                    out.extend(&chars[..m.start]);
                    i += 2;
                }
                '\'' => {
                    out.extend(&chars[m.end..]);
                    i += 2;
                }
                d if d.is_ascii_digit() => {
                    let mut j = i + 1;
                    let mut num = 0usize;
                    let mut digits = 0;
                    while j < t.len() && t[j].is_ascii_digit() && digits < 2 {
                        num = num * 10 + t[j].to_digit(10).unwrap() as usize;
                        j += 1;
                        digits += 1;
                    }
                    if num >= 1 && num <= m.groups.len() {
                        if let Some((a, b)) = m.groups[num - 1] {
                            out.extend(&chars[a..b]);
                        }
                        i = j;
                    } else {
                        out.push('$');
                        i += 1;
                    }
                }
                _ => {
                    out.push('$');
                    i += 1;
                }
            }
        } else {
            out.push(t[i]);
            i += 1;
        }
    }
    out
}

fn regex_replace(it: &mut Interp, s: &str, re: &Regex, repl: &Value) -> Eval<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut pos = 0;
    while let Some(m) = re.exec(&chars, pos) {
        out.extend(&chars[pos..m.start]);
        if repl.is_callable() {
            let mut cb_args = vec![Value::str(chars[m.start..m.end].iter().collect::<String>())];
            for g in &m.groups {
                cb_args.push(match g {
                    Some((a, b)) => Value::str(chars[*a..*b].iter().collect::<String>()),
                    None => Value::Undefined,
                });
            }
            cb_args.push(Value::Num(m.start as f64));
            cb_args.push(Value::str(s));
            let r = it.call(repl, Value::Undefined, &cb_args)?;
            out.push_str(&it.to_string_v(&r)?);
        } else {
            let tmpl = it.to_string_v(repl)?;
            out.push_str(&expand_replacement(&tmpl, &chars, &m));
        }
        pos = if m.end == m.start {
            if m.end < chars.len() {
                out.push(chars[m.end]);
            }
            m.end + 1
        } else {
            m.end
        };
        if !re.global {
            break;
        }
        if pos > chars.len() {
            break;
        }
    }
    if pos <= chars.len() {
        out.extend(&chars[pos.min(chars.len())..]);
    }
    Ok(out)
}

fn str_replace(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let s = it.to_string_v(&this)?;
    let pat = arg(a, 0);
    let repl = arg(a, 1);
    if let Some((src, flags)) = as_regex(&pat) {
        if let Ok(re) = Regex::new(&src, &flags) {
            return Ok(Value::str(regex_replace(it, &s, &re, &repl)?));
        }
        return Ok(Value::str(s));
    }
    // Plain-string replace of the first occurrence.
    let from = it.to_string_v(&pat)?;
    if repl.is_callable() {
        if let Some(idx) = s.find(&from) {
            let r = it.call(
                &repl,
                Value::Undefined,
                &[Value::str(from.clone()), Value::Num(s[..idx].chars().count() as f64), Value::str(s.clone())],
            )?;
            let rep = it.to_string_v(&r)?;
            return Ok(Value::str(format!("{}{}{}", &s[..idx], rep, &s[idx + from.len()..])));
        }
        return Ok(Value::str(s));
    }
    let to = it.to_string_v(&repl)?;
    Ok(Value::str(s.replacen(&from, &to, 1)))
}

fn str_replace_all(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let s = it.to_string_v(&this)?;
    let pat = arg(a, 0);
    let repl = arg(a, 1);
    if let Some((src, flags)) = as_regex(&pat) {
        let flags = if flags.contains('g') { flags } else { format!("{flags}g") };
        if let Ok(re) = Regex::new(&src, &flags) {
            return Ok(Value::str(regex_replace(it, &s, &re, &repl)?));
        }
        return Ok(Value::str(s));
    }
    let from = it.to_string_v(&pat)?;
    let to = it.to_string_v(&repl)?;
    Ok(Value::str(s.replace(&from, &to)))
}

fn str_split(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    let s = it.to_string_v(&this)?;
    let sep = arg(a, 0);
    if let Some((src, flags)) = as_regex(&sep) {
        if let Ok(re) = Regex::new(&src, &flags) {
            let chars: Vec<char> = s.chars().collect();
            let mut parts = Vec::new();
            let mut last = 0;
            let mut pos = 0;
            while pos <= chars.len() {
                match re.exec(&chars, pos) {
                    Some(m) if m.end > m.start || m.start > last => {
                        if m.start == m.end && m.start == last {
                            pos += 1;
                            continue;
                        }
                        parts.push(Value::str(chars[last..m.start].iter().collect::<String>()));
                        last = m.end;
                        pos = if m.end == m.start { m.end + 1 } else { m.end };
                    }
                    _ => break,
                }
            }
            parts.push(Value::str(chars[last..].iter().collect::<String>()));
            return Ok(it.new_array(parts));
        }
    }
    match sep {
        Value::Undefined => Ok(it.new_array(vec![Value::str(s)])),
        _ => {
            let sep = it.to_string_v(&sep)?;
            let parts: Vec<Value> = if sep.is_empty() {
                s.chars().map(|c| Value::str(c.to_string())).collect()
            } else {
                s.split(&sep).map(Value::str).collect()
            };
            Ok(it.new_array(parts))
        }
    }
}

// ---- Map & Set -------------------------------------------------------------

/// SameValueZero key equality (like `===` but with `NaN` equal to itself).
fn same_value_zero(a: &Value, b: &Value) -> bool {
    if let (Value::Num(x), Value::Num(y)) = (a, b) {
        return x == y || (x.is_nan() && y.is_nan());
    }
    strict_eq(a, b)
}

fn slot_gc(this: &Value, slot: &str) -> Option<Gc> {
    if let Value::Object(o) = this {
        if let Some(PropDesc::Data(Value::Object(a))) = o.borrow().get_own(slot) {
            return Some(a.clone());
        }
    }
    None
}

fn slot_vec(this: &Value, slot: &str) -> Vec<Value> {
    match slot_gc(this, slot) {
        Some(g) => {
            if let ObjKind::Array(e) = &g.borrow().kind {
                e.clone()
            } else {
                Vec::new()
            }
        }
        None => Vec::new(),
    }
}

fn arr_push(gc: &Gc, v: Value) {
    if let ObjKind::Array(e) = &mut gc.borrow_mut().kind {
        e.push(v);
    }
}

fn arr_remove(gc: &Gc, i: usize) {
    if let ObjKind::Array(e) = &mut gc.borrow_mut().kind {
        if i < e.len() {
            e.remove(i);
        }
    }
}

fn def_getter(it: &Interp, proto: &Gc, name: &str, f: NativeFn) {
    let g = it.native_fn(name, f).as_object().cloned();
    proto
        .borrow_mut()
        .set_own(name, PropDesc::Accessor { get: g, set: None });
}

fn install_map_set(it: &mut Interp) {
    let mproto = it.new_object(Some(it.object_proto.clone()));
    it.define_method(&mproto, "set", map_set);
    it.define_method(&mproto, "get", map_get);
    it.define_method(&mproto, "has", map_has);
    it.define_method(&mproto, "delete", map_delete);
    it.define_method(&mproto, "clear", coll_clear);
    it.define_method(&mproto, "forEach", map_for_each);
    it.define_method(&mproto, "keys", |it, this, _a| Ok(it.new_array(slot_vec(&this, "__keys"))));
    it.define_method(&mproto, "values", |it, this, _a| Ok(it.new_array(slot_vec(&this, "__vals"))));
    it.define_method(&mproto, "entries", map_entries_method);
    def_getter(it, &mproto, "size", map_size);
    let mctor = make_ctor(it, "Map", map_construct, &mproto);
    set_global(it, "Map", mctor);

    let sproto = it.new_object(Some(it.object_proto.clone()));
    it.define_method(&sproto, "add", set_add);
    it.define_method(&sproto, "has", set_has);
    it.define_method(&sproto, "delete", set_delete);
    it.define_method(&sproto, "clear", coll_clear);
    it.define_method(&sproto, "forEach", set_for_each);
    it.define_method(&sproto, "values", |it, this, _a| Ok(it.new_array(slot_vec(&this, "__vals"))));
    it.define_method(&sproto, "keys", |it, this, _a| Ok(it.new_array(slot_vec(&this, "__vals"))));
    def_getter(it, &sproto, "size", set_size);
    let sctor = make_ctor(it, "Set", set_construct, &sproto);
    set_global(it, "Set", sctor);
}

fn map_size(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    Ok(Value::Num(slot_vec(&this, "__keys").len() as f64))
}
fn set_size(_it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    Ok(Value::Num(slot_vec(&this, "__vals").len() as f64))
}

fn map_construct(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let keys = it.new_array(Vec::new());
    let vals = it.new_array(Vec::new());
    if let Value::Object(o) = &this {
        let mut b = o.borrow_mut();
        b.class = "Map";
        b.set_data("__keys", keys);
        b.set_data("__vals", vals);
    }
    if let Some(init) = args.first() {
        if !init.is_nullish() {
            for entry in it.iterate(init)? {
                let k = it.get_member(&entry, "0")?;
                let v = it.get_member(&entry, "1")?;
                map_set(it, this.clone(), &[k, v])?;
            }
        }
    }
    Ok(Value::Undefined)
}

fn map_set(_it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let k = arg(args, 0);
    let v = arg(args, 1);
    if let (Some(kg), Some(vg)) = (slot_gc(&this, "__keys"), slot_gc(&this, "__vals")) {
        let keys = slot_vec(&this, "__keys");
        match keys.iter().position(|x| same_value_zero(x, &k)) {
            Some(i) => {
                if let ObjKind::Array(e) = &mut vg.borrow_mut().kind {
                    e[i] = v;
                }
            }
            None => {
                arr_push(&kg, k);
                arr_push(&vg, v);
            }
        }
    }
    Ok(this)
}

fn map_get(_it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let k = arg(args, 0);
    let keys = slot_vec(&this, "__keys");
    match keys.iter().position(|x| same_value_zero(x, &k)) {
        Some(i) => Ok(slot_vec(&this, "__vals").get(i).cloned().unwrap_or(Value::Undefined)),
        None => Ok(Value::Undefined),
    }
}

fn map_has(_it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let k = arg(args, 0);
    Ok(Value::Bool(slot_vec(&this, "__keys").iter().any(|x| same_value_zero(x, &k))))
}

fn map_delete(_it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let k = arg(args, 0);
    let keys = slot_vec(&this, "__keys");
    match keys.iter().position(|x| same_value_zero(x, &k)) {
        Some(i) => {
            if let Some(kg) = slot_gc(&this, "__keys") {
                arr_remove(&kg, i);
            }
            if let Some(vg) = slot_gc(&this, "__vals") {
                arr_remove(&vg, i);
            }
            Ok(Value::Bool(true))
        }
        None => Ok(Value::Bool(false)),
    }
}

fn map_for_each(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let cb = arg(args, 0);
    let keys = slot_vec(&this, "__keys");
    let vals = slot_vec(&this, "__vals");
    for (k, v) in keys.iter().zip(vals.iter()) {
        it.call(&cb, Value::Undefined, &[v.clone(), k.clone(), this.clone()])?;
    }
    Ok(Value::Undefined)
}

fn map_entries_method(it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    let keys = slot_vec(&this, "__keys");
    let vals = slot_vec(&this, "__vals");
    let pairs: Vec<Value> = keys
        .into_iter()
        .zip(vals)
        .map(|(k, v)| it.new_array(vec![k, v]))
        .collect();
    Ok(it.new_array(pairs))
}

fn coll_clear(it: &mut Interp, this: Value, _a: &[Value]) -> Eval<Value> {
    for slot in ["__keys", "__vals"] {
        if let Value::Object(o) = &this {
            if o.borrow().get_own(slot).is_some() {
                let empty = it.new_array(Vec::new());
                o.borrow_mut().set_data(slot, empty);
            }
        }
    }
    Ok(Value::Undefined)
}

fn set_construct(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let vals = it.new_array(Vec::new());
    if let Value::Object(o) = &this {
        let mut b = o.borrow_mut();
        b.class = "Set";
        b.set_data("__vals", vals);
    }
    if let Some(init) = args.first() {
        if !init.is_nullish() {
            for v in it.iterate(init)? {
                set_add(it, this.clone(), &[v])?;
            }
        }
    }
    Ok(Value::Undefined)
}

fn set_add(_it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let v = arg(args, 0);
    if let Some(vg) = slot_gc(&this, "__vals") {
        if !slot_vec(&this, "__vals").iter().any(|x| same_value_zero(x, &v)) {
            arr_push(&vg, v);
        }
    }
    Ok(this)
}

fn set_has(_it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let v = arg(args, 0);
    Ok(Value::Bool(slot_vec(&this, "__vals").iter().any(|x| same_value_zero(x, &v))))
}

fn set_delete(_it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let v = arg(args, 0);
    let vals = slot_vec(&this, "__vals");
    match vals.iter().position(|x| same_value_zero(x, &v)) {
        Some(i) => {
            if let Some(vg) = slot_gc(&this, "__vals") {
                arr_remove(&vg, i);
            }
            Ok(Value::Bool(true))
        }
        None => Ok(Value::Bool(false)),
    }
}

fn set_for_each(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let cb = arg(args, 0);
    for v in slot_vec(&this, "__vals") {
        it.call(&cb, Value::Undefined, &[v.clone(), v.clone(), this.clone()])?;
    }
    Ok(Value::Undefined)
}

// ---- Promise + timers ------------------------------------------------------

fn this_promise(v: &Value) -> Option<Gc> {
    match v {
        Value::Object(o) if o.borrow().class == "Promise" => Some(o.clone()),
        _ => None,
    }
}

fn install_promise(it: &mut Interp) {
    let proto = it.promise_proto.clone();
    it.define_method(&proto, "then", promise_then_method);
    it.define_method(&proto, "catch", promise_catch_method);
    it.define_method(&proto, "finally", promise_finally_method);

    let ctor = make_ctor(it, "Promise", promise_construct, &proto);
    if let Value::Object(c) = &ctor {
        let cc = c.clone();
        it.define_method(&cc, "resolve", |it, _t, a| Ok(it.make_resolved_promise(arg(a, 0))));
        it.define_method(&cc, "reject", |it, _t, a| Ok(it.make_rejected_promise(arg(a, 0))));
        it.define_method(&cc, "all", promise_all);
        it.define_method(&cc, "race", promise_race);
        it.define_method(&cc, "allSettled", promise_all_settled);
    }
    set_global(it, "Promise", ctor);
}

fn promise_construct(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let p = match &this {
        Value::Object(o) => o.clone(),
        _ => it.new_promise(),
    };
    {
        let mut b = p.borrow_mut();
        b.class = "Promise";
        b.set_data("__state", Value::str("pending"));
        b.set_data("__value", Value::Undefined);
    }
    let cbs = it.new_array(Vec::new());
    p.borrow_mut().set_data("__cbs", cbs);

    let executor = arg(args, 0);
    if executor.is_callable() {
        let resolve = it.bound_method(promise_resolve_native, Value::Object(p.clone()));
        let reject = it.bound_method(promise_reject_native, Value::Object(p.clone()));
        match it.call(&executor, Value::Undefined, &[resolve, reject]) {
            Ok(_) => {}
            Err(super::interp::Abrupt::Throw(e)) => it.reject_promise(&p, e),
            Err(other) => return Err(other),
        }
    }
    Ok(Value::Undefined)
}

fn promise_resolve_native(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    if let Value::Object(p) = &this {
        it.resolve_promise(p, arg(args, 0));
    }
    Ok(Value::Undefined)
}
fn promise_reject_native(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    if let Value::Object(p) = &this {
        it.reject_promise(p, arg(args, 0));
    }
    Ok(Value::Undefined)
}

fn promise_then_method(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    match this_promise(&this) {
        Some(p) => Ok(it.promise_then(&p, arg(args, 0), arg(args, 1))),
        None => it.throw_type("Promise.prototype.then called on a non-Promise"),
    }
}
fn promise_catch_method(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    match this_promise(&this) {
        Some(p) => Ok(it.promise_then(&p, Value::Undefined, arg(args, 0))),
        None => it.throw_type("Promise.prototype.catch called on a non-Promise"),
    }
}
fn promise_finally_method(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    let p = match this_promise(&this) {
        Some(p) => p,
        None => return it.throw_type("Promise.prototype.finally on a non-Promise"),
    };
    let on = arg(args, 0);
    let f = it.bound_method(finally_fulfill, on.clone());
    let r = it.bound_method(finally_reject, on);
    Ok(it.promise_then(&p, f, r))
}
fn finally_fulfill(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    if this.is_callable() {
        it.call(&this, Value::Undefined, &[])?;
    }
    Ok(arg(args, 0))
}
fn finally_reject(it: &mut Interp, this: Value, args: &[Value]) -> Eval<Value> {
    if this.is_callable() {
        it.call(&this, Value::Undefined, &[])?;
    }
    Err(super::interp::Abrupt::Throw(arg(args, 0)))
}

fn promise_all(it: &mut Interp, _t: Value, args: &[Value]) -> Eval<Value> {
    let items = it.iterate(&arg(args, 0))?;
    let mut results = Vec::new();
    for item in items {
        match it.await_value(item) {
            Ok(v) => results.push(v),
            Err(super::interp::Abrupt::Throw(e)) => return Ok(it.make_rejected_promise(e)),
            Err(other) => return Err(other),
        }
    }
    let arr = it.new_array(results);
    Ok(it.make_resolved_promise(arr))
}
fn promise_race(it: &mut Interp, _t: Value, args: &[Value]) -> Eval<Value> {
    // In this synchronous model every input settles when awaited, so the first
    // input determines the race outcome.
    match it.iterate(&arg(args, 0))?.into_iter().next() {
        Some(item) => match it.await_value(item) {
            Ok(v) => Ok(it.make_resolved_promise(v)),
            Err(super::interp::Abrupt::Throw(e)) => Ok(it.make_rejected_promise(e)),
            Err(other) => Err(other),
        },
        None => Ok(Value::Object(it.new_promise())),
    }
}
fn promise_all_settled(it: &mut Interp, _t: Value, args: &[Value]) -> Eval<Value> {
    let mut out = Vec::new();
    for item in it.iterate(&arg(args, 0))? {
        let o = it.new_object(Some(it.object_proto.clone()));
        match it.await_value(item) {
            Ok(v) => {
                o.borrow_mut().set_data("status", Value::str("fulfilled"));
                o.borrow_mut().set_data("value", v);
            }
            Err(super::interp::Abrupt::Throw(e)) => {
                o.borrow_mut().set_data("status", Value::str("rejected"));
                o.borrow_mut().set_data("reason", e);
            }
            Err(other) => return Err(other),
        }
        out.push(Value::Object(o));
    }
    let arr = it.new_array(out);
    Ok(it.make_resolved_promise(arr))
}

// ---- Generators ------------------------------------------------------------

fn install_generator(it: &mut Interp) {
    let proto = it.generator_proto.clone();
    it.define_method(&proto, "next", gen_next);
    it.define_method(&proto, "return", gen_return);
}

fn gen_next(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    // Lazy (VM-backed) generators carry a `__genid`; resume their frame.
    if let Value::Num(id) = it.get_member(&this, "__genid")? {
        return it.generator_next(id as usize, arg(a, 0));
    }
    let items = slot_vec(&this, "__items");
    let idx_v = it.get_member(&this, "__index")?;
    let idx = it.to_number(&idx_v)? as usize;
    let result = it.new_object(Some(it.object_proto.clone()));
    if idx < items.len() {
        result.borrow_mut().set_data("value", items[idx].clone());
        result.borrow_mut().set_data("done", Value::Bool(false));
        it.set_member(&this, "__index", Value::Num((idx + 1) as f64))?;
    } else {
        let ret = it.get_member(&this, "__return")?;
        result.borrow_mut().set_data("value", ret);
        result.borrow_mut().set_data("done", Value::Bool(true));
    }
    Ok(Value::Object(result))
}

fn gen_return(it: &mut Interp, this: Value, a: &[Value]) -> Eval<Value> {
    if let Value::Num(id) = it.get_member(&this, "__genid")? {
        return Ok(it.generator_return(id as usize, arg(a, 0)));
    }
    let len = slot_vec(&this, "__items").len();
    it.set_member(&this, "__index", Value::Num(len as f64))?;
    let result = it.new_object(Some(it.object_proto.clone()));
    result.borrow_mut().set_data("value", arg(a, 0));
    result.borrow_mut().set_data("done", Value::Bool(true));
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::super::interp::Interp;
    use super::super::value::Value;

    fn eval(src: &str) -> Value {
        let mut it = Interp::new();
        let p = super::super::parser::parse(src).expect("parse");
        it.run(&p).unwrap_or_else(|e| panic!("eval error: {e:?}"))
    }

    fn num(src: &str) -> f64 {
        match eval(src) {
            Value::Num(n) => n,
            other => panic!("expected number, got {other:?}"),
        }
    }
    fn string(src: &str) -> String {
        match eval(src) {
            Value::Str(s) => s.to_string(),
            other => panic!("expected string, got {other:?}"),
        }
    }
    fn boolean(src: &str) -> bool {
        match eval(src) {
            Value::Bool(b) => b,
            other => panic!("expected bool, got {other:?}"),
        }
    }

    #[test]
    fn array_methods() {
        assert_eq!(num("[1,2,3].map(x => x * 2).reduce((a,b) => a + b, 0)"), 12.0);
        assert_eq!(num("[1,2,3,4].filter(x => x % 2 === 0).length"), 2.0);
        assert_eq!(string("['a','b','c'].join('-')"), "a-b-c");
        assert_eq!(num("let a=[3,1,2]; a.sort((x,y)=>x-y); a[0]*100 + a[2]"), 103.0);
        assert_eq!(num("[1,2,3].indexOf(2)"), 1.0);
        assert!(boolean("[1,2,3].includes(3)"));
        assert_eq!(num("let a=[1]; a.push(2,3); a.length"), 3.0);
        assert_eq!(num("[5,4,3].find(x => x < 5)"), 4.0);
    }

    #[test]
    fn string_methods() {
        assert_eq!(string("'Hello'.toUpperCase()"), "HELLO");
        assert_eq!(string("'  hi  '.trim()"), "hi");
        assert_eq!(string("'a,b,c'.split(',').join('|')"), "a|b|c");
        assert_eq!(num("'hello'.indexOf('l')"), 2.0);
        assert_eq!(string("'abc'.slice(1)"), "bc");
        assert_eq!(string("'x'.repeat(3)"), "xxx");
        assert_eq!(string("'5'.padStart(3, '0')"), "005");
        assert_eq!(string("'foofoo'.replaceAll('o', '0')"), "f00f00");
        assert!(boolean("'hello world'.startsWith('hello')"));
    }

    #[test]
    fn math_builtins() {
        assert_eq!(num("Math.max(3, 7, 2)"), 7.0);
        assert_eq!(num("Math.min(3, 7, 2)"), 2.0);
        assert_eq!(num("Math.floor(3.9)"), 3.0);
        assert_eq!(num("Math.round(2.5)"), 3.0);
        assert_eq!(num("Math.abs(-4)"), 4.0);
        assert_eq!(num("Math.pow(2, 10)"), 1024.0);
        assert!(num("Math.PI") > 3.14 && num("Math.PI") < 3.15);
    }

    #[test]
    fn json_roundtrip() {
        assert_eq!(
            string("JSON.stringify({a: 1, b: [2, 3], c: 'x'})"),
            r#"{"a":1,"b":[2,3],"c":"x"}"#
        );
        assert_eq!(num("JSON.parse('{\"n\": 42}').n"), 42.0);
        assert_eq!(num("JSON.parse('[1,2,3]').length"), 3.0);
        assert_eq!(
            string("JSON.parse(JSON.stringify({k: 'v'})).k"),
            "v"
        );
    }

    #[test]
    fn object_statics() {
        assert_eq!(string("Object.keys({a:1, b:2}).join(',')"), "a,b");
        assert_eq!(num("Object.values({a:1, b:2}).reduce((x,y)=>x+y,0)"), 3.0);
        assert_eq!(num("Object.assign({}, {a:1}, {b:2}).b"), 2.0);
    }

    #[test]
    fn function_call_apply_bind() {
        assert_eq!(
            num("function f(a,b){ return this.k + a + b; } f.call({k:10}, 1, 2)"),
            13.0
        );
        assert_eq!(
            num("function f(a,b){ return a + b; } f.apply(null, [4, 5])"),
            9.0
        );
        assert_eq!(
            num("function f(a,b){ return this.k + a + b; } let g = f.bind({k:100}, 1); g(2)"),
            103.0
        );
    }

    #[test]
    fn console_capture() {
        let mut it = Interp::new();
        let p = super::super::parser::parse("console.log('hello', 42); console.log('world')").unwrap();
        it.run(&p).unwrap();
        assert_eq!(it.output, vec!["hello 42".to_string(), "world".to_string()]);
    }

    #[test]
    fn parse_int_float() {
        assert_eq!(num("parseInt('42px')"), 42.0);
        assert_eq!(num("parseInt('ff', 16)"), 255.0);
        assert_eq!(num("parseFloat('3.14xyz')"), 3.14);
        assert!(boolean("isNaN(parseInt('abc'))"));
    }

    #[test]
    fn regexp_and_string_regex_methods() {
        assert!(boolean(r"/\d+/.test('abc123')"));
        assert!(!boolean(r"/^\d+$/.test('12a')"));
        assert_eq!(string("'2024-01-15'.replace(/-/g, '/')"), "2024/01/15");
        assert_eq!(
            string(r"'hello world'.replace(/(\w+) (\w+)/, '$2 $1')"),
            "world hello"
        );
        assert_eq!(num(r"'a1b2c3'.match(/\d/g).length"), 3.0);
        assert_eq!(num(r"'one two three'.split(/\s+/).length"), 3.0);
        assert_eq!(num("'foobar'.search(/bar/)"), 3.0);
        assert_eq!(string(r"/(\w)(\w)/.exec('ab')[2]"), "b");
        assert_eq!(
            string(r"'a1b2'.replace(/\d/g, function(d){ return '[' + d + ']'; })"),
            "a[1]b[2]"
        );
        assert!(boolean("new RegExp('a.c').test('axc')"));
    }

    #[test]
    fn map_and_set() {
        assert_eq!(
            num("let m = new Map(); m.set('a', 1); m.set('b', 2); m.set('a', 9); m.get('a') + m.size"),
            11.0
        );
        assert!(boolean("let m = new Map([['x', 1]]); m.has('x')"));
        assert_eq!(
            num("let s = new Set([1, 2, 2, 3, 3, 3]); s.size"),
            3.0
        );
        assert!(boolean("let s = new Set(); s.add(5); s.has(5)"));
        // for-of over a Map yields [k, v] entries; over a Set yields values.
        assert_eq!(
            num("let m = new Map([['a',10],['b',20]]); let t=0; for (const [k,v] of m) t += v; t"),
            30.0
        );
        assert_eq!(
            num("let s = new Set([4,5,6]); let t=0; for (const v of s) t += v; t"),
            15.0
        );
        assert_eq!(
            num("let m = new Map(); m.set('a',1); m.delete('a'); m.size"),
            0.0
        );
    }

    fn run_then_global(src: &str, name: &str) -> Value {
        let mut it = Interp::new();
        let p = super::super::parser::parse(src).expect("parse");
        it.run(&p).expect("run");
        let g = Value::Object(it.global.clone());
        it.get_member(&g, name).expect("get global")
    }

    #[test]
    fn async_await_microtask_model() {
        // `await` now *yields* (spec microtask ordering); observe the result
        // after the event loop has drained.
        assert!(matches!(
            run_then_global(
                "globalThis.out=0; async function f(){ return 5; } async function g(){ globalThis.out = await f() + 1; } g();",
                "out"
            ),
            Value::Num(n) if n == 6.0
        ));
        assert!(matches!(
            run_then_global(
                "globalThis.s=0; async function g(){ let xs = await Promise.all([1,2,3].map(x=>Promise.resolve(x))); globalThis.s = xs[0]+xs[1]+xs[2]; } g();",
                "s"
            ),
            Value::Num(n) if n == 6.0
        ));
        // A rejected await throws at the await point and is caught by the
        // VM-compiled `try`/`catch`.
        assert!(matches!(
            run_then_global(
                "globalThis.m=''; async function g(){ try { await Promise.reject('boom'); } catch(e) { globalThis.m = e; } } g();",
                "m"
            ),
            Value::Str(s) if &*s == "boom"
        ));
        // `catch` can recover and the async function resolves with its return.
        assert!(matches!(
            run_then_global(
                "globalThis.v=0; async function g(){ try { await Promise.reject('x'); return -1; } catch(e) { return 7; } } g().then(r => { globalThis.v = r; });",
                "v"
            ),
            Value::Num(n) if n == 7.0
        ));
        // An uncaught rejected await rejects the returned promise.
        assert!(matches!(
            run_then_global(
                "globalThis.c=''; async function g(){ await Promise.reject('bad'); } g().catch(e => { globalThis.c = e; });",
                "c"
            ),
            Value::Str(s) if &*s == "bad"
        ));
    }

    #[test]
    fn async_await_orders_after_sync_code() {
        // `await` defers the continuation past the synchronous tail, so the
        // order is a, b, c, d — not a, b, d, c (the old synchronous model).
        let mut it = Interp::new();
        let p = super::super::parser::parse(
            "console.log('a'); async function f(){ console.log('b'); await 0; console.log('d'); } f(); console.log('c');",
        )
        .expect("parse");
        it.run(&p).expect("run");
        assert_eq!(it.output, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn promise_microtasks_and_timers() {
        assert!(matches!(
            run_then_global("globalThis.out=0; Promise.resolve(21).then(v=>{ globalThis.out=v*2; });", "out"),
            Value::Num(n) if n == 42.0
        ));
        assert!(matches!(
            run_then_global("globalThis.r=0; Promise.resolve(2).then(v=>v+3).then(v=>{ globalThis.r=v; });", "r"),
            Value::Num(n) if n == 5.0
        ));
        assert!(matches!(
            run_then_global("globalThis.t=0; setTimeout(()=>{ globalThis.t=99; }, 5);", "t"),
            Value::Num(n) if n == 99.0
        ));
        assert!(matches!(
            run_then_global("globalThis.c=0; Promise.reject('x').catch(()=>{ globalThis.c=7; });", "c"),
            Value::Num(n) if n == 7.0
        ));
    }

    #[test]
    fn generators() {
        assert_eq!(
            num("function* g(){ yield 1; yield 2; yield 3; } let s=0; for (const x of g()) s += x; s"),
            6.0
        );
        assert_eq!(
            num("function* g(){ yield 10; yield 20; } let it = g(); it.next().value + it.next().value"),
            30.0
        );
        assert!(boolean("function* g(){ yield 1; } let it = g(); it.next(); it.next().done"));
        assert_eq!(
            num("function* inner(){ yield 1; yield 2; } function* g(){ yield* inner(); yield 3; } let s=0; for (const x of g()) s += x; s"),
            6.0
        );
        assert_eq!(num("function* g(){ yield 4; yield 5; } [...g()].length"), 2.0);
    }

    #[test]
    fn eval_function_symbol_tagged() {
        // Direct eval sees the local scope.
        assert_eq!(num("let x = 10; eval('x + 5')"), 15.0);
        // The Function constructor.
        assert_eq!(num("let f = new Function('a', 'b', 'return a + b'); f(3, 4)"), 7.0);
        assert_eq!(num("Function('return 42')()"), 42.0);
        // Tagged template cooking: tag(strings, ...values).
        assert_eq!(
            string("function tag(s, ...v){ return s[0] + v[0] + s[1]; } let n = 5; tag`a${n}b`"),
            "a5b"
        );
        // Symbols.
        assert!(boolean("typeof Symbol() === 'symbol'"));
        assert!(boolean("Symbol() !== Symbol()"));
        assert!(boolean("Symbol.for('k') === Symbol.for('k')"));
        // Custom iterable via Symbol.iterator.
        assert_eq!(
            num("let obj = { [Symbol.iterator]() { let i = 0; return { next() { return i < 3 ? { value: ++i, done: false } : { value: undefined, done: true }; } }; } }; let s = 0; for (const x of obj) s += x; s"),
            6.0
        );
    }
}
