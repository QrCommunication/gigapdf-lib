//! The tree-walking JavaScript interpreter.
//!
//! Evaluates a parsed [`Program`] against a runtime built on [`super::value`].
//! Abrupt completions (`return` / `break` / `continue` / `throw`) flow through
//! Rust's `Result` as the [`Abrupt`] error type, so `?` propagates them
//! naturally. Object/array/function semantics, the prototype chain, the abstract
//! operations that may call user code (`ToPrimitive`, abstract `==`), closures,
//! `this` binding and `var`/function hoisting are all implemented here.
//!
//! Scope: ES5 core plus widely-used ES2015+ runtime semantics (arrows,
//! destructuring, spread, template literals, `for…of`, classes). Not yet:
//! generators/`yield` execution, `async`/`await` execution, the `arguments`
//! object, `Symbol`, and a real event loop.

use super::ast::*;
use super::bytecode::Op;
use super::compile;
use super::value::*;
use super::vm::{Frame, Step};
use std::cell::RefCell;
use std::rc::Rc;

/// An abrupt completion — the `Err` half of [`Eval`].
#[derive(Debug, Clone)]
pub enum Abrupt {
    /// `return value;`
    Return(Value),
    /// `break [label];`
    Break(Option<String>),
    /// `continue [label];`
    Continue(Option<String>),
    /// A thrown value.
    Throw(Value),
}

/// The result of evaluating a node: a value (or other completion) or an abrupt.
pub type Eval<T> = Result<T, Abrupt>;

/// A deferred unit of Promise work.
pub type Microtask = Box<dyn FnOnce(&mut Interp) -> Eval<()>>;

/// The Promise microtask queue. Stored as boxed closures so the scheduler can
/// run Rust-side resolution logic; its `Debug` is shallow (a pending count).
#[derive(Default)]
pub struct MicrotaskQueue(std::collections::VecDeque<Microtask>);

impl core::fmt::Debug for MicrotaskQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "MicrotaskQueue({} pending)", self.0.len())
    }
}

/// How a binding is being introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclKind {
    Var,
    Let,
    Const,
    Param,
}

/// The interpreter and its intrinsics.
#[derive(Debug)]
pub struct Interp {
    /// The global object.
    pub global: Gc,
    /// `Object.prototype`.
    pub object_proto: Gc,
    /// `Function.prototype`.
    pub function_proto: Gc,
    /// `Array.prototype`.
    pub array_proto: Gc,
    /// `String.prototype`.
    pub string_proto: Gc,
    /// `Number.prototype`.
    pub number_proto: Gc,
    /// `Boolean.prototype`.
    pub boolean_proto: Gc,
    /// `Error.prototype`.
    pub error_proto: Gc,
    /// `RegExp.prototype`.
    pub regexp_proto: Gc,
    /// `Promise.prototype`.
    pub promise_proto: Gc,
    /// `Generator.prototype` (the shared prototype of generator objects).
    pub generator_proto: Gc,
    /// Active generator yield buffers (one per running `function*` body). The
    /// eager fallback model runs the body to completion collecting `yield`ed
    /// values (used when a body can't be compiled to suspendable bytecode).
    pub gen_yield_stack: Vec<Vec<Value>>,
    /// Suspended VM frames backing **lazy** generators (and `await` points),
    /// indexed by the `__genid` stored on the generator object. `None` once the
    /// frame has finished, so the slot's `Rc<Chunk>` is released.
    pub gen_frames: Vec<Option<Frame>>,
    /// Monotonic counter giving each `Symbol()` a unique property key.
    pub sym_counter: u64,
    /// The microtask queue (Promise reactions), drained after each task.
    pub microtasks: MicrotaskQueue,
    /// Pending timers (`setTimeout`): `(delay, callback, args)`. Run after the
    /// script's synchronous part and microtasks, in delay order.
    pub timers: Vec<(f64, Value, Vec<Value>)>,
    /// The global lexical environment.
    pub global_env: Env,
    /// Captured `console` output (one entry per `console.*` call).
    pub output: Vec<String>,
    /// Deterministic PRNG state for `Math.random` (WASM has no entropy; a fixed
    /// seed also makes renders reproducible).
    pub rng_state: u64,
    /// Shared DOM prototypes `[element, text, document]`, installed by
    /// [`super::dom`] when running `<script>`s; empty otherwise.
    pub dom_protos: Vec<Gc>,
    /// Internal flag: an optional-chain link short-circuited.
    short_circuit: bool,
}

impl Interp {
    /// Build a fresh interpreter with empty intrinsic prototypes installed.
    /// Built-ins (`console`, `Math`, `Array.prototype.*`, …) are layered on by
    /// [`super::builtins::install`].
    pub fn new() -> Interp {
        let object_proto = Rc::new(RefCell::new(Obj::plain(None)));
        let mk = |proto: &Gc| Rc::new(RefCell::new(Obj::plain(Some(proto.clone()))));
        let function_proto = mk(&object_proto);
        let array_proto = mk(&object_proto);
        let string_proto = mk(&object_proto);
        let number_proto = mk(&object_proto);
        let boolean_proto = mk(&object_proto);
        let error_proto = mk(&object_proto);
        let regexp_proto = mk(&object_proto);
        let promise_proto = mk(&object_proto);
        let generator_proto = mk(&object_proto);
        let global = mk(&object_proto);

        let global_env = new_scope(None, true);
        global_env.borrow_mut().this_val = Some(Value::Object(global.clone()));

        let mut interp = Interp {
            global,
            object_proto,
            function_proto,
            array_proto,
            string_proto,
            number_proto,
            boolean_proto,
            error_proto,
            regexp_proto,
            promise_proto,
            generator_proto,
            gen_yield_stack: Vec::new(),
            gen_frames: Vec::new(),
            sym_counter: 0,
            microtasks: MicrotaskQueue::default(),
            timers: Vec::new(),
            global_env,
            output: Vec::new(),
            rng_state: 0x2545_F491_4F6C_DD1D,
            dom_protos: Vec::new(),
            short_circuit: false,
        };
        super::builtins::install(&mut interp);
        interp
    }

    /// Advance the deterministic PRNG and return a float in `[0, 1)`.
    pub fn next_random(&mut self) -> f64 {
        // xorshift64*
        let mut x = self.rng_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng_state = x;
        let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        // Top 53 bits → [0, 1).
        ((r >> 11) as f64) / ((1u64 << 53) as f64)
    }

    // ---- object construction ---------------------------------------------

    /// A new ordinary object with the given prototype.
    pub fn new_object(&self, proto: Option<Gc>) -> Gc {
        Rc::new(RefCell::new(Obj::plain(proto)))
    }

    /// A new array object wrapping `elems`.
    pub fn new_array(&self, elems: Vec<Value>) -> Value {
        Value::Object(Rc::new(RefCell::new(Obj {
            proto: Some(self.array_proto.clone()),
            props: Vec::new(),
            kind: ObjKind::Array(elems),
            extensible: true,
            class: "Array",
        })))
    }

    /// A native function value.
    pub fn native_fn(&self, name: &str, f: NativeFn) -> Value {
        let obj = Obj {
            proto: Some(self.function_proto.clone()),
            props: vec![("name".into(), PropDesc::Data(Value::str(name)))],
            kind: ObjKind::Function(Callable::Native {
                name: name.to_string(),
                f,
            }),
            extensible: true,
            class: "Function",
        };
        Value::Object(Rc::new(RefCell::new(obj)))
    }

    /// Install a method on an object (a native function under `name`).
    pub fn define_method(&self, target: &Gc, name: &str, f: NativeFn) {
        let func = self.native_fn(name, f);
        target.borrow_mut().set_data(name, func);
    }

    /// A native function bound to a fixed `this` (used for Promise
    /// `resolve`/`reject` and `finally`).
    pub fn bound_method(&self, f: NativeFn, this: Value) -> Value {
        let native = self.native_fn("", f);
        let o = Rc::new(RefCell::new(Obj {
            proto: Some(self.function_proto.clone()),
            props: Vec::new(),
            kind: ObjKind::Function(Callable::Bound {
                target: Box::new(native),
                bound_this: Box::new(this),
                bound_args: Vec::new(),
            }),
            extensible: true,
            class: "Function",
        }));
        Value::Object(o)
    }

    /// A user function value from an AST definition + closure environment.
    /// `home` is the `[[HomeObject]]` (the prototype/object a method lives on),
    /// used to resolve `super`; pass `None` for plain functions and arrows.
    fn make_user_function(
        &self,
        def: Rc<Func>,
        env: Env,
        is_arrow: bool,
        captured_this: Option<Value>,
        home: Option<Value>,
    ) -> Value {
        let callable = Callable::User {
            def: def.clone(),
            env,
            is_arrow,
            captured_this: captured_this.map(Box::new),
            home: home.map(Box::new),
        };
        let f = Rc::new(RefCell::new(Obj {
            proto: Some(self.function_proto.clone()),
            props: vec![
                (
                    "name".into(),
                    PropDesc::Data(Value::str(def.name.clone().unwrap_or_default())),
                ),
                ("length".into(), PropDesc::Data(Value::Num(def.params.len() as f64))),
            ],
            kind: ObjKind::Function(callable),
            extensible: true,
            class: "Function",
        }));
        if !is_arrow {
            let proto_obj = self.new_object(Some(self.object_proto.clone()));
            proto_obj
                .borrow_mut()
                .set_data("constructor", Value::Object(f.clone()));
            f.borrow_mut()
                .set_data("prototype", Value::Object(proto_obj));
        }
        Value::Object(f)
    }

    /// Build an `Error`-like object with `name`/`message`.
    pub fn make_error(&self, name: &str, message: impl Into<String>) -> Value {
        let o = self.new_object(Some(self.error_proto.clone()));
        {
            let mut b = o.borrow_mut();
            b.class = "Error";
            b.set_data("name", Value::str(name));
            b.set_data("message", Value::str(message.into()));
        }
        Value::Object(o)
    }

    /// Build a `RegExp` object (the compiled engine lives in [`super::regex`];
    /// the object carries its `source`/`flags` and is recompiled on use).
    pub fn make_regexp(&self, source: &str, flags: &str) -> Value {
        let o = self.new_object(Some(self.regexp_proto.clone()));
        {
            let mut b = o.borrow_mut();
            b.class = "RegExp";
            b.set_data("source", Value::str(source));
            b.set_data("flags", Value::str(flags));
            b.set_data("global", Value::Bool(flags.contains('g')));
            b.set_data("ignoreCase", Value::Bool(flags.contains('i')));
            b.set_data("multiline", Value::Bool(flags.contains('m')));
            b.set_data("sticky", Value::Bool(flags.contains('y')));
            b.set_data("lastIndex", Value::Num(0.0));
        }
        Value::Object(o)
    }

    // ---- promises & the microtask queue ----------------------------------

    /// Queue a Promise reaction (or any deferred work).
    pub fn enqueue_microtask(&mut self, task: Microtask) {
        self.microtasks.0.push_back(task);
    }

    /// Run one pending task — a microtask if any, else the earliest timer.
    /// Returns `false` when both queues are empty.
    fn run_one_task(&mut self) -> Eval<bool> {
        if let Some(task) = self.microtasks.0.pop_front() {
            task(self)?;
            return Ok(true);
        }
        if !self.timers.is_empty() {
            let mut idx = 0;
            for i in 1..self.timers.len() {
                if self.timers[i].0 < self.timers[idx].0 {
                    idx = i;
                }
            }
            let (_, cb, args) = self.timers.remove(idx);
            // A throwing timer callback must not abort the loop.
            let _ = self.call(&cb, Value::Undefined, &args);
            return Ok(true);
        }
        Ok(false)
    }

    /// Drain microtasks and timers until both are empty (bounded).
    pub fn run_event_loop(&mut self) -> Eval<()> {
        let mut guard = 0u64;
        while self.run_one_task()? {
            guard += 1;
            if guard > 5_000_000 {
                break;
            }
        }
        Ok(())
    }

    /// A fresh pending Promise.
    pub fn new_promise(&self) -> Gc {
        let o = self.new_object(Some(self.promise_proto.clone()));
        let cbs = self.new_array(Vec::new());
        {
            let mut b = o.borrow_mut();
            b.class = "Promise";
            b.set_data("__state", Value::str("pending"));
            b.set_data("__value", Value::Undefined);
            b.set_data("__cbs", cbs);
        }
        o
    }

    /// Settle a promise (fulfilled/rejected), scheduling its reactions.
    pub fn settle_promise(&mut self, p: &Gc, state: &str, value: Value) {
        if promise_state(p) != "pending" {
            return;
        }
        {
            let mut b = p.borrow_mut();
            b.set_data("__state", Value::str(state));
            b.set_data("__value", value);
        }
        for cb in read_slot_array(p, "__cbs") {
            let pc = p.clone();
            self.enqueue_microtask(Box::new(move |it| it.run_reaction(&pc, cb)));
        }
        let empty = self.new_array(Vec::new());
        p.borrow_mut().set_data("__cbs", empty);
    }

    /// The Promise resolution procedure: adopt an inner promise, else fulfil.
    pub fn resolve_promise(&mut self, p: &Gc, v: Value) {
        if promise_state(p) != "pending" {
            return;
        }
        if let Some(inner) = as_promise(&v) {
            // A passthrough reaction settles `p` with the inner's outcome.
            let cb = self.new_object(Some(self.object_proto.clone()));
            cb.borrow_mut().set_data("result", Value::Object(p.clone()));
            self.subscribe(&inner, Value::Object(cb));
            return;
        }
        self.settle_promise(p, "fulfilled", v);
    }

    /// Reject a promise.
    pub fn reject_promise(&mut self, p: &Gc, e: Value) {
        self.settle_promise(p, "rejected", e);
    }

    /// Register a reaction object `{onF?, onR?, result?}` on a target promise.
    fn subscribe(&mut self, target: &Gc, cb: Value) {
        if promise_state(target) == "pending" {
            if let Value::Object(arr) = &read_cbs(target) {
                if let ObjKind::Array(e) = &mut arr.borrow_mut().kind {
                    e.push(cb);
                }
            }
        } else {
            let t = target.clone();
            self.enqueue_microtask(Box::new(move |it| it.run_reaction(&t, cb)));
        }
    }

    /// Run a settled promise's reaction.
    fn run_reaction(&mut self, settled: &Gc, cb: Value) -> Eval<()> {
        // An async-function resume reaction re-drives the parked VM frame
        // instead of calling a JS handler (native fns can't capture the frame).
        if let Value::Num(id) = obj_slot(&cb, "__resume_async") {
            if let Some(rp) = as_promise(&obj_slot(&cb, "__resume_promise")) {
                self.resume_async(id as usize, settled, rp);
            }
            return Ok(());
        }
        let state = promise_state(settled);
        let value = promise_value(settled);
        let handler = if state == "fulfilled" {
            obj_slot(&cb, "onF")
        } else {
            obj_slot(&cb, "onR")
        };
        let result = as_promise(&obj_slot(&cb, "result"));
        if handler.is_callable() {
            match self.call(&handler, Value::Undefined, &[value]) {
                Ok(r) => {
                    if let Some(rg) = result {
                        self.resolve_promise(&rg, r);
                    }
                }
                Err(Abrupt::Throw(e)) => {
                    if let Some(rg) = result {
                        self.reject_promise(&rg, e);
                    }
                }
                Err(other) => return Err(other),
            }
        } else if let Some(rg) = result {
            if state == "fulfilled" {
                self.resolve_promise(&rg, value);
            } else {
                self.reject_promise(&rg, value);
            }
        }
        Ok(())
    }

