//! Bytecode for the suspendable JS VM.
//!
//! The tree-walking [`Interp`](super::interp::Interp) evaluates ordinary code,
//! but it cannot *suspend* mid-expression — a hard requirement for **lazy /
//! infinite generators** and **spec-correct `await`**. WASM is single-threaded,
//! so native coroutines are out; the only zero-dependency way to freeze and
//! resume execution is an explicit, heap-allocated machine state.
//!
//! This module defines that machine's instruction set. A [`Chunk`] is the
//! compiled form of a `function*` / `async` body: a flat list of [`Op`]s plus
//! two constant pools (literal [`Value`]s and interned name strings). Jump
//! targets are **absolute** indices into `code`, patched by the compiler.
//!
//! Design rule (keeps the VM small and semantically identical to the
//! tree-walker): the VM owns only **control flow, the operand stack, the local
//! environment and the suspension point**. Every *value* operation — binary
//! ops, property access, calls, `ToBoolean`, iteration — is delegated back to
//! `Interp`. So most opcodes are one-to-one with an AST node and carry only
//! pool indices, never inline logic.

use std::rc::Rc;

use super::ast::{BinOp, Expr, Pattern, UnOp, VarKind};
use super::value::Value;

/// A single VM instruction. Operands are indices into a [`Chunk`]'s pools or
/// absolute jump targets; nothing here interprets values itself.
#[derive(Debug, Clone)]
pub enum Op {
    // ── delegation to the tree-walker ────────────────────────────────────
    /// Evaluate `exprs[idx]` (a sub-expression containing **no** `yield`/
    /// `await`) via `Interp::eval_expr` in the frame's current scope, and push
    /// its result. This is how the VM reuses the interpreter for everything
    /// that doesn't need to suspend — keeping semantics identical.
    EvalExpr(u32),

    // ── operand stack ────────────────────────────────────────────────────
    /// Push `consts[idx]`.
    PushConst(u32),
    /// Push `undefined` / `null` / `true` / `false`.
    PushUndefined,
    PushNull,
    PushTrue,
    PushFalse,
    /// Discard the top of stack.
    Pop,
    /// Duplicate the top of stack.
    Dup,
    /// Swap the top two stack slots.
    Swap,

    // ── variables (resolved by name through the frame environment) ───────
    /// Push the value bound to `names[idx]` (walks the scope chain).
    LoadName(u32),
    /// Pop a value and assign it to `names[idx]` (existing binding).
    StoreName(u32),
    /// Declare `var names[idx]` in the function scope (hoisted).
    DeclareVar(u32),
    /// Declare `let names[idx]` in the current block scope.
    DeclareLet(u32),
    /// Declare `const names[idx]` in the current block scope.
    DeclareConst(u32),
    /// Pop a value and bind it through the destructuring pattern at
    /// `patterns[idx]` (delegated to the interpreter).
    BindPattern(u32),
    /// Enter a fresh block scope / leave it (lexical `let`/`const`).
    PushScope,
    PopScope,

    // ── operators (delegated to `Interp`) ────────────────────────────────
    /// Pop `b`, pop `a`, push `a <op> b`.
    Binary(BinOp),
    /// Pop `a`, push `<op> a`.
    Unary(UnOp),

    // ── property & element access ────────────────────────────────────────
    /// Pop `obj`, push `obj.<names[idx]>`.
    GetProp(u32),
    /// Pop `value`, pop `obj`, set `obj.<names[idx]> = value`, push `value`.
    SetProp(u32),
    /// Pop `key`, pop `obj`, push `obj[key]`.
    GetIndex,
    /// Pop `value`, pop `key`, pop `obj`, set `obj[key] = value`, push `value`.
    SetIndex,

    // ── calls & construction ─────────────────────────────────────────────
    /// Stack: `callee, arg0 … arg(argc-1)` → result. `this` is `undefined`.
    Call(u32),
    /// Stack: `obj, arg0 … arg(argc-1)` → result. Calls `obj.<names[idx]>`
    /// with `this = obj` (method-call `this` binding).
    CallMethod(u32, u32),
    /// Stack: `ctor, arg0 … arg(argc-1)` → new instance.
    New(u32),
    /// Stack: `callee, args_array` → result. Calls with `this = undefined` and
    /// the array's elements as arguments (`f(...spread)`).
    CallApply,
    /// Stack: `obj, args_array` → result. Calls `obj.<names[idx]>` with
    /// `this = obj` and the array's elements as arguments.
    CallMethodApply(u32),

    // ── literals ─────────────────────────────────────────────────────────
    /// Pop `n` elements and push an array of them (in stack order).
    MakeArray(u32),
    /// Push a fresh empty array (start of a spread-containing array literal).
    NewArray,
    /// Pop a value and append it to the array now on top of the stack.
    ArrayAppend,
    /// Pop an iterable and append all its values to the array on top of the
    /// stack (`...spread`).
    ArrayAppendSpread,
    /// Pop `2 × n` values (`key0, val0, …`) and push an object.
    MakeObject(u32),

    // ── control flow (absolute targets into `code`) ──────────────────────
    /// Unconditional jump.
    Jump(u32),
    /// Pop a value; jump if it is falsy.
    JumpIfFalse(u32),
    /// Pop a value; jump if it is truthy.
    JumpIfTrue(u32),
    /// `&&`: if the **kept** top is falsy, jump; else pop and continue.
    JumpIfFalsyKeep(u32),
    /// `||`: if the **kept** top is truthy, jump; else pop and continue.
    JumpIfTruthyKeep(u32),
    /// `??`: if the **kept** top is non-nullish, jump; else pop and continue.
    JumpIfNullishKeep(u32),