    /// `promise.then(onF, onR)` → a new promise.
    pub fn promise_then(&mut self, target: &Gc, on_f: Value, on_r: Value) -> Value {
        let result = self.new_promise();
        let cb = self.new_object(Some(self.object_proto.clone()));
        {
            let mut b = cb.borrow_mut();
            b.set_data("onF", on_f);
            b.set_data("onR", on_r);
            b.set_data("result", Value::Object(result.clone()));
        }
        self.subscribe(target, Value::Object(cb));
        Value::Object(result)
    }

    /// A promise already fulfilled with `v` (adopting if `v` is a promise).
    pub fn make_resolved_promise(&mut self, v: Value) -> Value {
        if as_promise(&v).is_some() {
            return v;
        }
        let p = self.new_promise();
        self.resolve_promise(&p, v);
        Value::Object(p)
    }

    /// A promise already rejected with `e`.
    pub fn make_rejected_promise(&mut self, e: Value) -> Value {
        let p = self.new_promise();
        self.reject_promise(&p, e);
        Value::Object(p)
    }

    /// `await v` — synchronously drain the queue until `v` (if a promise)
    /// settles, then return its value or throw its reason.
    pub fn await_value(&mut self, v: Value) -> Eval<Value> {
        let p = match as_promise(&v) {
            Some(p) => p,
            None => return Ok(v),
        };
        let mut guard = 0u64;
        while promise_state(&p) == "pending" {
            if !self.run_one_task()? {
                break;
            }
            guard += 1;
            if guard > 5_000_000 {
                break;
            }
        }
        match promise_state(&p).as_str() {
            "fulfilled" => Ok(promise_value(&p)),
            "rejected" => Err(Abrupt::Throw(promise_value(&p))),
            _ => Ok(Value::Undefined),
        }
    }

    /// Throw a `TypeError`.
    pub fn throw_type<T>(&self, message: impl Into<String>) -> Eval<T> {
        Err(Abrupt::Throw(self.make_error("TypeError", message)))
    }

    /// Throw a `RangeError`.
    pub fn throw_range<T>(&self, message: impl Into<String>) -> Eval<T> {
        Err(Abrupt::Throw(self.make_error("RangeError", message)))
    }

    // ---- top level --------------------------------------------------------

    /// Run a whole program, returning the completion value of the last
    /// statement (handy for testing).
    pub fn run(&mut self, program: &Program) -> Eval<Value> {
        let env = self.global_env.clone();
        self.hoist(&program.body, &env);
        let result = self.exec_stmts(&program.body, &env);
        // Settle any pending Promise reactions / timers before returning.
        self.run_event_loop()?;
        result
    }

    /// Parse and run `src`, returning the completion value or a message.
    pub fn eval_source(src: &str) -> Result<Value, String> {
        let program = super::parser::parse(src).map_err(|e| e.to_string())?;
        let mut interp = Interp::new();
        match interp.run(&program) {
            Ok(v) => Ok(v),
            Err(Abrupt::Throw(v)) => Err(format!("Uncaught {}", interp.display_lossy(&v))),
            Err(_) => Err("illegal abrupt completion at top level".to_string()),
        }
    }

    fn display_lossy(&mut self, v: &Value) -> String {
        self.to_string_v(v).unwrap_or_else(|_| "<error>".to_string())
    }

    /// Parse and run `src` in the given environment (direct `eval`).
    pub fn eval_in_scope(&mut self, src: &str, env: &Env) -> Eval<Value> {
        match super::parser::parse(src) {
            Ok(prog) => {
                self.hoist(&prog.body, env);
                self.exec_stmts(&prog.body, env)
            }
            Err(e) => Err(Abrupt::Throw(self.make_error("SyntaxError", e.to_string()))),
        }
    }

    /// Parse and run `src` in the global environment (indirect `eval`).
    pub fn eval_in_global(&mut self, src: &str) -> Eval<Value> {
        let env = self.global_env.clone();
        self.eval_in_scope(src, &env)
    }

    /// Build a function from `new Function(...params, body)` strings.
    pub fn function_from_strings(&mut self, params: &[String], body: &str) -> Eval<Value> {
        let src = format!("(function anonymous({}) {{ {} }})", params.join(", "), body);
        match super::parser::parse(&src) {
            Ok(prog) => match prog.body.first() {
                Some(Stmt::Expr(e)) => {
                    let env = self.global_env.clone();
                    self.eval_expr(e, &env)
                }
                _ => self.throw_type("invalid Function body"),
            },
            Err(e) => Err(Abrupt::Throw(self.make_error("SyntaxError", e.to_string()))),
        }
    }

    /// Allocate a unique symbol property key.
    pub fn next_symbol_key(&mut self) -> String {
        self.sym_counter += 1;
        format!("@@sym:{}", self.sym_counter)
    }

    // ---- statement execution ---------------------------------------------

    fn exec_stmts(&mut self, stmts: &[Stmt], env: &Env) -> Eval<Value> {
        let mut last = Value::Undefined;
        for s in stmts {
            last = self.eval_stmt(s, env)?;
        }
        Ok(last)
    }

    fn eval_stmt(&mut self, stmt: &Stmt, env: &Env) -> Eval<Value> {
        match stmt {
            Stmt::Expr(e) => self.eval_expr(e, env),
            Stmt::Empty | Stmt::Debugger => Ok(Value::Undefined),
            Stmt::Block(stmts) => {
                let child = new_scope(Some(env.clone()), false);
                self.hoist_block_funcs(stmts, &child);
                self.exec_stmts(stmts, &child)
            }
            Stmt::VarDecl { kind, decls } => {
                let dk = match kind {
                    VarKind::Var => DeclKind::Var,
                    VarKind::Let => DeclKind::Let,
                    VarKind::Const => DeclKind::Const,
                };
                for d in decls {
                    match &d.init {
                        Some(e) => {
                            let v = self.eval_expr(e, env)?;
                            self.declare_pattern(&d.id, v, env, dk)?;
                        }
                        None => {
                            // `var x;` keeps the hoisted binding; `let`/`const x;`
                            // create an `undefined` binding in this scope.
                            if dk != DeclKind::Var {
                                self.declare_pattern(&d.id, Value::Undefined, env, dk)?;
                            }
                        }
                    }
                }
                Ok(Value::Undefined)
            }
            Stmt::FuncDecl(f) => {
                // Already hoisted; ensure the binding exists.
                let name = f.name.clone().unwrap_or_default();
                if !env.borrow().vars.contains_key(&name) {
                    let val = self.make_user_function(Rc::new(f.clone()), env.clone(), false, None, None);
                    scope_declare_var(env, &name, val);
                }
                Ok(Value::Undefined)
            }
            Stmt::ClassDecl(c) => {
                let val = self.eval_class(c, env)?;
                if let Some(name) = &c.name {
                    scope_declare(env, name, val, true);
                }
                Ok(Value::Undefined)
            }
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval_expr(e, env)?,
                    None => Value::Undefined,
                };
                Err(Abrupt::Return(v))
            }
            Stmt::Throw(e) => {
                let v = self.eval_expr(e, env)?;
                Err(Abrupt::Throw(v))
            }
            Stmt::Break(l) => Err(Abrupt::Break(l.clone())),
            Stmt::Continue(l) => Err(Abrupt::Continue(l.clone())),
            Stmt::If { test, cons, alt } => {
                let t = self.eval_expr(test, env)?;
                if to_boolean(&t) {
                    self.eval_stmt(cons, env)
                } else if let Some(a) = alt {
                    self.eval_stmt(a, env)
                } else {
                    Ok(Value::Undefined)
                }
            }
            Stmt::While { test, body } => self.run_while(test, body, env, None),
            Stmt::DoWhile { body, test } => self.run_do_while(body, test, env, None),
            Stmt::For {
                init,
                test,
                update,
                body,
            } => self.run_for(init, test, update, body, env, None),
            Stmt::ForOf { left, right, body } => self.run_for_of(left, right, body, env, None),
            Stmt::ForIn { left, right, body } => self.run_for_in(left, right, body, env, None),
            Stmt::Switch { disc, cases } => self.run_switch(disc, cases, env),
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => self.run_try(block, handler, finalizer, env),
            Stmt::Labeled { label, body } => self.run_labeled(label, body, env),
        }
    }

    fn run_labeled(&mut self, label: &str, body: &Stmt, env: &Env) -> Eval<Value> {
        let result = match body {
            Stmt::While { test, body } => self.run_while(test, body, env, Some(label)),
            Stmt::DoWhile { body, test } => self.run_do_while(body, test, env, Some(label)),
            Stmt::For {
                init,
                test,
                update,
                body,
            } => self.run_for(init, test, update, body, env, Some(label)),
            Stmt::ForOf { left, right, body } => self.run_for_of(left, right, body, env, Some(label)),
            Stmt::ForIn { left, right, body } => self.run_for_in(left, right, body, env, Some(label)),
            other => self.eval_stmt(other, env),
        };
        match result {
            Err(Abrupt::Break(Some(l))) if l == label => Ok(Value::Undefined),
            other => other,
        }
    }

    fn run_while(&mut self, test: &Expr, body: &Stmt, env: &Env, label: Option<&str>) -> Eval<Value> {
        while to_boolean(&self.eval_expr(test, env)?) {
            match self.eval_stmt(body, env) {
                Ok(_) => {}
                Err(Abrupt::Break(l)) if loop_targets(&l, label) => break,
                Err(Abrupt::Continue(l)) if loop_targets(&l, label) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(Value::Undefined)
    }

    fn run_do_while(
        &mut self,
        body: &Stmt,
        test: &Expr,
        env: &Env,
        label: Option<&str>,
    ) -> Eval<Value> {
        loop {
            match self.eval_stmt(body, env) {
                Ok(_) => {}
                Err(Abrupt::Break(l)) if loop_targets(&l, label) => break,
                Err(Abrupt::Continue(l)) if loop_targets(&l, label) => {}
                Err(e) => return Err(e),
            }
            if !to_boolean(&self.eval_expr(test, env)?) {
                break;
            }
        }
        Ok(Value::Undefined)
    }

    fn run_for(
        &mut self,
        init: &Option<Box<ForInit>>,
        test: &Option<Expr>,
        update: &Option<Expr>,
        body: &Stmt,
        env: &Env,
        label: Option<&str>,
    ) -> Eval<Value> {
        let loop_env = new_scope(Some(env.clone()), false);
        if let Some(init) = init {
            match init.as_ref() {
                ForInit::Expr(e) => {
                    self.eval_expr(e, &loop_env)?;
                }
                ForInit::VarDecl { kind, decls } => {
                    for d in decls {
                        let value = match &d.init {
                            Some(e) => self.eval_expr(e, &loop_env)?,
                            None => Value::Undefined,
                        };
                        let dk = match kind {
                            VarKind::Var => DeclKind::Var,
                            VarKind::Let => DeclKind::Let,
                            VarKind::Const => DeclKind::Const,
                        };
                        self.declare_pattern(&d.id, value, &loop_env, dk)?;
                    }
                }
            }
        }
        loop {
            if let Some(t) = test {
                if !to_boolean(&self.eval_expr(t, &loop_env)?) {
                    break;
                }
            }
            // Fresh per-iteration binding scope for `let` semantics.
            let iter_env = new_scope(Some(loop_env.clone()), false);
            match self.eval_stmt(body, &iter_env) {
                Ok(_) => {}
                Err(Abrupt::Break(l)) if loop_targets(&l, label) => break,
                Err(Abrupt::Continue(l)) if loop_targets(&l, label) => {}
                Err(e) => return Err(e),
            }
            if let Some(u) = update {
                self.eval_expr(u, &loop_env)?;
            }
        }
        Ok(Value::Undefined)
    }

    fn run_for_of(
        &mut self,
        left: &ForHead,
        right: &Expr,
        body: &Stmt,
        env: &Env,
        label: Option<&str>,
    ) -> Eval<Value> {
        let iterable = self.eval_expr(right, env)?;
        let items = self.iterate(&iterable)?;
        for item in items {
            let iter_env = new_scope(Some(env.clone()), false);
            self.bind_for_head(left, item, &iter_env)?;
            match self.eval_stmt(body, &iter_env) {
                Ok(_) => {}
                Err(Abrupt::Break(l)) if loop_targets(&l, label) => break,
                Err(Abrupt::Continue(l)) if loop_targets(&l, label) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(Value::Undefined)
    }

    fn run_for_in(
        &mut self,
        left: &ForHead,
        right: &Expr,
        body: &Stmt,
        env: &Env,
        label: Option<&str>,
    ) -> Eval<Value> {
        let obj = self.eval_expr(right, env)?;
        let keys = self.enum_keys(&obj);
        for key in keys {
            let iter_env = new_scope(Some(env.clone()), false);
            self.bind_for_head(left, Value::str(key), &iter_env)?;
            match self.eval_stmt(body, &iter_env) {
                Ok(_) => {}
                Err(Abrupt::Break(l)) if loop_targets(&l, label) => break,
                Err(Abrupt::Continue(l)) if loop_targets(&l, label) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(Value::Undefined)
    }

    fn bind_for_head(&mut self, head: &ForHead, value: Value, env: &Env) -> Eval<()> {
        match head {
            ForHead::Decl { kind, pat } => {
                let dk = match kind {
                    VarKind::Var => DeclKind::Var,
                    VarKind::Let => DeclKind::Let,
                    VarKind::Const => DeclKind::Const,
                };
                self.declare_pattern(pat, value, env, dk)
            }
            ForHead::Pattern(pat) => match pat {
                Pattern::Ident(name) => {
                    self.assign_name(name, value, env)?;
                    Ok(())
                }
                Pattern::Member(e) => {
                    let r = self.resolve_ref(e, env)?;
                    self.set_ref(&r, value)
                }
                other => self.declare_pattern(other, value, env, DeclKind::Let),
            },
        }
    }

    fn run_switch(&mut self, disc: &Expr, cases: &[SwitchCase], env: &Env) -> Eval<Value> {
        let d = self.eval_expr(disc, env)?;
        let child = new_scope(Some(env.clone()), false);
        // Hoist block-scoped function declarations across the switch body.
        for case in cases {
            self.hoist_block_funcs(&case.body, &child);
        }
        let mut matched = None;
        for (i, case) in cases.iter().enumerate() {
            if let Some(test) = &case.test {
                let t = self.eval_expr(test, &child)?;
                if strict_eq(&d, &t) {
                    matched = Some(i);
                    break;
                }
            }
        }
        let start = match matched {
            Some(i) => i,
            None => cases
                .iter()
                .position(|c| c.test.is_none())
                .unwrap_or(cases.len()),
        };
        for case in cases.iter().skip(start) {
            match self.exec_stmts(&case.body, &child) {
                Ok(_) => {}
                Err(Abrupt::Break(None)) => return Ok(Value::Undefined),
                Err(e) => return Err(e),
            }
        }
        Ok(Value::Undefined)
    }

    fn run_try(
        &mut self,
        block: &[Stmt],
        handler: &Option<Catch>,
        finalizer: &Option<Vec<Stmt>>,
        env: &Env,
    ) -> Eval<Value> {
        let try_env = new_scope(Some(env.clone()), false);
        self.hoist_block_funcs(block, &try_env);
        let mut result = self.exec_stmts(block, &try_env);

        if let Err(Abrupt::Throw(v)) = &result {
            if let Some(cat) = handler {
                let cenv = new_scope(Some(env.clone()), false);
                if let Some(p) = &cat.param {
                    self.declare_pattern(p, v.clone(), &cenv, DeclKind::Let)?;
                }
                self.hoist_block_funcs(&cat.body, &cenv);
                result = self.exec_stmts(&cat.body, &cenv);
            }
        }

        if let Some(fin) = finalizer {
            let fenv = new_scope(Some(env.clone()), false);
            self.hoist_block_funcs(fin, &fenv);
            // An abrupt finalizer overrides the protected/handler result.
            self.exec_stmts(fin, &fenv)?;
        }
        result
    }

    // ---- hoisting ---------------------------------------------------------

    /// Function-scope hoisting: bind `var` names (to `undefined`) and function
    /// declarations (to their function value) before execution.
    fn hoist(&mut self, stmts: &[Stmt], env: &Env) {
        for s in stmts {
            if let Stmt::FuncDecl(f) = s {
                if let Some(name) = &f.name {
                    let val =
                        self.make_user_function(Rc::new(f.clone()), env.clone(), false, None, None);
                    scope_declare_var(env, name, val);
                }
            }
        }
        self.collect_vars(stmts, env);
    }

    fn hoist_block_funcs(&mut self, stmts: &[Stmt], env: &Env) {
        for s in stmts {
            if let Stmt::FuncDecl(f) = s {
                if let Some(name) = &f.name {
                    let val =
                        self.make_user_function(Rc::new(f.clone()), env.clone(), false, None, None);
                    scope_declare(env, name, val, true);
                }
            }
        }
    }

    /// Recursively declare `var` names found in `stmts` (not descending into
    /// nested function bodies).
    fn collect_vars(&mut self, stmts: &[Stmt], env: &Env) {
        for s in stmts {
            self.collect_vars_stmt(s, env);
        }
    }

    fn collect_vars_stmt(&mut self, s: &Stmt, env: &Env) {
        match s {
            Stmt::VarDecl {
                kind: VarKind::Var,
                decls,
            } => {
                for d in decls {
                    for name in pattern_names(&d.id) {
                        scope_declare_var(env, &name, Value::Undefined);
                    }
                }
            }
            Stmt::Block(inner) => self.collect_vars(inner, env),
            Stmt::If { cons, alt, .. } => {
                self.collect_vars_stmt(cons, env);
                if let Some(a) = alt {
                    self.collect_vars_stmt(a, env);
                }
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::Labeled { body, .. } => self.collect_vars_stmt(body, env),
            Stmt::For { init, body, .. } => {
                if let Some(init) = init {
                    if let ForInit::VarDecl {
                        kind: VarKind::Var,
                        decls,
                    } = init.as_ref()
                    {
                        for d in decls {
                            for name in pattern_names(&d.id) {
                                scope_declare_var(env, &name, Value::Undefined);
                            }
                        }
                    }
                }
                self.collect_vars_stmt(body, env);
            }
            Stmt::ForIn { left, body, .. } | Stmt::ForOf { left, body, .. } => {
                if let ForHead::Decl {
                    kind: VarKind::Var,
                    pat,
                } = left.as_ref()
                {
                    for name in pattern_names(pat) {
                        scope_declare_var(env, &name, Value::Undefined);
                    }
                }
                self.collect_vars_stmt(body, env);
            }
            Stmt::Switch { cases, .. } => {
                for c in cases {
                    self.collect_vars(&c.body, env);
                }
            }
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
                self.collect_vars(block, env);
                if let Some(h) = handler {
                    self.collect_vars(&h.body, env);
                }
                if let Some(f) = finalizer {
                    self.collect_vars(f, env);
                }
            }
            _ => {}
        }
    }

    // ---- expression evaluation -------------------------------------------

    fn eval_expr(&mut self, expr: &Expr, env: &Env) -> Eval<Value> {
        if !matches!(expr, Expr::Member { .. } | Expr::Call { .. }) {
            self.short_circuit = false;
        }
        match expr {
            Expr::Num(n) => Ok(Value::Num(*n)),
            Expr::Str(s) => Ok(Value::str(s.clone())),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),
            Expr::BigInt(s) => Ok(Value::str(format!("{s}n"))), // no BigInt runtime yet
            Expr::Regex { body, flags } => Ok(self.make_regexp(body, flags)),
            Expr::Ident(name) => self.lookup_ident(name, env),
            Expr::This => Ok(scope_this(env)),
            Expr::Super => Ok(Value::Undefined),
            Expr::Template { quasis, exprs } => self.eval_template(quasis, exprs, env),
            Expr::TaggedTemplate { tag, quasis, exprs } => {
                let tagf = self.eval_expr(tag, env)?;
                let strings = self.new_array(quasis.iter().map(|q| Value::str(q.clone())).collect());
                let raw = self.new_array(quasis.iter().map(|q| Value::str(q.clone())).collect());
                self.set_member(&strings, "raw", raw)?;
                let mut argv = vec![strings];
                for e in exprs {
                    argv.push(self.eval_expr(e, env)?);
                }
                self.call(&tagf, Value::Undefined, &argv)
            }
            Expr::Array(elems) => self.eval_array(elems, env),
            Expr::Object(props) => self.eval_object(props, env),
            Expr::Func(f) => Ok(self.make_user_function(
                (**f).clone().into(),
                env.clone(),
                false,
                None,
                None,
            )),
            Expr::Arrow(f) => {
                let this = scope_this(env);
                Ok(self.make_user_function((**f).clone().into(), env.clone(), true, Some(this), None))
            }
            Expr::Class(c) => self.eval_class(c, env),
            Expr::Unary { op, arg } => self.eval_unary(*op, arg, env),
            Expr::Update { op, prefix, arg } => self.eval_update(*op, *prefix, arg, env),
            Expr::Binary { op, left, right } => {
                let l = self.eval_expr(left, env)?;
                let r = self.eval_expr(right, env)?;
                self.apply_binop(*op, l, r)
            }
            Expr::Logical { op, left, right } => self.eval_logical(*op, left, right, env),
            Expr::Conditional { test, cons, alt } => {
                if to_boolean(&self.eval_expr(test, env)?) {
                    self.eval_expr(cons, env)
                } else {
                    self.eval_expr(alt, env)
                }
            }
            Expr::Assign { op, target, value } => self.eval_assign(*op, target, value, env),
            Expr::Sequence(items) => {
                let mut last = Value::Undefined;
                for e in items {
                    last = self.eval_expr(e, env)?;
                }
                Ok(last)
            }
            Expr::Member {
                object,
                property,
                optional,
                ..
            } => {
                if matches!(**object, Expr::Super) {
                    let key = self.member_key(property, env)?;
                    return self.super_get(&key, env);
                }
                let base = self.eval_expr(object, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined);
                }
                if *optional && base.is_nullish() {
                    self.short_circuit = true;
                    return Ok(Value::Undefined);
                }
                let key = self.member_key(property, env)?;
                self.get_member(&base, &key)
            }
            Expr::Call {
                callee,
                args,
                optional,
            } => self.eval_call(callee, args, *optional, env),
            Expr::New { callee, args } => self.eval_new(callee, args, env),
            Expr::Spread(_) => self.throw_type("unexpected spread"),
            Expr::Yield { arg, delegate } => {
                if self.gen_yield_stack.is_empty() {
                    return self.throw_type("yield outside a generator");
                }
                let value = match arg {
                    Some(e) => self.eval_expr(e, env)?,
                    None => Value::Undefined,
                };
                let items = if *delegate {
                    self.iterate(&value)?
                } else {
                    vec![value]
                };
                let buf = self.gen_yield_stack.last_mut().unwrap();
                buf.extend(items);
                if buf.len() > 1_000_000 {
                    return self.throw_range("generator produced too many values");
                }
                Ok(Value::Undefined)
            }
            Expr::Await(e) => {
                let v = self.eval_expr(e, env)?;
                self.await_value(v)
            }
        }
    }

    fn lookup_ident(&mut self, name: &str, env: &Env) -> Eval<Value> {
        if name == "undefined" {
            return Ok(Value::Undefined);
        }
        if let Some(v) = scope_get(env, name) {
            return Ok(v);
        }
        // Fall back to a global-object property.
        let g = self.global.clone();
        if g.borrow().get_own(name).is_some() {
            return self.get_member(&Value::Object(g), name);
        }
        Err(Abrupt::Throw(self.make_error(
            "ReferenceError",
            format!("{name} is not defined"),
        )))
    }

    fn member_key(&mut self, prop: &MemberProp, env: &Env) -> Eval<String> {
        match prop {
            MemberProp::Ident(s) => Ok(s.clone()),
            MemberProp::Computed(e) => {
                let v = self.eval_expr(e, env)?;
                self.property_key(&v)
            }
        }
    }

    fn eval_template(&mut self, quasis: &[String], exprs: &[Expr], env: &Env) -> Eval<Value> {
        let mut out = String::new();
        for (i, q) in quasis.iter().enumerate() {
            out.push_str(q);
            if i < exprs.len() {
                let v = self.eval_expr(&exprs[i], env)?;
                out.push_str(&self.to_string_v(&v)?);
            }
        }
        Ok(Value::str(out))
    }

    fn eval_array(&mut self, elems: &[Option<Expr>], env: &Env) -> Eval<Value> {
        let mut out = Vec::new();
        for el in elems {
            match el {
                None => out.push(Value::Undefined),
                Some(Expr::Spread(inner)) => {
                    let v = self.eval_expr(inner, env)?;
                    out.extend(self.iterate(&v)?);
                }
                Some(e) => out.push(self.eval_expr(e, env)?),
            }
        }
        Ok(self.new_array(out))
    }

    fn eval_object(&mut self, props: &[Prop], env: &Env) -> Eval<Value> {
        let obj = self.new_object(Some(self.object_proto.clone()));
        for p in props {
            match p.kind {
                PropKind::Spread => {
                    if let PropValue::Spread(e) = &p.value {
                        let src = self.eval_expr(e, env)?;
                        for k in self.enum_keys(&src) {
                            let v = self.get_member(&src, &k)?;
                            obj.borrow_mut().set_data(&k, v);
                        }
                    }
                }
                PropKind::Init | PropKind::Method => {
                    let key = self.prop_key_str(&p.key, env)?;
                    if let PropValue::Expr(e) = &p.value {
                        let v = self.eval_expr(e, env)?;
                        obj.borrow_mut().set_data(&key, v);
                    }
                }
                PropKind::Get | PropKind::Set => {
                    let key = self.prop_key_str(&p.key, env)?;
                    if let PropValue::Expr(e) = &p.value {
                        let f = self.eval_expr(e, env)?;
                        let fgc = f.as_object().cloned();
                        let mut b = obj.borrow_mut();
                        let existing = b.get_own(&key).cloned();
                        let (mut get, mut set) = match existing {
                            Some(PropDesc::Accessor { get, set }) => (get, set),
                            _ => (None, None),
                        };
                        if p.kind == PropKind::Get {
                            get = fgc;
                        } else {
                            set = fgc;
                        }
                        b.set_own(&key, PropDesc::Accessor { get, set });
                    }
                }
            }
        }
        Ok(Value::Object(obj))
    }

    fn prop_key_str(&mut self, key: &PropKey, env: &Env) -> Eval<String> {
        match key {
            PropKey::Ident(s) | PropKey::Str(s) => Ok(s.clone()),
            PropKey::Num(n) => Ok(num_to_str(*n)),
            PropKey::Computed(e) => {
                let v = self.eval_expr(e, env)?;
                self.property_key(&v)
            }
        }
    }

    fn eval_unary(&mut self, op: UnOp, arg: &Expr, env: &Env) -> Eval<Value> {
        // `typeof x` on an undeclared identifier yields "undefined", not an error.
        if matches!(op, UnOp::Typeof) {
            if let Expr::Ident(name) = arg {
                if name != "undefined" && !scope_has(env, name) && self.global.borrow().get_own(name).is_none() {
                    return Ok(Value::str("undefined"));
                }
            }
        }
        if matches!(op, UnOp::Delete) {
            if let Expr::Member { object, property, .. } = arg {
                let base = self.eval_expr(object, env)?;
                let key = self.member_key(property, env)?;
                if let Value::Object(o) = &base {
                    let removed = {
                        let mut b = o.borrow_mut();
                        if let ObjKind::Array(elems) = &mut b.kind {
                            if let Ok(i) = key.parse::<usize>() {
                                if i < elems.len() {
                                    elems[i] = Value::Undefined;
                                }
                            }
                        }
                        b.remove_own(&key)
                    };
                    let _ = removed;
                }
                return Ok(Value::Bool(true));
            }
            return Ok(Value::Bool(true));
        }
        let v = self.eval_expr(arg, env)?;
        Ok(match op {
            UnOp::Minus => Value::Num(-self.to_number(&v)?),
            UnOp::Plus => Value::Num(self.to_number(&v)?),
            UnOp::Not => Value::Bool(!to_boolean(&v)),
            UnOp::BitNot => Value::Num(!to_int32(self.to_number(&v)?) as f64),
            UnOp::Typeof => Value::str(type_of(&v)),
            UnOp::Void => Value::Undefined,
            UnOp::Delete => Value::Bool(true),
        })
    }

    fn eval_update(&mut self, op: UpdateOp, prefix: bool, arg: &Expr, env: &Env) -> Eval<Value> {
        let r = self.resolve_ref(arg, env)?;
        let cur = self.get_ref(&r)?;
        let old = self.to_number(&cur)?;
        let new = match op {
            UpdateOp::Inc => old + 1.0,
            UpdateOp::Dec => old - 1.0,
        };
        self.set_ref(&r, Value::Num(new))?;
        Ok(Value::Num(if prefix { new } else { old }))
    }

    fn eval_logical(&mut self, op: LogicalOp, left: &Expr, right: &Expr, env: &Env) -> Eval<Value> {
        let l = self.eval_expr(left, env)?;
        match op {
            LogicalOp::And => {
                if to_boolean(&l) {
                    self.eval_expr(right, env)
                } else {
                    Ok(l)
                }
            }
            LogicalOp::Or => {
                if to_boolean(&l) {
                    Ok(l)
                } else {
                    self.eval_expr(right, env)
                }
            }
            LogicalOp::Nullish => {
                if l.is_nullish() {
                    self.eval_expr(right, env)
                } else {
                    Ok(l)
                }
            }
        }
    }

    fn eval_assign(&mut self, op: AssignOp, target: &Expr, value: &Expr, env: &Env) -> Eval<Value> {
        // Destructuring assignment to an array/object literal target.
        if matches!(op, AssignOp::Assign) && matches!(target, Expr::Array(_) | Expr::Object(_)) {
            let v = self.eval_expr(value, env)?;
            self.assign_destructure(target, v.clone(), env)?;
            return Ok(v);
        }

        let r = self.resolve_ref(target, env)?;
        let result = match op {
            AssignOp::Assign => self.eval_expr(value, env)?,
            AssignOp::And => {
                let cur = self.get_ref(&r)?;
                if !to_boolean(&cur) {
                    return Ok(cur);
                }
                self.eval_expr(value, env)?
            }
            AssignOp::Or => {
                let cur = self.get_ref(&r)?;
                if to_boolean(&cur) {
                    return Ok(cur);
                }
                self.eval_expr(value, env)?
            }
            AssignOp::Nullish => {
                let cur = self.get_ref(&r)?;
                if !cur.is_nullish() {
                    return Ok(cur);
                }
                self.eval_expr(value, env)?
            }
            other => {
                let cur = self.get_ref(&r)?;
                let rhs = self.eval_expr(value, env)?;
                self.apply_binop(compound_binop(other), cur, rhs)?
            }
        };
        self.set_ref(&r, result.clone())?;
        Ok(result)
    }

    // ---- calls & new ------------------------------------------------------

    fn eval_call(&mut self, callee: &Expr, args: &[Expr], optional: bool, env: &Env) -> Eval<Value> {
        // `super(...)` — invoke the parent constructor with the current `this`.
        if matches!(callee, Expr::Super) {
            return self.eval_super_call(args, env);
        }
        // `super.method(...)` — invoke an inherited method with the current `this`.
        if let Expr::Member { object, property, .. } = callee {
            if matches!(**object, Expr::Super) {
                let key = self.member_key(property, env)?;
                let func = self.super_get(&key, env)?;
                let this = scope_this(env);
                let argv = self.eval_args(args, env)?;
                return self.call(&func, this, &argv);
            }
        }
        // Direct `eval(src)` runs in the current (calling) scope.
        if matches!(callee, Expr::Ident(n) if n == "eval") {
            let argv = self.eval_args(args, env)?;
            return match argv.into_iter().next() {
                Some(Value::Str(src)) => self.eval_in_scope(&src, env),
                Some(other) => Ok(other),
                None => Ok(Value::Undefined),
            };
        }
        // Member call: bind `this` to the receiver.
        if let Expr::Member {
            object,
            property,
            optional: member_opt,
            ..
        } = callee
        {
            let base = self.eval_expr(object, env)?;
            if self.short_circuit {
                return Ok(Value::Undefined);
            }
            if *member_opt && base.is_nullish() {
                self.short_circuit = true;
                return Ok(Value::Undefined);
            }
            let key = self.member_key(property, env)?;
            let func = self.get_member(&base, &key)?;
            if optional && func.is_nullish() {
                self.short_circuit = true;
                return Ok(Value::Undefined);
            }
            let argv = self.eval_args(args, env)?;
            return self.call(&func, base, &argv);
        }

        let func = self.eval_expr(callee, env)?;
        if self.short_circuit {
            return Ok(Value::Undefined);
        }
        if optional && func.is_nullish() {
            self.short_circuit = true;
            return Ok(Value::Undefined);
        }
        let argv = self.eval_args(args, env)?;
        self.call(&func, Value::Undefined, &argv)
    }

    fn eval_args(&mut self, args: &[Expr], env: &Env) -> Eval<Vec<Value>> {
        let mut out = Vec::new();
        for a in args {
            if let Expr::Spread(inner) = a {
                let v = self.eval_expr(inner, env)?;
                out.extend(self.iterate(&v)?);
            } else {
                out.push(self.eval_expr(a, env)?);
            }
        }
        Ok(out)
    }

    /// Call a function value with an explicit `this` and arguments.
    pub fn call(&mut self, func: &Value, this: Value, args: &[Value]) -> Eval<Value> {
        let callable = match func {
            Value::Object(o) => match &o.borrow().kind {
                ObjKind::Function(c) => c.clone(),
                _ => return self.throw_type("value is not a function"),
            },
            _ => {
                return self.throw_type(format!("{} is not a function", type_of(func)));
            }
        };
        match callable {
            Callable::Native { f, .. } => f(self, this, args),
            Callable::Bound {
                target,
                bound_this,
                bound_args,
            } => {
                let mut all = bound_args;
                all.extend_from_slice(args);
                self.call(&target, *bound_this, &all)
            }
            Callable::User {
                def,
                env,
                is_arrow,
                captured_this,
                home,
            } => {
                let act = new_scope(Some(env), true);
                let this_val = if is_arrow {
                    captured_this.map(|b| *b).unwrap_or(Value::Undefined)
                } else {
                    self.coerce_this(this)
                };
                {
                    let mut b = act.borrow_mut();
                    b.this_val = Some(this_val);
                    if !is_arrow {
                        b.home = home.map(|h| *h);
                        b.current_fn = Some(func.clone());
                    }
                }
                if !is_arrow {
                    // The classic `arguments` object (a real array here).
                    let arguments = self.new_array(args.to_vec());
                    scope_declare(&act, "arguments", arguments, true);
                }
                self.bind_params(&def.params, args, &act)?;
                if def.is_generator {
                    return self.run_generator(&def, &act);
                }
                // Preferred async path: compile to suspendable bytecode so
                // `await` yields to the event loop (spec microtask ordering).
                if def.is_async {
                    if let FuncBody::Block(stmts) = &def.body {
                        if let Some(chunk) = compile::compile_body(stmts) {
                            self.hoist(stmts, &act);
                            let rp = self.new_promise();
                            let frame = Frame::new(Rc::new(chunk), act.clone());
                            self.drive_async_frame(frame, rp.clone());
                            return Ok(Value::Object(rp));
                        }
                    }
                }
                let completion = match &def.body {
                    FuncBody::Block(stmts) => {
                        self.hoist(stmts, &act);
                        match self.exec_stmts(stmts, &act) {
                            Ok(_) => Ok(Value::Undefined),
                            Err(Abrupt::Return(v)) => Ok(v),
                            Err(e) => Err(e),
                        }
                    }
                    FuncBody::Expr(e) => self.eval_expr(e, &act),
                };
                // An `async` function returns a promise of its completion.
                if def.is_async {
                    match completion {
                        Ok(v) => Ok(self.make_resolved_promise(v)),
                        Err(Abrupt::Throw(e)) => Ok(self.make_rejected_promise(e)),
                        Err(other) => Err(other),
                    }
                } else {
                    completion
                }
            }
        }
    }

    fn coerce_this(&self, this: Value) -> Value {
        if this.is_nullish() {
            Value::Object(self.global.clone())
        } else {
            this
        }
    }

    /// Eagerly run a `function*` body, collecting `yield`ed values, and return a
    /// generator object that iterates them. Finite generators only (a large cap
    /// guards against accidental infinite loops).
    fn run_generator(&mut self, def: &Rc<Func>, act: &Env) -> Eval<Value> {
        // Preferred path: compile the body to suspendable bytecode so the
        // generator is **lazy** (works for infinite generators). The activation
        // already has its parameters bound; hoist `var`/function decls into it.
        if let FuncBody::Block(stmts) = &def.body {
            if let Some(chunk) = compile::compile_body(stmts) {
                self.hoist(stmts, act);
                let id = self.gen_frames.len();
                self.gen_frames
                    .push(Some(Frame::new(Rc::new(chunk), act.clone())));
                let gen = self.new_object(Some(self.generator_proto.clone()));
                {
                    let mut b = gen.borrow_mut();
                    b.class = "Generator";
                    b.set_data("__genid", Value::Num(id as f64));
                }
                return Ok(Value::Object(gen));
            }
        }
        // Fallback: eager model (run the whole body, buffering every `yield`).
        self.gen_yield_stack.push(Vec::new());
        let outcome = match &def.body {
            FuncBody::Block(stmts) => {
                self.hoist(stmts, act);
                self.exec_stmts(stmts, act)
            }
            FuncBody::Expr(e) => self.eval_expr(e, act),
        };
        let buffer = self.gen_yield_stack.pop().unwrap_or_default();
        let ret = match outcome {
            Ok(_) => Value::Undefined,
            Err(Abrupt::Return(v)) => v,
            Err(e) => return Err(e),
        };
        let gen = self.new_object(Some(self.generator_proto.clone()));
        let items = self.new_array(buffer);
        {
            let mut b = gen.borrow_mut();
            b.class = "Generator";
            b.set_data("__items", items);
            b.set_data("__index", Value::Num(0.0));
            b.set_data("__return", ret);
        }
        Ok(Value::Object(gen))
    }

    // ─── bytecode VM (lazy generators & spec-correct async) ──────────────────

    /// Build a `{ value, done }` iterator-result object.
    fn iter_result(&self, value: Value, done: bool) -> Value {
        let o = self.new_object(Some(self.object_proto.clone()));
        {
            let mut b = o.borrow_mut();
            b.set_data("value", value);
            b.set_data("done", Value::Bool(done));
        }
        Value::Object(o)
    }

    /// Wrap finished values in an *eager* generator-iterator (a `Generator`
    /// object whose `next()` walks `__items`). Used to give finite built-in
    /// collections an iterator without depending on `Symbol.iterator`.
    pub fn make_eager_generator(&self, values: Vec<Value>) -> Value {
        let gen = self.new_object(Some(self.generator_proto.clone()));
        let items = self.new_array(values);
        {
            let mut b = gen.borrow_mut();
            b.class = "Generator";
            b.set_data("__items", items);
            b.set_data("__index", Value::Num(0.0));
            b.set_data("__return", Value::Undefined);
        }
        Value::Object(gen)
    }

    /// Resume the lazy generator with frame index `id`, feeding `sent` as the
    /// value of the suspended `yield`. Returns a `{ value, done }` object.
    pub fn generator_next(&mut self, id: usize, sent: Value) -> Eval<Value> {
        let mut frame = match self.gen_frames.get_mut(id).and_then(Option::take) {
            Some(f) => f,
            None => return Ok(self.iter_result(Value::Undefined, true)),
        };
        if frame.started {
            frame.stack.push(sent); // becomes the result of the `yield` expr
        }
        frame.started = true;
        match self.run_frame(&mut frame) {
            Step::Yield(v) => {
                self.gen_frames[id] = Some(frame); // keep the suspended frame
                Ok(self.iter_result(v, false))
            }
            Step::Done(v) => Ok(self.iter_result(v, true)),
            Step::Throw(e) => Err(Abrupt::Throw(e)),
            // `await` cannot appear in a (non-async) generator body.
            Step::Await(_) => self.throw_type("await outside async function"),
        }
    }

    /// Force-finish a lazy generator (`gen.return(v)`).
    pub fn generator_return(&mut self, id: usize, value: Value) -> Value {
        if let Some(slot) = self.gen_frames.get_mut(id) {
            *slot = None;
        }
        self.iter_result(value, true)
    }

    /// Stash a suspended frame, returning its index in `gen_frames`.
    fn park_frame(&mut self, frame: Frame) -> usize {
        let id = self.gen_frames.len();
        self.gen_frames.push(Some(frame));
        id
    }

    /// Drive an **async** function's frame, settling `rp` (the promise the call
    /// returned) on completion. At each `await` the frame is parked and a
    /// resume reaction is registered on the awaited promise, so control returns
    /// to the event loop — `await` truly yields, matching the spec ordering.
    pub fn drive_async_frame(&mut self, mut frame: Frame, rp: Gc) {
        match self.run_frame(&mut frame) {
            Step::Done(v) => self.resolve_promise(&rp, v),
            Step::Throw(e) => self.reject_promise(&rp, e),
            Step::Yield(_) => {
                let err = self.make_error("SyntaxError", "yield in async function");
                self.reject_promise(&rp, err);
            }
            Step::Await(awaited) => {
                let ap = match as_promise(&awaited) {
                    Some(p) => p,
                    None => match self.make_resolved_promise(awaited) {
                        Value::Object(o) => o,
                        _ => self.new_promise(),
                    },
                };
                let id = self.park_frame(frame);
                let cb = self.new_object(Some(self.object_proto.clone()));
                {
                    let mut b = cb.borrow_mut();
                    b.set_data("__resume_async", Value::Num(id as f64));
                    b.set_data("__resume_promise", Value::Object(rp));
                }
                self.subscribe(&ap, Value::Object(cb));
            }
        }
    }

    /// Resume a parked async frame after its awaited promise settled.
    fn resume_async(&mut self, id: usize, settled: &Gc, rp: Gc) {
        let Some(mut frame) = self.gen_frames.get_mut(id).and_then(Option::take) else {
            return;
        };
        if promise_state(settled) == "fulfilled" {
            // The settled value becomes the result of the `await` expression.
            frame.stack.push(promise_value(settled));
            self.drive_async_frame(frame, rp);
        } else {
            // A rejected await throws at the await point: let an enclosing
            // `try`/`catch` handle it; if uncaught, the async function rejects.
            match frame.take_handler(promise_value(settled)) {
                Ok(()) => self.drive_async_frame(frame, rp),
                Err(reason) => self.reject_promise(&rp, reason),
            }
        }
    }

    /// Obtain a **step-wise** iterator for `iterable`, preserving laziness:
    /// a generator is its own iterator; a custom `Symbol.iterator` is honoured;
    /// only genuinely finite built-ins are materialised eagerly.
    fn vm_get_iterator(&mut self, iterable: &Value) -> Eval<Value> {
        if let Value::Object(o) = iterable {
            if o.borrow().class == "Generator" {
                return Ok(iterable.clone());
            }
        }
        let iter_fn = self.get_member(iterable, "@@iterator")?;
        if iter_fn.is_callable() {
            return self.call(&iter_fn, iterable.clone(), &[]);
        }
        let values = self.iterate(iterable)?;
        Ok(self.make_eager_generator(values))
    }

    /// Apply a value-based unary operator (`delete` is excluded by the compiler).
    fn vm_unary(&mut self, op: UnOp, a: Value) -> Eval<Value> {
        Ok(match op {
            UnOp::Not => Value::Bool(!to_boolean(&a)),
            UnOp::Minus => Value::Num(-self.to_number(&a)?),
            UnOp::Plus => Value::Num(self.to_number(&a)?),
            UnOp::BitNot => Value::Num(f64::from(!to_int32(self.to_number(&a)?))),
            UnOp::Typeof => Value::str(type_of(&a)),
            UnOp::Void => Value::Undefined,
            UnOp::Delete => Value::Bool(true),
        })
    }

    /// Run `frame` until it suspends (`yield`/`await`), returns, or throws.
    ///
    /// The loop owns only control flow, the operand stack and the scope; every
    /// value operation is delegated to the interpreter, so semantics match the
    /// tree-walker exactly. A thrown [`Abrupt`] from a delegate becomes
    /// [`Step::Throw`].
    pub fn run_frame(&mut self, frame: &mut Frame) -> Step {
        macro_rules! delegate {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    // A thrown value unwinds to the innermost `try` handler;
                    // with none installed it escapes the frame.
                    Err(Abrupt::Throw(t)) => match frame.take_handler(t) {
                        Ok(()) => continue,
                        Err(t) => return Step::Throw(t),
                    },
                    Err(_) => return Step::Throw(Value::Undefined),
                }
            };
        }
        loop {
            let op = match frame.chunk.code.get(frame.ip) {
                Some(op) => op.clone(),
                None => return Step::Done(Value::Undefined),
            };
            frame.ip += 1;
            match op {
                Op::EvalExpr(i) => {
                    let e = frame.chunk.exprs[i as usize].clone();
                    let v = delegate!(self.eval_expr(&e, &frame.env));
                    frame.stack.push(v);
                }
                Op::PushConst(i) => frame.stack.push(frame.chunk.consts[i as usize].clone()),
                Op::PushUndefined => frame.stack.push(Value::Undefined),
                Op::PushNull => frame.stack.push(Value::Null),
                Op::PushTrue => frame.stack.push(Value::Bool(true)),
                Op::PushFalse => frame.stack.push(Value::Bool(false)),
                Op::Pop => {
                    frame.pop();
                }
                Op::Dup => frame.stack.push(frame.peek()),
                Op::Swap => {
                    let n = frame.stack.len();
                    if n >= 2 {
                        frame.stack.swap(n - 1, n - 2);
                    }
                }
                Op::LoadName(i) => {
                    let name = &frame.chunk.names[i as usize];
                    frame
                        .stack
                        .push(scope_get(&frame.env, name).unwrap_or(Value::Undefined));
                }
                Op::StoreName(i) => {
                    let v = frame.pop();
                    let name = frame.chunk.names[i as usize].clone();
                    if scope_set(&frame.env, &name, v.clone()) == SetOutcome::NotFound {
                        scope_declare(&self.global_env, &name, v, true);
                    }
                }
                Op::DeclareVar(i) => {
                    let v = frame.pop();
                    scope_var_set(&frame.env, &frame.chunk.names[i as usize], v);
                }
                Op::DeclareLet(i) => {
                    let v = frame.pop();
                    scope_declare(&frame.env, &frame.chunk.names[i as usize], v, true);
                }
                Op::DeclareConst(i) => {
                    let v = frame.pop();
                    scope_declare(&frame.env, &frame.chunk.names[i as usize], v, false);
                }
                Op::BindPattern(i) => {
                    let (pat, kind) = frame.chunk.patterns[i as usize].clone();
                    let value = frame.pop();
                    let dk = match kind {
                        VarKind::Var => DeclKind::Var,
                        VarKind::Let => DeclKind::Let,
                        VarKind::Const => DeclKind::Const,
                    };
                    let env = frame.env.clone();
                    delegate!(self.declare_pattern(&pat, value, &env, dk));
                }
                Op::PushScope => frame.env = new_scope(Some(frame.env.clone()), false),
                Op::PopScope => {
                    let parent = frame.env.borrow().parent.clone();
                    if let Some(p) = parent {
                        frame.env = p;
                    }
                }
                Op::Binary(bop) => {
                    let b = frame.pop();
                    let a = frame.pop();
                    let v = delegate!(self.apply_binop(bop, a, b));
                    frame.stack.push(v);
                }
                Op::Unary(uop) => {
                    let a = frame.pop();
                    let v = delegate!(self.vm_unary(uop, a));
                    frame.stack.push(v);
                }
                Op::GetProp(i) => {
                    let obj = frame.pop();
                    let name = frame.chunk.names[i as usize].clone();
                    let v = delegate!(self.get_member(&obj, &name));
                    frame.stack.push(v);
                }
                Op::SetProp(i) => {
                    let v = frame.pop();
                    let obj = frame.pop();
                    let name = frame.chunk.names[i as usize].clone();
                    delegate!(self.set_member(&obj, &name, v.clone()));
                    frame.stack.push(v);
                }
                Op::GetIndex => {
                    let key = frame.pop();
                    let obj = frame.pop();
                    let ks = delegate!(self.property_key(&key));
                    let v = delegate!(self.get_member(&obj, &ks));
                    frame.stack.push(v);
                }
                Op::SetIndex => {
                    let v = frame.pop();
                    let key = frame.pop();
                    let obj = frame.pop();
                    let ks = delegate!(self.property_key(&key));
                    delegate!(self.set_member(&obj, &ks, v.clone()));
                    frame.stack.push(v);
                }
                Op::Call(argc) => {
                    let args = frame.pop_n(argc);
                    let callee = frame.pop();
                    let v = delegate!(self.call(&callee, Value::Undefined, &args));
                    frame.stack.push(v);
                }
                Op::CallMethod(i, argc) => {
                    let args = frame.pop_n(argc);
                    let obj = frame.pop();
                    let name = frame.chunk.names[i as usize].clone();
                    let f = delegate!(self.get_member(&obj, &name));
                    let v = delegate!(self.call(&f, obj, &args));
                    frame.stack.push(v);
                }
                Op::New(argc) => {
                    let args = frame.pop_n(argc);
                    let ctor = frame.pop();
                    let v = delegate!(self.construct(&ctor, &args));
                    frame.stack.push(v);
                }
                Op::CallApply => {
                    let args = array_elems(&frame.pop());
                    let callee = frame.pop();
                    let v = delegate!(self.call(&callee, Value::Undefined, &args));
                    frame.stack.push(v);
                }
                Op::CallMethodApply(i) => {
                    let args = array_elems(&frame.pop());
                    let obj = frame.pop();
                    let name = frame.chunk.names[i as usize].clone();
                    let f = delegate!(self.get_member(&obj, &name));
                    let v = delegate!(self.call(&f, obj, &args));
                    frame.stack.push(v);
                }
                Op::MakeArray(n) => {
                    let items = frame.pop_n(n);
                    frame.stack.push(self.new_array(items));
                }
                Op::NewArray => frame.stack.push(self.new_array(Vec::new())),
                Op::ArrayAppend => {
                    let v = frame.pop();
                    if let Some(Value::Object(o)) = frame.stack.last().cloned() {
                        if let ObjKind::Array(vec) = &mut o.borrow_mut().kind {
                            vec.push(v);
                        }
                    }
                }
                Op::ArrayAppendSpread => {
                    let iterable = frame.pop();
                    let items = delegate!(self.iterate(&iterable));
                    if let Some(Value::Object(o)) = frame.stack.last().cloned() {
                        if let ObjKind::Array(vec) = &mut o.borrow_mut().kind {
                            vec.extend(items);
                        }
                    }
                }
                Op::MakeObject(n) => {
                    let flat = frame.pop_n(n * 2);
                    let o = self.new_object(Some(self.object_proto.clone()));
                    let mut it = flat.into_iter();
                    while let (Some(k), Some(v)) = (it.next(), it.next()) {
                        let key = delegate!(self.property_key(&k));
                        o.borrow_mut().set_data(&key, v);
                    }
                    frame.stack.push(Value::Object(o));
                }
                Op::Jump(t) => frame.ip = t as usize,
                Op::JumpIfFalse(t) => {
                    if !to_boolean(&frame.pop()) {
                        frame.ip = t as usize;
                    }
                }
                Op::JumpIfTrue(t) => {
                    if to_boolean(&frame.pop()) {
                        frame.ip = t as usize;
                    }
                }
                Op::JumpIfFalsyKeep(t) => {
                    if !to_boolean(&frame.peek()) {
                        frame.ip = t as usize;
                    }
                }
                Op::JumpIfTruthyKeep(t) => {
                    if to_boolean(&frame.peek()) {
                        frame.ip = t as usize;
                    }
                }
                Op::JumpIfNullishKeep(t) => {
                    if !matches!(frame.peek(), Value::Undefined | Value::Null) {
                        frame.ip = t as usize;
                    }
                }
                Op::GetIterator => {
                    let iterable = frame.pop();
                    let iter = delegate!(self.vm_get_iterator(&iterable));
                    frame.stack.push(iter);
                }
                Op::GetEnumIterator => {
                    let obj = frame.pop();
                    let keys: Vec<Value> = self.enum_keys(&obj).into_iter().map(Value::str).collect();
                    let iter = self.make_eager_generator(keys);
                    frame.stack.push(iter);
                }
                Op::IterNext(done_t) => {
                    let iterator = frame.peek();
                    let next_fn = delegate!(self.get_member(&iterator, "next"));
                    let result = delegate!(self.call(&next_fn, iterator, &[]));
                    let done = delegate!(self.get_member(&result, "done"));
                    if to_boolean(&done) {
                        frame.ip = done_t as usize;
                    } else {
                        let value = delegate!(self.get_member(&result, "value"));
                        frame.stack.push(value);
                    }
                }
                Op::PushHandler(catch_ip) => frame.push_handler(catch_ip),
                Op::PopHandler => frame.pop_handler(),
                Op::PushFinally(finally_ip) => frame.push_finally(finally_ip),
                Op::PopFinally => frame.pop_handler(),
                Op::EndFinally => {
                    // A throw pending from the protected block is re-raised
                    // after the finally block ran.
                    if let Some(v) = frame.take_pending_throw() {
                        match frame.take_handler(v) {
                            Ok(()) => continue,
                            Err(t) => return Step::Throw(t),
                        }
                    }
                }
                Op::Yield => return Step::Yield(frame.pop()),
                Op::Await => return Step::Await(frame.pop()),
                Op::Return => return Step::Done(frame.pop()),
                Op::ReturnUndefined => return Step::Done(Value::Undefined),
                Op::Throw => {
                    let v = frame.pop();
                    match frame.take_handler(v) {
                        Ok(()) => continue,
                        Err(t) => return Step::Throw(t),
                    }
                }
            }
        }
    }

    fn eval_new(&mut self, callee: &Expr, args: &[Expr], env: &Env) -> Eval<Value> {
        let func = self.eval_expr(callee, env)?;
        let argv = self.eval_args(args, env)?;
        self.construct(&func, &argv)
    }

    /// Resolve `super.key` — look `key` up on the home object's prototype, with
    /// `this` as the receiver.
    fn super_get(&mut self, key: &str, env: &Env) -> Eval<Value> {
        let home = scope_home(env).unwrap_or(Value::Undefined);
        let this = scope_this(env);
        let proto = match &home {
            Value::Object(o) => o.borrow().proto.clone(),
            _ => None,
        };
        match proto {
            Some(p) => self.lookup_chain(&p, &this, key),
            None => Ok(Value::Undefined),
        }
    }

    /// Evaluate `super(...)` — call the parent constructor (the current
    /// function's `[[Prototype]]`) with the current `this`.
    fn eval_super_call(&mut self, args: &[Expr], env: &Env) -> Eval<Value> {
        let cur = scope_current_fn(env).unwrap_or(Value::Undefined);
        let parent = match &cur {
            Value::Object(o) => o.borrow().proto.clone(),
            _ => None,
        };
        let this = scope_this(env);
        let argv = self.eval_args(args, env)?;
        if let Some(p) = parent {
            let pv = Value::Object(p);
            if pv.is_callable() {
                self.call(&pv, this, &argv)?;
            }
        }
        Ok(Value::Undefined)
    }

    /// `new func(args)`.
    pub fn construct(&mut self, func: &Value, args: &[Value]) -> Eval<Value> {
        let fobj = match func.as_object() {
            Some(o) if matches!(o.borrow().kind, ObjKind::Function(_)) => o.clone(),
            _ => return self.throw_type("not a constructor"),
        };
        let proto = match self.get_member(func, "prototype")? {
            Value::Object(p) => p,
            _ => self.object_proto.clone(),
        };
        let obj = self.new_object(Some(proto));
        let this = Value::Object(obj);
        let _ = fobj;
        let result = self.call(func, this.clone(), args)?;
        Ok(match result {
            Value::Object(_) => result,
            _ => this,
        })
    }

    fn bind_params(&mut self, params: &[Pattern], args: &[Value], env: &Env) -> Eval<()> {
        for (i, p) in params.iter().enumerate() {
            if let Pattern::Rest(inner) = p {
                let rest: Vec<Value> = args.iter().skip(i).cloned().collect();
                let arr = self.new_array(rest);
                self.declare_pattern(inner, arr, env, DeclKind::Param)?;
                return Ok(());
            }
            let v = args.get(i).cloned().unwrap_or(Value::Undefined);
            self.declare_pattern(p, v, env, DeclKind::Param)?;
        }
        Ok(())
    }

    // ---- classes ----------------------------------------------------------

    fn eval_class(&mut self, class: &Class, env: &Env) -> Eval<Value> {
        let super_proto;
        let super_ctor;
        if let Some(sc) = &class.super_class {
            let sval = self.eval_expr(sc, env)?;
            super_ctor = Some(sval.clone());
            super_proto = match self.get_member(&sval, "prototype")? {
                Value::Object(p) => Some(p),
                _ => Some(self.object_proto.clone()),
            };
        } else {
            super_ctor = None;
            super_proto = Some(self.object_proto.clone());
        }

        let proto = self.new_object(super_proto);

        // Find the constructor, or synthesise a default one.
        let ctor_func = class
            .members
            .iter()
            .find(|m| m.kind == ClassMemberKind::Constructor)
            .and_then(|m| match &m.value {
                Some(ClassMemberValue::Func(f)) => Some(f.clone()),
                _ => None,
            });

        let ctor_def = match ctor_func {
            Some(mut f) => {
                f.name = class.name.clone();
                Rc::new(f)
            }
            None => Rc::new(default_constructor(class.name.clone(), super_ctor.is_some())),
        };
        let ctor_val = self.make_user_function(ctor_def, env.clone(), false, None, None);
        let ctor_obj = ctor_val.as_object().unwrap().clone();

        // Wire prototype <-> constructor.
        proto.borrow_mut().set_data("constructor", ctor_val.clone());
        ctor_obj
            .borrow_mut()
            .set_data("prototype", Value::Object(proto.clone()));
        // Static inheritance: constructor.__proto__ = SuperConstructor.
        if let Some(Value::Object(sco)) = &super_ctor {
            ctor_obj.borrow_mut().proto = Some(sco.clone());
        }

        // Install methods / accessors / static members / fields.
        for m in &class.members {
            if m.kind == ClassMemberKind::Constructor {
                continue;
            }
            let key = self.prop_key_str(&m.key, env)?;
            let target = if m.is_static { &ctor_obj } else { &proto };
            match m.kind {
                ClassMemberKind::Method => {
                    if let Some(ClassMemberValue::Func(f)) = &m.value {
                        let home = Some(Value::Object(target.clone()));
                        let fv = self
                            .make_user_function(Rc::new(f.clone()), env.clone(), false, None, home);
                        target.borrow_mut().set_data(&key, fv);
                    }
                }
                ClassMemberKind::Get | ClassMemberKind::Set => {
                    if let Some(ClassMemberValue::Func(f)) = &m.value {
                        let home = Some(Value::Object(target.clone()));
                        let fv = self
                            .make_user_function(Rc::new(f.clone()), env.clone(), false, None, home);
                        let fgc = fv.as_object().cloned();
                        let mut b = target.borrow_mut();
                        let (mut get, mut set) = match b.get_own(&key).cloned() {
                            Some(PropDesc::Accessor { get, set }) => (get, set),
                            _ => (None, None),
                        };
                        if m.kind == ClassMemberKind::Get {
                            get = fgc;
                        } else {
                            set = fgc;
                        }
                        b.set_own(&key, PropDesc::Accessor { get, set });
                    }
                }
                ClassMemberKind::Field if m.is_static => {
                    let v = match &m.value {
                        Some(ClassMemberValue::Expr(e)) => self.eval_expr(e, env)?,
                        _ => Value::Undefined,
                    };
                    ctor_obj.borrow_mut().set_data(&key, v);
                }
                _ => {}
            }
        }
        Ok(ctor_val)
    }

    // ---- references (l-values) -------------------------------------------

    fn resolve_ref(&mut self, expr: &Expr, env: &Env) -> Eval<Reference> {
        match expr {
            Expr::Ident(name) => Ok(Reference::Var(name.clone(), env.clone())),
            Expr::Member {
                object, property, ..
            } => {
                let base = self.eval_expr(object, env)?;
                let key = self.member_key(property, env)?;
                Ok(Reference::Prop(base, key))
            }
            _ => self.throw_type("invalid assignment target"),
        }
    }

    fn get_ref(&mut self, r: &Reference) -> Eval<Value> {
        match r {
            Reference::Var(name, env) => self.lookup_ident(name, env),
            Reference::Prop(base, key) => self.get_member(base, key),
        }
    }

    fn set_ref(&mut self, r: &Reference, value: Value) -> Eval<()> {
        match r {
            Reference::Var(name, env) => self.assign_name(name, value, env),
            Reference::Prop(base, key) => self.set_member(base, key, value),
        }
    }

    fn assign_name(&mut self, name: &str, value: Value, env: &Env) -> Eval<()> {
        match scope_set(env, name, value.clone()) {
            SetOutcome::Set => Ok(()),
            SetOutcome::NotFound => {
                // Implicit global (sloppy mode).
                self.global.borrow_mut().set_data(name, value);
                Ok(())
            }
            SetOutcome::Const => {
                self.throw_type(format!("Assignment to constant variable '{name}'"))
            }
        }
    }

    // ---- property access --------------------------------------------------

    /// Get `base[key]`, following the prototype chain and honouring accessors.
    pub fn get_member(&mut self, base: &Value, key: &str) -> Eval<Value> {
        match base {
            Value::Undefined | Value::Null => self.throw_type(format!(
                "Cannot read properties of {} (reading '{key}')",
                if matches!(base, Value::Null) {
                    "null"
                } else {
                    "undefined"
                }
            )),
            Value::Str(s) => self.get_string_member(s, key, base),
            Value::Num(_) => {
                let proto = self.number_proto.clone();
                self.lookup_chain(&proto, base, key)
            }
            Value::Bool(_) => {
                let proto = self.boolean_proto.clone();
                self.lookup_chain(&proto, base, key)
            }
            Value::Object(o) => {
                if let ObjKind::Array(elems) = &o.borrow().kind {
                    if key == "length" {
                        return Ok(Value::Num(elems.len() as f64));
                    }
                    if let Ok(i) = key.parse::<usize>() {
                        return Ok(elems.get(i).cloned().unwrap_or(Value::Undefined));
                    }
                }
                let start = o.clone();
                self.lookup_chain(&start, base, key)
            }
        }
    }

    fn get_string_member(&mut self, s: &Rc<str>, key: &str, base: &Value) -> Eval<Value> {
        if key == "length" {
            return Ok(Value::Num(s.chars().count() as f64));
        }
        if let Ok(i) = key.parse::<usize>() {
            return Ok(match s.chars().nth(i) {
                Some(c) => Value::str(c.to_string()),
                None => Value::Undefined,
            });
        }
        let proto = self.string_proto.clone();
        self.lookup_chain(&proto, base, key)
    }

    /// Walk the prototype chain starting at `start`, returning the value for
    /// `key` (invoking a getter with `receiver` as `this`).
    fn lookup_chain(&mut self, start: &Gc, receiver: &Value, key: &str) -> Eval<Value> {
        let mut cur = Some(start.clone());
        while let Some(c) = cur {
            let found = c.borrow().get_own(key).cloned();
            if let Some(prop) = found {
                return match prop {
                    PropDesc::Data(v) => Ok(v),
                    PropDesc::Accessor { get, .. } => match get {
                        Some(g) => self.call(&Value::Object(g), receiver.clone(), &[]),
                        None => Ok(Value::Undefined),
                    },
                };
            }
            cur = c.borrow().proto.clone();
        }
        Ok(Value::Undefined)
    }

    /// Set `base[key] = value`.
    pub fn set_member(&mut self, base: &Value, key: &str, value: Value) -> Eval<()> {
        let o = match base {
            Value::Object(o) => o.clone(),
            Value::Undefined | Value::Null => {
                return self.throw_type(format!("Cannot set properties of {}", type_of(base)))
            }
            _ => return Ok(()), // primitives: silently ignore (sloppy)
        };

        // Array index / length fast paths.
        {
            let mut b = o.borrow_mut();
            if let ObjKind::Array(elems) = &mut b.kind {
                if key == "length" {
                    let n = to_number_pure(&value) as usize;
                    elems.resize(n, Value::Undefined);
                    return Ok(());
                }
                if let Ok(i) = key.parse::<usize>() {
                    if i >= elems.len() {
                        elems.resize(i + 1, Value::Undefined);
                    }
                    elems[i] = value;
                    return Ok(());
                }
            }
        }

        // Own accessor setter?
        let own = o.borrow().get_own(key).cloned();
        if let Some(PropDesc::Accessor { set, .. }) = own {
            if let Some(setter) = set {
                self.call(&Value::Object(setter), base.clone(), &[value])?;
            }
            return Ok(());
        }
        // Inherited accessor setter?
        if o.borrow().get_own(key).is_none() {
            let mut cur = o.borrow().proto.clone();
            while let Some(c) = cur {
                let p = c.borrow().get_own(key).cloned();
                if let Some(PropDesc::Accessor { set, .. }) = p {
                    if let Some(setter) = set {
                        self.call(&Value::Object(setter), base.clone(), &[value])?;
                    }
                    return Ok(());
                }
                if c.borrow().get_own(key).is_some() {
                    break; // shadowed by an inherited data prop; define own below
                }
                cur = c.borrow().proto.clone();
            }
        }

        let extensible = o.borrow().extensible;
        if o.borrow().get_own(key).is_some() || extensible {
            o.borrow_mut().set_data(key, value);
        }
        Ok(())
    }

    fn has_property(&self, base: &Value, key: &str) -> bool {
        match base {
            Value::Object(o) => {
                if let ObjKind::Array(elems) = &o.borrow().kind {
                    if key == "length" {
                        return true;
                    }
                    if let Ok(i) = key.parse::<usize>() {
                        return i < elems.len();
                    }
                }
                let mut cur = Some(o.clone());
                while let Some(c) = cur {
                    if c.borrow().get_own(key).is_some() {
                        return true;
                    }
                    cur = c.borrow().proto.clone();
                }
                false
            }
            _ => false,
        }
    }

    // ---- abstract operations needing the interpreter ----------------------

    /// `ToPrimitive` (ECMA-262 §7.1.1).
    pub fn to_primitive(&mut self, v: &Value, prefer_string: bool) -> Eval<Value> {
        let obj = match v {
            Value::Object(_) => v.clone(),
            other => return Ok(other.clone()),
        };
        let order: [&str; 2] = if prefer_string {
            ["toString", "valueOf"]
        } else {
            ["valueOf", "toString"]
        };
        for name in order {
            let method = self.get_member(&obj, name)?;
            if method.is_callable() {
                let r = self.call(&method, obj.clone(), &[])?;
                if !matches!(r, Value::Object(_)) {
                    return Ok(r);
                }
            }
        }
        self.throw_type("cannot convert object to primitive value")
    }

    /// `ToNumber` (ECMA-262 §7.1.4).
    pub fn to_number(&mut self, v: &Value) -> Eval<f64> {
        Ok(match v {
            Value::Undefined => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Num(n) => *n,
            Value::Str(s) => str_to_num(s),
            Value::Object(_) => {
                let p = self.to_primitive(v, false)?;
                self.to_number(&p)?
            }
        })
    }

    /// `ToString` (ECMA-262 §7.1.17).
    pub fn to_string_v(&mut self, v: &Value) -> Eval<String> {
        Ok(match v {
            Value::Undefined => "undefined".to_string(),
            Value::Null => "null".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Num(n) => num_to_str(*n),
            Value::Str(s) => s.to_string(),
            Value::Object(_) => {
                let p = self.to_primitive(v, true)?;
                self.to_string_v(&p)?
            }
        })
    }

    fn property_key(&mut self, v: &Value) -> Eval<String> {
        // A Symbol indexes a property by its unique internal key.
        if let Value::Object(o) = v {
            if o.borrow().class == "Symbol" {
                if let Some(PropDesc::Data(Value::Str(k))) = o.borrow().get_own("__key") {
                    return Ok(k.to_string());
                }
            }
        }
        self.to_string_v(v)
    }

    fn apply_binop(&mut self, op: BinOp, l: Value, r: Value) -> Eval<Value> {
        Ok(match op {
            BinOp::Add => {
                let lp = self.to_primitive(&l, false)?;
                let rp = self.to_primitive(&r, false)?;
                if matches!(lp, Value::Str(_)) || matches!(rp, Value::Str(_)) {
                    let mut s = self.to_string_v(&lp)?;
                    s.push_str(&self.to_string_v(&rp)?);
                    Value::str(s)
                } else {
                    Value::Num(self.to_number(&lp)? + self.to_number(&rp)?)
                }
            }
            BinOp::Sub => Value::Num(self.to_number(&l)? - self.to_number(&r)?),
            BinOp::Mul => Value::Num(self.to_number(&l)? * self.to_number(&r)?),
            BinOp::Div => Value::Num(self.to_number(&l)? / self.to_number(&r)?),
            BinOp::Mod => Value::Num(js_mod(self.to_number(&l)?, self.to_number(&r)?)),
            BinOp::Exp => Value::Num(self.to_number(&l)?.powf(self.to_number(&r)?)),
            BinOp::BitAnd => Value::Num((to_int32(self.to_number(&l)?) & to_int32(self.to_number(&r)?)) as f64),
            BinOp::BitOr => Value::Num((to_int32(self.to_number(&l)?) | to_int32(self.to_number(&r)?)) as f64),
            BinOp::BitXor => Value::Num((to_int32(self.to_number(&l)?) ^ to_int32(self.to_number(&r)?)) as f64),
            BinOp::Shl => {
                let a = to_int32(self.to_number(&l)?);
                let b = to_uint32(self.to_number(&r)?) & 31;
                Value::Num((a.wrapping_shl(b)) as f64)
            }
            BinOp::Shr => {
                let a = to_int32(self.to_number(&l)?);
                let b = to_uint32(self.to_number(&r)?) & 31;
                Value::Num((a >> b) as f64)
            }
            BinOp::UShr => {
                let a = to_uint32(self.to_number(&l)?);
                let b = to_uint32(self.to_number(&r)?) & 31;
                Value::Num((a >> b) as f64)
            }
            BinOp::Eq => Value::Bool(self.abstract_eq(&l, &r)?),
            BinOp::Neq => Value::Bool(!self.abstract_eq(&l, &r)?),
            BinOp::StrictEq => Value::Bool(strict_eq(&l, &r)),
            BinOp::StrictNeq => Value::Bool(!strict_eq(&l, &r)),
            BinOp::Lt => self.compare(&l, &r, Cmp::Lt)?,
            BinOp::Gt => self.compare(&l, &r, Cmp::Gt)?,
            BinOp::Le => self.compare(&l, &r, Cmp::Le)?,
            BinOp::Ge => self.compare(&l, &r, Cmp::Ge)?,
            BinOp::In => {
                let key = self.property_key(&l)?;
                Value::Bool(self.has_property(&r, &key))
            }
            BinOp::Instanceof => Value::Bool(self.instance_of(&l, &r)?),
        })
    }

    fn abstract_eq(&mut self, a: &Value, b: &Value) -> Eval<bool> {
        Ok(match (a, b) {
            (Value::Null, Value::Undefined) | (Value::Undefined, Value::Null) => true,
            _ if std::mem::discriminant(a) == std::mem::discriminant(b) => strict_eq(a, b),
            (Value::Num(_), Value::Str(_)) => {
                let bn = self.to_number(b)?;
                self.to_number(a)? == bn
            }
            (Value::Str(_), Value::Num(_)) => {
                let an = self.to_number(a)?;
                an == self.to_number(b)?
            }
            (Value::Bool(_), _) => {
                let an = Value::Num(self.to_number(a)?);
                self.abstract_eq(&an, b)?
            }
            (_, Value::Bool(_)) => {
                let bn = Value::Num(self.to_number(b)?);
                self.abstract_eq(a, &bn)?
            }
            (Value::Object(_), Value::Num(_) | Value::Str(_)) => {
                let ap = self.to_primitive(a, false)?;
                self.abstract_eq(&ap, b)?
            }
            (Value::Num(_) | Value::Str(_), Value::Object(_)) => {
                let bp = self.to_primitive(b, false)?;
                self.abstract_eq(a, &bp)?
            }
            _ => false,
        })
    }

    fn compare(&mut self, a: &Value, b: &Value, cmp: Cmp) -> Eval<Value> {
        let pa = self.to_primitive(a, false)?;
        let pb = self.to_primitive(b, false)?;
        if let (Value::Str(x), Value::Str(y)) = (&pa, &pb) {
            let r = match cmp {
                Cmp::Lt => x < y,
                Cmp::Gt => x > y,
                Cmp::Le => x <= y,
                Cmp::Ge => x >= y,
            };
            return Ok(Value::Bool(r));
        }
        let x = self.to_number(&pa)?;
        let y = self.to_number(&pb)?;
        if x.is_nan() || y.is_nan() {
            return Ok(Value::Bool(false));
        }
        Ok(Value::Bool(match cmp {
            Cmp::Lt => x < y,
            Cmp::Gt => x > y,
            Cmp::Le => x <= y,
            Cmp::Ge => x >= y,
        }))
    }

    fn instance_of(&mut self, val: &Value, ctor: &Value) -> Eval<bool> {
        let proto = match self.get_member(ctor, "prototype")? {
            Value::Object(p) => p,
            _ => return Ok(false),
        };
        let mut cur = match val {
            Value::Object(o) => o.borrow().proto.clone(),
            _ => return Ok(false),
        };
        while let Some(c) = cur {
            if Rc::ptr_eq(&c, &proto) {
                return Ok(true);
            }
            cur = c.borrow().proto.clone();
        }
        Ok(false)
    }

    // ---- iteration & enumeration -----------------------------------------

    /// Collect the elements produced by iterating `value` (arrays and strings).
    pub fn iterate(&mut self, value: &Value) -> Eval<Vec<Value>> {
        match value {
            Value::Object(o) => {
                if let ObjKind::Array(elems) = &o.borrow().kind {
                    return Ok(elems.clone());
                }
                let class = o.borrow().class;
                if class == "Set" {
                    return Ok(read_slot_array(o, "__vals"));
                }
                if class == "Generator" {
                    // Lazy (VM-backed) generator: drive `next()` to exhaustion.
                    if o.borrow().get_own("__genid").is_some() {
                        let next_fn = self.get_member(value, "next")?;
                        let mut out = Vec::new();
                        let mut guard = 0u64;
                        loop {
                            let r = self.call(&next_fn, value.clone(), &[])?;
                            if to_boolean(&self.get_member(&r, "done")?) {
                                break;
                            }
                            out.push(self.get_member(&r, "value")?);
                            guard += 1;
                            if guard > 10_000_000 {
                                return self.throw_range("iterator produced too many values");
                            }
                        }
                        return Ok(out);
                    }
                    // Eager generator: hand back the not-yet-consumed buffer.
                    let items = read_slot_array(o, "__items");
                    let idx = match o.borrow().get_own("__index") {
                        Some(PropDesc::Data(Value::Num(n))) => *n as usize,
                        _ => 0,
                    };
                    o.borrow_mut().set_data("__index", Value::Num(items.len() as f64));
                    return Ok(items[idx.min(items.len())..].to_vec());
                }
                if class == "Map" {
                    let keys = read_slot_array(o, "__keys");
                    let vals = read_slot_array(o, "__vals");
                    return Ok(keys
                        .into_iter()
                        .zip(vals)
                        .map(|(k, v)| self.new_array(vec![k, v]))
                        .collect());
                }
                // Custom iterable via the iterator protocol (`Symbol.iterator`).
                let iter_fn = self.get_member(value, "@@iterator")?;
                if iter_fn.is_callable() {
                    let iterator = self.call(&iter_fn, value.clone(), &[])?;
                    let next_fn = self.get_member(&iterator, "next")?;
                    let mut out = Vec::new();
                    let mut guard = 0u64;
                    loop {
                        let result = self.call(&next_fn, iterator.clone(), &[])?;
                        if to_boolean(&self.get_member(&result, "done")?) {
                            break;
                        }
                        out.push(self.get_member(&result, "value")?);
                        guard += 1;
                        if guard > 10_000_000 {
                            return self.throw_range("iterator produced too many values");
                        }
                    }
                    return Ok(out);
                }
                // Array-like (has length + indices)?
                let len = self.get_member(value, "length")?;
                if let Value::Num(n) = len {
                    let mut out = Vec::new();
                    for i in 0..(n.max(0.0) as usize) {
                        out.push(self.get_member(value, &i.to_string())?);
                    }
                    return Ok(out);
                }
                self.throw_type("value is not iterable")
            }
            Value::Str(s) => Ok(s.chars().map(|c| Value::str(c.to_string())).collect()),
            _ => self.throw_type("value is not iterable"),
        }
    }

    /// Enumerable own string keys for `for…in` and `Object.keys`.
    pub fn enum_keys(&self, value: &Value) -> Vec<String> {
        match value {
            Value::Object(o) => {
                let b = o.borrow();
                let mut keys = Vec::new();
                if let ObjKind::Array(elems) = &b.kind {
                    for i in 0..elems.len() {
                        keys.push(i.to_string());
                    }
                }
                for (k, _) in &b.props {
                    keys.push(k.clone());
                }
                keys
            }
            _ => Vec::new(),
        }
    }

    // ---- pattern binding & destructuring assignment ----------------------

    fn declare_pattern(
        &mut self,
        pat: &Pattern,
        value: Value,
        env: &Env,
        kind: DeclKind,
    ) -> Eval<()> {
        match pat {
            Pattern::Ident(name) => {
                match kind {
                    DeclKind::Var => scope_var_set(env, name, value),
                    DeclKind::Let | DeclKind::Param => scope_declare(env, name, value, true),
                    DeclKind::Const => scope_declare(env, name, value, false),
                }
                Ok(())
            }
            Pattern::Default { target, default } => {
                let v = if matches!(value, Value::Undefined) {
                    self.eval_expr(default, env)?
                } else {
                    value
                };
                self.declare_pattern(target, v, env, kind)
            }
            Pattern::Array(elems) => {
                let items = self.iterate(&value)?;
                for (i, el) in elems.iter().enumerate() {
                    match el {
                        None => {}
                        Some(Pattern::Rest(inner)) => {
                            let rest: Vec<Value> = items.iter().skip(i).cloned().collect();
                            let arr = self.new_array(rest);
                            self.declare_pattern(inner, arr, env, kind)?;
                            break;
                        }
                        Some(p) => {
                            let v = items.get(i).cloned().unwrap_or(Value::Undefined);
                            self.declare_pattern(p, v, env, kind)?;
                        }
                    }
                }
                Ok(())
            }
            Pattern::Object { props, rest } => {
                let mut taken = Vec::new();
                for p in props {
                    let key = self.prop_key_str(&p.key, env)?;
                    taken.push(key.clone());
                    let v = self.get_member(&value, &key)?;
                    self.declare_pattern(&p.value, v, env, kind)?;
                }
                if let Some(rest_pat) = rest {
                    let robj = self.new_object(Some(self.object_proto.clone()));
                    for k in self.enum_keys(&value) {
                        if !taken.contains(&k) {
                            let v = self.get_member(&value, &k)?;
                            robj.borrow_mut().set_data(&k, v);
                        }
                    }
                    self.declare_pattern(rest_pat, Value::Object(robj), env, kind)?;
                }
                Ok(())
            }
            Pattern::Rest(inner) => self.declare_pattern(inner, value, env, kind),
            Pattern::Member(_) => self.throw_type("invalid binding target"),
        }
    }

    fn assign_destructure(&mut self, target: &Expr, value: Value, env: &Env) -> Eval<()> {
        match target {
            Expr::Array(elems) => {
                let items = self.iterate(&value)?;
                for (i, el) in elems.iter().enumerate() {
                    match el {
                        None => {}
                        Some(Expr::Spread(inner)) => {
                            let rest: Vec<Value> = items.iter().skip(i).cloned().collect();
                            let arr = self.new_array(rest);
                            self.assign_target(inner, arr, env)?;
                            break;
                        }
                        Some(e) => {
                            let v = items.get(i).cloned().unwrap_or(Value::Undefined);
                            self.assign_target(e, v, env)?;
                        }
                    }
                }
                Ok(())
            }
            Expr::Object(props) => {
                let mut taken = Vec::new();
                for p in props {
                    if p.kind == PropKind::Spread {
                        if let PropValue::Spread(e) = &p.value {
                            let robj = self.new_object(Some(self.object_proto.clone()));
                            for k in self.enum_keys(&value) {
                                if !taken.contains(&k) {
                                    let v = self.get_member(&value, &k)?;
                                    robj.borrow_mut().set_data(&k, v);
                                }
                            }
                            self.assign_target(e, Value::Object(robj), env)?;
                        }
                        continue;
                    }
                    let key = self.prop_key_str(&p.key, env)?;
                    taken.push(key.clone());
                    let v = self.get_member(&value, &key)?;
                    if let PropValue::Expr(e) = &p.value {
                        self.assign_target(e, v, env)?;
                    }
                }
                Ok(())
            }
            _ => self.assign_target(target, value, env),
        }
    }

    fn assign_target(&mut self, target: &Expr, value: Value, env: &Env) -> Eval<()> {
        match target {
            Expr::Ident(name) => self.assign_name(name, value, env),
            Expr::Member { .. } => {
                let r = self.resolve_ref(target, env)?;
                self.set_ref(&r, value)
            }
            Expr::Array(_) | Expr::Object(_) => self.assign_destructure(target, value, env),
            Expr::Assign {
                op: AssignOp::Assign,
                target: inner,
                value: default,
            } => {
                let v = if matches!(value, Value::Undefined) {
                    self.eval_expr(default, env)?
                } else {
                    value
                };
                self.assign_target(inner, v, env)
            }
            _ => self.throw_type("invalid assignment target"),
        }
    }
}

impl Default for Interp {
    fn default() -> Self {
        Self::new()
    }
}

// ---- supporting types & free functions -------------------------------------

/// A resolved l-value reference.
#[derive(Debug)]
enum Reference {
    /// A variable in a scope chain.
    Var(String, Env),
    /// A property of a base value.
    Prop(Value, String),
}

#[derive(Debug, Clone, Copy)]
enum Cmp {
    Lt,
    Gt,
    Le,
    Ge,
}

fn loop_targets(label: &Option<String>, current: Option<&str>) -> bool {
    match label {
        None => true,
        Some(l) => current == Some(l.as_str()),
    }
}

fn compound_binop(op: AssignOp) -> BinOp {
    match op {
        AssignOp::Add => BinOp::Add,
        AssignOp::Sub => BinOp::Sub,
        AssignOp::Mul => BinOp::Mul,
        AssignOp::Div => BinOp::Div,
        AssignOp::Mod => BinOp::Mod,
        AssignOp::Exp => BinOp::Exp,
        AssignOp::Shl => BinOp::Shl,
        AssignOp::Shr => BinOp::Shr,
        AssignOp::UShr => BinOp::UShr,
        AssignOp::BitAnd => BinOp::BitAnd,
        AssignOp::BitOr => BinOp::BitOr,
        AssignOp::BitXor => BinOp::BitXor,
        // Logical compound ops are handled before this is reached.
        AssignOp::Assign | AssignOp::And | AssignOp::Or | AssignOp::Nullish => BinOp::Add,
    }
}

/// The object handle if `v` is a Promise.
fn as_promise(v: &Value) -> Option<Gc> {
    match v {
        Value::Object(o) if o.borrow().class == "Promise" => Some(o.clone()),
        _ => None,
    }
}

fn promise_state(p: &Gc) -> String {
    match p.borrow().get_own("__state") {
        Some(PropDesc::Data(Value::Str(s))) => s.to_string(),
        _ => "pending".to_string(),
    }
}

fn promise_value(p: &Gc) -> Value {
    match p.borrow().get_own("__value") {
        Some(PropDesc::Data(v)) => v.clone(),
        _ => Value::Undefined,
    }
}

fn read_cbs(p: &Gc) -> Value {
    match p.borrow().get_own("__cbs") {
        Some(PropDesc::Data(v)) => v.clone(),
        _ => Value::Undefined,
    }
}

/// Read an own data slot of an object value (`Undefined` if absent).
fn obj_slot(v: &Value, key: &str) -> Value {
    if let Value::Object(o) = v {
        if let Some(PropDesc::Data(d)) = o.borrow().get_own(key) {
            return d.clone();
        }
    }
    Value::Undefined
}

/// Read a slot holding an array object (used to iterate `Map`/`Set`).
fn read_slot_array(o: &Gc, slot: &str) -> Vec<Value> {
    if let Some(PropDesc::Data(Value::Object(g))) = o.borrow().get_own(slot) {
        if let ObjKind::Array(e) = &g.borrow().kind {
            return e.clone();
        }
    }
    Vec::new()
}

/// The elements of an array value (empty for non-arrays); used to apply the
/// arguments array built for a spread call.
fn array_elems(v: &Value) -> Vec<Value> {
    if let Value::Object(o) = v {
        if let ObjKind::Array(e) = &o.borrow().kind {
            return e.clone();
        }
    }
    Vec::new()
}

/// JavaScript `%` is a remainder with the sign of the dividend.
fn js_mod(a: f64, b: f64) -> f64 {
    if b == 0.0 || a.is_nan() || b.is_nan() || a.is_infinite() {
        return f64::NAN;
    }
    if b.is_infinite() {
        return a;
    }
    a - b * (a / b).trunc()
}

/// `ToNumber` for an already-primitive value (used where calling user code is
/// not possible, e.g. setting `array.length`).
fn to_number_pure(v: &Value) -> f64 {
    match v {
        Value::Num(n) => *n,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Str(s) => str_to_num(s),
        Value::Null => 0.0,
        _ => f64::NAN,
    }
}

/// The names bound by a pattern (for `var` hoisting).
fn pattern_names(pat: &Pattern) -> Vec<String> {
    let mut out = Vec::new();
    collect_pattern_names(pat, &mut out);
    out
}

fn collect_pattern_names(pat: &Pattern, out: &mut Vec<String>) {
    match pat {
        Pattern::Ident(name) => out.push(name.clone()),
        Pattern::Default { target, .. } | Pattern::Rest(target) => {
            collect_pattern_names(target, out)
        }
        Pattern::Array(elems) => {
            for el in elems.iter().flatten() {
                collect_pattern_names(el, out);
            }
        }
        Pattern::Object { props, rest } => {
            for p in props {
                collect_pattern_names(&p.value, out);
            }
            if let Some(r) = rest {
                collect_pattern_names(r, out);
            }
        }
        Pattern::Member(_) => {}
    }
}

/// Synthesize a default class constructor (`constructor(...a){ super(...a) }`).
fn default_constructor(name: Option<String>, has_super: bool) -> Func {
    // A derived class's implicit constructor is `constructor(...a){ super(...a) }`.
    let body = if has_super {
        vec![Stmt::Expr(Expr::Call {
            callee: Box::new(Expr::Super),
            args: vec![Expr::Spread(Box::new(Expr::Ident("arguments".into())))],
            optional: false,
        })]
    } else {
        Vec::new()
    };
    Func {
        name,
        params: Vec::new(),
        body: FuncBody::Block(body),
        is_arrow: false,
        is_async: false,
        is_generator: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(src: &str) -> Value {
        let mut interp = Interp::new();
        let program = super::super::parser::parse(src).expect("parse");
        interp.run(&program).unwrap_or_else(|e| panic!("eval error: {e:?}"))
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
    fn lazy_infinite_generator() {
        // An infinite `while (true)` generator: laziness is proven by pulling
        // only the first five values without hanging.
        assert_eq!(
            num(
                "function* nats(){ let i = 0; while (true) { yield i; i = i + 1; } }
                 const g = nats();
                 let sum = 0;
                 for (let k = 0; k < 5; k++) { sum = sum + g.next().value; }
                 sum"
            ),
            10.0 // 0+1+2+3+4
        );
    }

    #[test]
    fn bidirectional_generator_next() {
        // `next(v)` feeds `v` back as the value of the suspended `yield`.
        assert_eq!(
            num(
                "function* echo(){ const a = yield 1; const b = yield a + 10; return b; }
                 const g = echo();
                 const r1 = g.next();    // {value:1}
                 const r2 = g.next(5);   // a=5 → yield 15
                 const r3 = g.next(100); // b=100 → return 100, done
                 r1.value * 1000 + r2.value * 10 + r3.value"
            ),
            1250.0
        );
    }

    #[test]
    fn yield_star_delegation_and_for_of() {
        // `yield*` delegates lazily; `for…of` drives a VM generator step-wise.
        assert_eq!(
            num(
                "function* inner(){ yield 1; yield 2; }
                 function* outer(){ yield* inner(); yield 3; }
                 let s = 0;
                 for (const x of outer()) { s = s + x; }
                 s"
            ),
            6.0
        );
    }

    #[test]
    fn spread_collects_a_generator() {
        assert_eq!(
            num(
                "function* g(){ yield 10; yield 20; yield 30; }
                 const a = [...g()];
                 a[0] + a[1] + a[2]"
            ),
            60.0
        );
    }

    #[test]
    fn for_of_inside_a_generator_stays_lazy() {
        // The loop variable iterates an array via GetIterator/IterNext inside
        // the VM, re-yielding transformed values.
        assert_eq!(
            num(
                "function* doubler(xs){ for (const x of xs) { yield x * 2; } }
                 let s = 0;
                 for (const y of doubler([1, 2, 3])) { s = s + y; }
                 s"
            ),
            12.0
        );
    }

    #[test]
    fn generator_try_catch_across_yield() {
        // A `try`/`catch` spanning a `yield` is compiled into the VM: the
        // handler survives suspension and catches a later throw.
        assert_eq!(
            string(
                "function* g(){
                   try { yield 1; throw 'e'; yield 999; }
                   catch (err) { yield 'caught:' + err; }
                   yield 2;
                 }
                 const it = g();
                 const a = it.next().value;
                 const b = it.next().value;
                 const c = it.next().value;
                 a + ',' + b + ',' + c"
            ),
            "1,caught:e,2"
        );
    }

    #[test]
    fn generator_catches_delegated_throw() {
        // A throw raised inside a delegated (`EvalExpr`) sub-expression — here a
        // TypeError from `null.x` — is routed to the VM handler too.
        assert_eq!(
            string(
                "function* g(){ try { yield 1; null.x; } catch (e) { yield 'oops'; } }
                 const it = g();
                 it.next();
                 it.next().value"
            ),
            "oops"
        );
    }

    #[test]
    fn generator_for_in_and_destructuring() {
        // `for…in` over object keys, compiled into the VM.
        assert_eq!(
            string(
                "function* g(o){ let s=''; for (const k in o) { s = s + k; } yield s; }
                 g({ a: 1, b: 2 }).next().value"
            ),
            "ab"
        );
        // Destructuring declarations (array + object) delegate to the
        // interpreter via BindPattern.
        assert_eq!(
            num(
                "function* g(){ const [a, b] = [10, 20]; const { x } = { x: 5 }; yield a + b + x; }
                 g().next().value"
            ),
            35.0
        );
        // Destructuring catch parameter.
        assert_eq!(
            string(
                "function* g(){ try { throw { msg: 'hi' }; } catch ({ msg }) { yield msg; } }
                 g().next().value"
            ),
            "hi"
        );
    }

    #[test]
    fn generator_switch_fallthrough() {
        // `switch` with C-style fall-through, `break`, and `default`, compiled
        // into the VM (driven lazily by spread).
        assert_eq!(
            string(
                "function* g(n){
                   switch (n) {
                     case 1: yield 'one';
                     case 2: yield 'two'; break;
                     default: yield 'other';
                   }
                 }
                 [...g(1)].join(',') + '|' + [...g(2)].join(',') + '|' + [...g(9)].join(',')"
            ),
            "one,two|two|other"
        );
    }

    #[test]
    fn generator_labelled_break_and_continue() {
        // `continue outer` / `break outer` across nested loops, in the VM.
        assert_eq!(
            string(
                "function* g(){
                   outer: for (let i = 0; i < 3; i++) {
                     for (let j = 0; j < 3; j++) {
                       if (j === 1) continue outer;
                       if (i === 2) break outer;
                       yield i + ':' + j;
                     }
                   }
                 }
                 [...g()].join(',')"
            ),
            "0:0,1:0"
        );
    }

    #[test]
    fn generator_finally_runs() {
        // `finally` runs on normal completion, spanning a `yield`.
        assert_eq!(
            string(
                "function* g(){ try { yield 1; } finally { yield 'cleanup'; } yield 2; }
                 [...g()].join(',')"
            ),
            "1,cleanup,2"
        );
        // `try/catch/finally`: catch handles, finally still runs.
        assert_eq!(
            string(
                "function* g(){ let log=''; try { throw 'e'; } catch (err) { log = 'c:' + err; } finally { log = log + '|f'; } yield log; }
                 g().next().value"
            ),
            "c:e|f"
        );
        // A throw inside `try…finally` (no catch) runs finally, then propagates
        // to an outer `catch`.
        assert_eq!(
            string(
                "function* g(){ let log=''; try { try { throw 'boom'; } finally { log = 'f1'; } } catch (e) { log = log + '|caught:' + e; } yield log; }
                 g().next().value"
            ),
            "f1|caught:boom"
        );
    }

    #[test]
    fn generator_finally_with_return_falls_back() {
        // A `return` crossing the finally isn't VM-compiled; the eager fallback
        // still produces the correct values.
        assert_eq!(
            string(
                "function* g(){ try { yield 1; return; } finally {} yield 2; }
                 [...g()].join(',')"
            ),
            "1"
        );
    }

    #[test]
    fn generator_compound_assignment_with_yield() {
        // `+=` whose right-hand side suspends: the target is read once, then
        // combined with the value sent back in.
        assert_eq!(
            num(
                "function* g(){ let total = 10; total += yield 1; return total; }
                 const it = g(); it.next(); it.next(5).value"
            ),
            15.0
        );
        // Member compound assignment (`o.n += yield …`).
        assert_eq!(
            num(
                "function* g(){ const o = { n: 100 }; o.n += yield 1; return o.n; }
                 const it = g(); it.next(); it.next(7).value"
            ),
            107.0
        );
    }

    #[test]
    fn generator_call_spread_with_yield() {
        // Spread call arguments where the spread source suspends.
        assert_eq!(
            num(
                "function* g(){ const f = (a, b, c, d) => a + b + c + d; return f(1, ...(yield 0), 4); }
                 const it = g(); it.next(); it.next([2, 3]).value"
            ),
            10.0
        );
        // Spread method call (`this` bound to the receiver).
        assert_eq!(
            num(
                "function* g(){ const o = { m(a, b){ return a * 10 + b; } }; return o.m(...(yield 0)); }
                 const it = g(); it.next(); it.next([3, 4]).value"
            ),
            34.0
        );
    }

    #[test]
    fn generator_array_spread_with_yield() {
        // An array literal containing a suspension is built incrementally so a
        // `...spread` of the sent value is flattened in place.
        assert_eq!(
            string(
                "function* g(){ const arr = [1, ...(yield [2, 3]), 4]; return arr.join(','); }
                 const it = g(); it.next(); it.next([9, 9]).value"
            ),
            "1,9,9,4"
        );
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(num("1 + 2 * 3"), 7.0);
        assert_eq!(num("(1 + 2) * 3"), 9.0);
        assert_eq!(num("2 ** 3 ** 2"), 512.0);
        assert_eq!(num("7 % 3"), 1.0);
        assert_eq!(num("-5 % 3"), -2.0);
    }

    #[test]
    fn string_concatenation_and_coercion() {
        assert_eq!(string("'a' + 'b' + 'c'"), "abc");
        assert_eq!(string("1 + '2'"), "12");
        assert_eq!(string("'n=' + (3 * 4)"), "n=12");
        assert_eq!(num("'5' * 2"), 10.0);
    }

    #[test]
    fn variables_and_block_scope() {
        assert_eq!(num("let x = 10; { let x = 20; } x"), 10.0);
        assert_eq!(num("var a = 1; a += 4; a"), 5.0);
        assert_eq!(num("const k = 3; k * k"), 9.0);
    }

    #[test]
    fn closures_and_recursion() {
        assert_eq!(
            num("function fact(n){ return n <= 1 ? 1 : n * fact(n - 1); } fact(5)"),
            120.0
        );
        assert_eq!(
            num("function mk(){ let c = 0; return function(){ return ++c; }; } let f = mk(); f(); f(); f()"),
            3.0
        );
    }

    #[test]
    fn arrow_functions_lexical_this() {
        assert_eq!(
            num("let o = { v: 42, get(){ let f = () => this.v; return f(); } }; o.get()"),
            42.0
        );
        assert_eq!(num("let add = (a, b) => a + b; add(3, 4)"), 7.0);
    }

    #[test]
    fn arrays_and_indexing() {
        assert_eq!(num("let a = [1, 2, 3]; a[0] + a[2]"), 4.0);
        assert_eq!(num("let a = [1, 2, 3]; a.length"), 3.0);
        assert_eq!(num("let a = [10]; a[5] = 1; a.length"), 6.0);
    }

    #[test]
    fn objects_members_and_this() {
        assert_eq!(
            num("let o = { x: 1, inc(){ this.x += 1; return this.x; } }; o.inc(); o.inc()"),
            3.0
        );
        assert_eq!(string("let o = { a: { b: 'deep' } }; o.a.b"), "deep");
    }

    #[test]
    fn control_flow() {
        assert_eq!(
            num("let s = 0; for (let i = 1; i <= 5; i++) s += i; s"),
            15.0
        );
        assert_eq!(num("let s = 0; for (const x of [1,2,3,4]) s += x; s"), 10.0);
        assert_eq!(
            num("let i = 0, s = 0; while (i < 4) { s += i; i++; } s"),
            6.0
        );
        assert_eq!(
            num("let s = 0; for (let i = 0; i < 10; i++) { if (i === 5) break; s += i; } s"),
            10.0
        );
    }

    #[test]
    fn ternary_logical_typeof() {
        assert_eq!(string("typeof 5"), "number");
        assert_eq!(string("typeof 'x'"), "string");
        assert_eq!(string("typeof undefinedVar"), "undefined");
        assert_eq!(num("true ? 1 : 2"), 1.0);
        assert_eq!(num("0 || 7"), 7.0);
        assert_eq!(num("5 && 9"), 9.0);
        assert_eq!(num("null ?? 4"), 4.0);
    }

    #[test]
    fn equality_semantics() {
        assert!(boolean("1 == '1'"));
        assert!(!boolean("1 === '1'"));
        assert!(boolean("null == undefined"));
        assert!(!boolean("null === undefined"));
        assert!(boolean("NaN !== NaN"));
    }

    #[test]
    fn destructuring_and_defaults() {
        assert_eq!(num("let [a, b] = [1, 2]; a + b"), 3.0);
        assert_eq!(num("let { x, y } = { x: 4, y: 5 }; x * y"), 20.0);
        assert_eq!(
            num("function f({ a, b = 10 }){ return a + b; } f({ a: 1 })"),
            11.0
        );
        assert_eq!(num("let [a, ...rest] = [1, 2, 3, 4]; rest.length"), 3.0);
    }

    #[test]
    fn template_literals_runtime() {
        assert_eq!(
            string("let n = 3; `sum is ${n + n} ok`"),
            "sum is 6 ok"
        );
    }

    #[test]
    fn try_catch_throw() {
        assert_eq!(
            string("try { throw 'boom'; } catch (e) { e + '!'; }"),
            "boom!"
        );
        assert_eq!(
            num("let r = 0; try { r = 1; throw 1; } catch (e) { r = 2; } finally { r = 3; } r"),
            3.0
        );
    }

    #[test]
    fn switch_fallthrough() {
        assert_eq!(
            string("let r=''; switch (2) { case 1: r+='a'; case 2: r+='b'; case 3: r+='c'; break; case 4: r+='d'; } r"),
            "bc"
        );
    }

    #[test]
    fn new_and_prototype() {
        assert_eq!(
            num("function P(x){ this.x = x; } P.prototype.get = function(){ return this.x; }; let p = new P(7); p.get()"),
            7.0
        );
    }

    #[test]
    fn classes_runtime() {
        assert_eq!(
            num("class A { constructor(n){ this.n = n; } sq(){ return this.n * this.n; } } new A(6).sq()"),
            36.0
        );
        // Method inheritance through the prototype chain (B inherits A.v).
        assert_eq!(
            num("class A { v(){ return 5; } } class B extends A {} new B().v()"),
            5.0
        );
    }

    #[test]
    fn super_method_and_constructor() {
        // super.method()
        assert_eq!(
            num("class A { v(){ return 10; } } class B extends A { v(){ return super.v() + 5; } } new B().v()"),
            15.0
        );
        // Explicit super() in a derived constructor + own field init.
        assert_eq!(
            num("class A { constructor(x){ this.x = x; } } class B extends A { constructor(x){ super(x); this.y = x * 2; } } let b = new B(4); b.x + b.y"),
            12.0
        );
        // Implicit derived constructor forwards args via super(...arguments).
        assert_eq!(
            num("class A { constructor(x){ this.x = x; } } class B extends A {} new B(7).x"),
            7.0
        );
    }

    #[test]
    fn es_modules_transparent() {
        // `export <decl>` is a normal declaration; `import` and specifier lists
        // are elided; `export default <expr>` evaluates the expression.
        assert_eq!(
            num("export const a = 7; export function g(){ return a + 1; } g()"),
            8.0
        );
        assert_eq!(num("import x from './m'; export { a }; export default 5; 9"), 9.0);
    }

    #[test]
    fn arguments_object() {
        assert_eq!(
            num("function sum(){ var s = 0; for (var i = 0; i < arguments.length; i++) s += arguments[i]; return s; } sum(1, 2, 3, 4)"),
            10.0
        );
    }

    #[test]
    fn spread_in_calls_and_arrays() {
        assert_eq!(num("let a = [1, 2]; let b = [...a, 3, 4]; b.length"), 4.0);
        assert_eq!(
            num("function sum(a, b, c){ return a + b + c; } sum(...[1, 2, 3])"),
            6.0
        );
    }

    #[test]
    fn optional_chaining_runtime() {
        assert!(matches!(eval("let o = null; o?.a?.b"), Value::Undefined));
        assert_eq!(num("let o = { a: { b: 5 } }; o?.a?.b"), 5.0);
    }
}