    // ── iteration (step-wise, so `for…of` stays lazy in a generator) ─────
    /// Pop an iterable, push its iterator object.
    GetIterator,
    /// Pop an object, push an iterator over its enumerable string keys
    /// (`for…in`).
    GetEnumIterator,
    /// Peek the iterator on top; call `.next()`. On `{done:true}` jump to the
    /// target (leaving the iterator), otherwise push the yielded value.
    IterNext(u32),

    // ── suspension (the whole reason this VM exists) ─────────────────────
    /// Pop a value, **suspend** the frame yielding it; on resume push the sent
    /// value (`gen.next(v)`). `yield*` is desugared by the compiler into a
    /// step-wise `GetIterator`/`IterNext`/`Yield` loop, so it stays lazy.
    Yield,
    /// Pop a value, **suspend** awaiting it; on resume push the settled value.
    Await,

    // ── exceptions ───────────────────────────────────────────────────────
    /// Enter a `try`: install an exception handler whose `catch` block starts
    /// at the given address. A subsequent throw unwinds to it.
    PushHandler(u32),
    /// Leave a `try` on the normal path: discard the innermost handler.
    PopHandler,
    /// Enter a `try…finally`: install a finally handler whose block starts at
    /// the given address. A throw unwinds to it with a *pending* completion.
    PushFinally(u32),
    /// Discard the innermost finally handler on the normal path (the finally
    /// block is about to run explicitly).
    PopFinally,
    /// End of a finally block: if a throw was pending, re-raise it (to an outer
    /// handler or out of the frame); otherwise continue normally.
    EndFinally,

    // ── returns & throws ─────────────────────────────────────────────────
    /// Pop a value and return it from the function.
    Return,
    /// Return `undefined`.
    ReturnUndefined,
    /// Pop a value and throw it (caught by an installed handler, else
    /// propagates out of the frame).
    Throw,
}

/// A compiled `function*` / `async` body: instructions plus constant pools.
///
/// `consts` holds literal operand [`Value`]s (numbers, strings, …); `names`
/// holds interned identifier / property-key strings referenced by the
/// `*Name` and `*Prop` opcodes. Keeping names separate lets several opcodes
/// share one interned string and keeps `Op` a small `Copy`-ish enum.
#[derive(Debug, Default, Clone)]
pub struct Chunk {
    pub code: Vec<Op>,
    pub consts: Vec<Value>,
    pub names: Vec<Rc<str>>,
    /// Sub-expressions with no suspension, evaluated wholesale by `EvalExpr`
    /// through the tree-walking interpreter.
    pub exprs: Vec<Expr>,
    /// Destructuring patterns + their declaration kind, bound by `BindPattern`.
    pub patterns: Vec<(Pattern, VarKind)>,
}

impl Chunk {
    /// A new, empty chunk.
    pub fn new() -> Self {
        Chunk::default()
    }

    /// Append an instruction, returning its index (useful for back-patching).
    pub fn emit(&mut self, op: Op) -> usize {
        self.code.push(op);
        self.code.len() - 1
    }

    /// Intern a literal value, returning its constant-pool index.
    pub fn add_const(&mut self, v: Value) -> u32 {
        let idx = self.consts.len() as u32;
        self.consts.push(v);
        idx
    }

    /// Store a sub-expression for [`Op::EvalExpr`], returning its pool index.
    pub fn add_expr(&mut self, e: Expr) -> u32 {
        let idx = self.exprs.len() as u32;
        self.exprs.push(e);
        idx
    }

    /// Store a destructuring pattern for [`Op::BindPattern`].
    pub fn add_pattern(&mut self, pat: Pattern, kind: VarKind) -> u32 {
        let idx = self.patterns.len() as u32;
        self.patterns.push((pat, kind));
        idx
    }

    /// Intern a name string (deduplicated), returning its name-pool index.
    pub fn add_name(&mut self, name: &str) -> u32 {
        if let Some(i) = self.names.iter().position(|n| &**n == name) {
            return i as u32;
        }
        let idx = self.names.len() as u32;
        self.names.push(Rc::from(name));
        idx
    }

    /// Current instruction count — the address the next `emit` will occupy.
    pub fn here(&self) -> u32 {
        self.code.len() as u32
    }

    /// Rewrite a previously-emitted jump's absolute target to `dest`. Panics in
    /// debug builds if `at` is not a jump opcode (a compiler bug).
    pub fn patch_jump(&mut self, at: usize, dest: u32) {
        match &mut self.code[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfTrue(t)
            | Op::JumpIfFalsyKeep(t)
            | Op::JumpIfTruthyKeep(t)
            | Op::JumpIfNullishKeep(t)
            | Op::IterNext(t)
            | Op::PushHandler(t)
            | Op::PushFinally(t) => *t = dest,
            other => debug_assert!(false, "patch_jump on non-jump op: {other:?}"),
        }
    }

    /// A compact, human-readable disassembly (one line per instruction) for
    /// debugging the compiler and VM.
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        for (i, op) in self.code.iter().enumerate() {
            out.push_str(&format!("{i:04}  {op:?}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_patches_a_chunk() {
        let mut c = Chunk::new();
        let k = c.add_const(Value::Num(42.0));
        let n = c.add_name("x");
        // dedup: the same name reuses its slot.
        assert_eq!(c.add_name("x"), n);

        c.emit(Op::PushConst(k));
        c.emit(Op::StoreName(n));
        let jmp = c.emit(Op::Jump(0)); // placeholder target
        let target = c.here();
        c.emit(Op::LoadName(n));
        c.emit(Op::Return);
        c.patch_jump(jmp, target);

        assert_eq!(c.consts.len(), 1);
        assert_eq!(c.names.len(), 1);
        assert!(matches!(c.code[jmp], Op::Jump(t) if t == target));
        assert!(c.disassemble().contains("Jump"));
    }
}
