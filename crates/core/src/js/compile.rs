//! Compile a `function*` / `async` body to suspendable [`Chunk`] bytecode.
//!
//! Only the **control-flow skeleton and the suspension points** become real
//! opcodes. Any sub-expression that contains no `yield`/`await`
//! ([`contains_suspension`] is false) is emitted as a single
//! [`Op::EvalExpr`], evaluated wholesale by the tree-walker — so the compiler
//! stays small and its semantics match the interpreter exactly.
//!
//! Compilation is **best-effort**: every method returns `Option<()>` and a
//! `None` anywhere aborts the whole compile (`compile_body` returns `None`).
//! The caller then falls back to the eager generator path, so an exotic body
//! never regresses — it just isn't lazy.

use super::ast::{
    AssignOp, BinOp, Catch, Expr, ForHead, ForInit, MemberProp, Pattern, PropKey, PropValue, Stmt,
    SwitchCase, UnOp, VarKind,
};
use super::bytecode::{Chunk, Op};

/// Compile a function body's statements. `None` ⇒ unsupported ⇒ eager fallback.
pub fn compile_body(stmts: &[Stmt]) -> Option<Chunk> {
    let mut c = Compiler {
        chunk: Chunk::new(),
        loops: Vec::new(),
        handler_depth: 0,
        pending_label: None,
        switch_depth: 0,
    };
    c.stmts(stmts)?;
    c.chunk.emit(Op::ReturnUndefined);
    Some(c.chunk)
}

/// Does `e` contain a `yield`/`await` that belongs to *this* function (i.e. not
/// nested inside another function/class boundary)?
pub fn contains_suspension(e: &Expr) -> bool {
    match e {
        Expr::Yield { .. } | Expr::Await(_) => true,
        // Nested functions/classes start their own suspension scope.
        Expr::Func(_) | Expr::Arrow(_) | Expr::Class(_) => false,
        Expr::Unary { arg, .. } | Expr::Update { arg, .. } | Expr::Spread(arg) => {
            contains_suspension(arg)
        }
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            contains_suspension(left) || contains_suspension(right)
        }
        Expr::Assign { target, value, .. } => {
            contains_suspension(target) || contains_suspension(value)
        }
        Expr::Conditional { test, cons, alt } => {
            contains_suspension(test) || contains_suspension(cons) || contains_suspension(alt)
        }
        Expr::Call { callee, args, .. } | Expr::New { callee, args } => {
            contains_suspension(callee) || args.iter().any(contains_suspension)
        }
        Expr::Member {
            object, property, ..
        } => {
            contains_suspension(object)
                || matches!(&**property, MemberProp::Computed(e) if contains_suspension(e))
        }
        Expr::Array(els) => els.iter().flatten().any(contains_suspension),
        Expr::Object(props) => props.iter().any(|p| {
            matches!(&p.key, PropKey::Computed(e) if contains_suspension(e))
                || match &p.value {
                    PropValue::Expr(e) | PropValue::Spread(e) => contains_suspension(e),
                    PropValue::None => false,
                }
        }),
        Expr::Sequence(es) => es.iter().any(contains_suspension),
        Expr::Template { exprs, .. } | Expr::TaggedTemplate { exprs, .. } => {
            exprs.iter().any(contains_suspension)
        }
        _ => false,
    }
}

/// A breakable / continuable control context (loop, `switch`, or labelled
/// statement) for `break`/`continue` fix-up.
struct LoopCtx {
    break_jumps: Vec<usize>,
    continue_jumps: Vec<usize>,
    /// Active `try` handler depth when the context was entered, so a `break`/
    /// `continue` crossing a `try` can pop the handlers it escapes.
    handler_depth: usize,
    /// Source label, if this context was introduced by `label:`.
    label: Option<String>,
    /// `true` for a `switch` (breakable but **not** continuable).
    is_switch: bool,
}

impl LoopCtx {
    fn new(handler_depth: usize, label: Option<String>, is_switch: bool) -> Self {
        LoopCtx {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            handler_depth,
            label,
            is_switch,
        }
    }
}

struct Compiler {
    chunk: Chunk,
    loops: Vec<LoopCtx>,
    /// Number of `try` handlers installed along the current path.
    handler_depth: usize,
    /// A `label:` seen immediately before the next loop/switch, to attach to it.
    pending_label: Option<String>,
    /// Nesting depth of `switch`, for unique discriminant temporaries.
    switch_depth: usize,
}

impl Compiler {
    // ── statements ───────────────────────────────────────────────────────

    fn stmts(&mut self, stmts: &[Stmt]) -> Option<()> {
        for s in stmts {
            self.stmt(s)?;
        }
        Some(())
    }

    fn stmt(&mut self, s: &Stmt) -> Option<()> {
        match s {
            Stmt::Empty | Stmt::Debugger => Some(()),
            // Hoisted before the frame runs (like the eager path) → no opcodes.
            Stmt::FuncDecl(_) => Some(()),
            Stmt::Expr(e) => {
                self.push_value(e)?;
                self.chunk.emit(Op::Pop);
                Some(())
            }
            Stmt::Block(b) => {
                self.chunk.emit(Op::PushScope);
                let r = self.stmts(b);
                self.chunk.emit(Op::PopScope);
                r
            }
            Stmt::VarDecl { kind, decls } => {
                for d in decls {
                    match &d.init {
                        Some(init) => self.push_value(init)?,
                        None => {
                            self.chunk.emit(Op::PushUndefined);
                        }
                    }
                    match &d.id {
                        Pattern::Ident(n) => {
                            let idx = self.chunk.add_name(n);
                            self.chunk.emit(match kind {
                                VarKind::Var => Op::DeclareVar(idx),
                                VarKind::Let => Op::DeclareLet(idx),
                                VarKind::Const => Op::DeclareConst(idx),
                            });
                        }
                        // Destructuring (`const { a } = o;`) → delegate the
                        // pattern binding to the interpreter.
                        pat => {
                            let idx = self.chunk.add_pattern(pat.clone(), *kind);
                            self.chunk.emit(Op::BindPattern(idx));
                        }
                    }
                }
                Some(())
            }
            Stmt::Return(e) => {
                match e {
                    Some(v) => {
                        self.push_value(v)?;
                        self.chunk.emit(Op::Return);
                    }
                    None => {
                        self.chunk.emit(Op::ReturnUndefined);
                    }
                }
                Some(())
            }
            Stmt::Throw(e) => {
                self.push_value(e)?;
                self.chunk.emit(Op::Throw);
                Some(())
            }
            Stmt::If { test, cons, alt } => {
                self.push_value(test)?;
                let to_else = self.chunk.emit(Op::JumpIfFalse(0));
                self.stmt(cons)?;
                match alt {
                    Some(a) => {
                        let to_end = self.chunk.emit(Op::Jump(0));
                        let else_at = self.chunk.here();
                        self.chunk.patch_jump(to_else, else_at);
                        self.stmt(a)?;
                        let end = self.chunk.here();
                        self.chunk.patch_jump(to_end, end);
                    }
                    None => {
                        let end = self.chunk.here();
                        self.chunk.patch_jump(to_else, end);
                    }
                }
                Some(())
            }
            Stmt::While { test, body } => {
                let start = self.chunk.here();
                self.push_value(test)?;
                let to_end = self.chunk.emit(Op::JumpIfFalse(0));
                let label = self.pending_label.take();
                self.loops.push(LoopCtx::new(self.handler_depth, label, false));
                let body_ok = self.stmt(body);
                self.chunk.emit(Op::Jump(start));
                let end = self.chunk.here();
                self.chunk.patch_jump(to_end, end);
                self.finish_loop(start, end);
                body_ok
            }
            Stmt::DoWhile { body, test } => {
                let start = self.chunk.here();
                let label = self.pending_label.take();
                self.loops.push(LoopCtx::new(self.handler_depth, label, false));
                let body_ok = self.stmt(body);
                let cont = self.chunk.here();
                if self.push_value(test).is_none() {
                    self.loops.pop();
                    return None;
                }
                self.chunk.emit(Op::JumpIfTrue(start));
                let end = self.chunk.here();
                self.finish_loop(cont, end);
                body_ok
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => self.compile_for(init.as_deref(), test.as_ref(), update.as_ref(), body),
            Stmt::ForOf { left, right, body } => self.compile_for_of(left, right, body),
            Stmt::ForIn { left, right, body } => self.compile_for_in(left, right, body),
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => self.compile_try(block, handler.as_ref(), finalizer.as_ref()),
            Stmt::Switch { disc, cases } => self.compile_switch(disc, cases),
            Stmt::Labeled { label, body } => {
                self.pending_label = Some(label.clone());
                let r = self.stmt(body);
                self.pending_label = None; // clear if the body didn't consume it
                r
            }
            Stmt::Break(_) | Stmt::Continue(_) => self.simple_control(s),
            // Class declarations and anything else not modelled → eager fallback.
            _ => None,
        }
    }

    fn compile_try(
        &mut self,
        block: &[Stmt],
        handler: Option<&Catch>,
        finalizer: Option<&Vec<Stmt>>,
    ) -> Option<()> {
        match finalizer {
            None => self.compile_try_catch(block, handler?),
            Some(fin) => self.compile_try_finally(block, handler, fin),
        }
    }

    /// Bind a `catch` parameter (identifier, destructuring, or omitted): the
    /// thrown value is on the stack.
    fn bind_catch_param(&mut self, param: &Option<Pattern>) {
        match param {
            Some(Pattern::Ident(n)) => {
                let idx = self.chunk.add_name(n);
                self.chunk.emit(Op::DeclareLet(idx));
            }
            Some(pat) => {
                let idx = self.chunk.add_pattern(pat.clone(), VarKind::Let);
                self.chunk.emit(Op::BindPattern(idx));
            }
            None => {
                self.chunk.emit(Op::Pop); // optional binding: discard
            }
        }
    }

    /// `try { B } catch (e) { C }` — installs a handler, runs `B`, and on a
    /// throw unwinds to the catch with the thrown value bound.
    fn compile_try_catch(&mut self, block: &[Stmt], handler: &Catch) -> Option<()> {
        let ph = self.chunk.emit(Op::PushHandler(0));
        self.handler_depth += 1;
        let body_ok = self.stmts(block);
        self.chunk.emit(Op::PopHandler);
        self.handler_depth -= 1;
        let to_end = self.chunk.emit(Op::Jump(0));

        // The catch entry: a throw arrives here with the value already pushed.
        let catch_ip = self.chunk.here();
        self.chunk.patch_jump(ph, catch_ip);
        self.chunk.emit(Op::PushScope);
        self.bind_catch_param(&handler.param);
        let catch_ok = self.stmts(&handler.body);
        self.chunk.emit(Op::PopScope);

        let end = self.chunk.here();
        self.chunk.patch_jump(to_end, end);
        body_ok.and(catch_ok)
    }

    /// `try { B } [catch (e) { C }] finally { F }` for the **normal-completion
    /// and throw** paths: `F` always runs, and a throw is re-raised afterwards.
    /// Bails (eager fallback) if `B`/`C` can complete abruptly *through* the
    /// finally (a `return`, or a `break`/`continue` leaving the block) — the
    /// pending-completion case isn't modelled.
    fn compile_try_finally(
        &mut self,
        block: &[Stmt],
        handler: Option<&Catch>,
        fin: &[Stmt],
    ) -> Option<()> {
        if crosses_finally(block) || handler.is_some_and(|h| crosses_finally(&h.body)) {
            return None;
        }

        let pf = self.chunk.emit(Op::PushFinally(0));
        let ph = handler.map(|_| self.chunk.emit(Op::PushHandler(0)));
        let body_ok = self.stmts(block).is_some();
        if ph.is_some() {
            self.chunk.emit(Op::PopHandler); // normal: drop the catch handler
        }
        self.chunk.emit(Op::PopFinally); // normal: drop the finally handler
        let to_finally_normal = self.chunk.emit(Op::Jump(0));

        // The catch block (a throw in `B` lands here, value on the stack).
        let mut to_finally_catch = None;
        let mut catch_ok = true;
        if let Some(h) = handler {
            let catch_ip = self.chunk.here();
            self.chunk.patch_jump(ph.unwrap(), catch_ip);
            self.chunk.emit(Op::PushScope);
            self.bind_catch_param(&h.param);
            catch_ok = self.stmts(&h.body).is_some();
            self.chunk.emit(Op::PopScope);
            self.chunk.emit(Op::PopFinally); // catch completed: drop finally handler
            to_finally_catch = Some(self.chunk.emit(Op::Jump(0)));
        }

        // The finally block — reached normally (pending none) or by a throw
        // (pending set, re-raised by EndFinally). The finally handler is always
        // already removed (PopFinally on the normal/catch paths, or consumed by
        // the unwind on the throw path).
        let f_code = self.chunk.here();
        self.chunk.patch_jump(pf, f_code);
        self.chunk.patch_jump(to_finally_normal, f_code);
        if let Some(j) = to_finally_catch {
            self.chunk.patch_jump(j, f_code);
        }
        let fin_ok = self.stmts(fin).is_some();
        self.chunk.emit(Op::EndFinally);

        (body_ok && catch_ok && fin_ok).then_some(())
    }

    /// Index of the `break`/`continue` target context: the nearest matching
    /// label (or innermost when unlabelled); `continue` skips `switch` contexts.
    fn resolve_target(&self, label: Option<&str>, continuable: bool) -> Option<usize> {
        self.loops.iter().rposition(|c| {
            if continuable && c.is_switch {
                return false;
            }
            match label {
                Some(l) => c.label.as_deref() == Some(l),
                None => true,
            }
        })
    }

    /// `break`/`continue`, labelled or not. Pops any `try` handlers the jump
    /// escapes before branching to the target context.
    fn simple_control(&mut self, s: &Stmt) -> Option<()> {
        let (is_break, label) = match s {
            Stmt::Break(l) => (true, l.as_deref()),
            Stmt::Continue(l) => (false, l.as_deref()),
            _ => return None,
        };
        let target = self.resolve_target(label, !is_break)?;
        let target_depth = self.loops[target].handler_depth;
        for _ in 0..self.handler_depth.saturating_sub(target_depth) {
            self.chunk.emit(Op::PopHandler);
        }
        let j = self.chunk.emit(Op::Jump(0));
        if is_break {
            self.loops[target].break_jumps.push(j);
        } else {
            self.loops[target].continue_jumps.push(j);
        }
        Some(())
    }

    /// `switch (disc) { … }` — a jump table over `===` comparisons with C-style
    /// fall-through; `break` exits. The discriminant is held in a synthetic
    /// block-scoped temporary so case bodies don't juggle it on the stack.
    fn compile_switch(&mut self, disc: &Expr, cases: &[SwitchCase]) -> Option<()> {
        let label = self.pending_label.take();
        self.chunk.emit(Op::PushScope);
        let dn = self.chunk.add_name(&format!("@sw{}", self.switch_depth));
        self.switch_depth += 1;
        self.push_value(disc)?;
        self.chunk.emit(Op::DeclareLet(dn));

        // Jump table: `disc === case_i` → jump to body_i. A `default` is the
        // fallback when nothing matches.
        self.loops
            .push(LoopCtx::new(self.handler_depth, label, true));
        let mut case_jumps: Vec<Option<usize>> = Vec::with_capacity(cases.len());
        let mut default_idx: Option<usize> = None;
        for (i, case) in cases.iter().enumerate() {
            match &case.test {
                Some(test) => {
                    self.chunk.emit(Op::LoadName(dn));
                    self.push_value(test)?;
                    self.chunk.emit(Op::Binary(BinOp::StrictEq));
                    case_jumps.push(Some(self.chunk.emit(Op::JumpIfTrue(0))));
                }
                None => {
                    default_idx = Some(i);
                    case_jumps.push(None);
                }
            }
        }
        let to_default = self.chunk.emit(Op::Jump(0));

        let mut body_addr = Vec::with_capacity(cases.len());
        let mut bodies_ok = true;
        for case in cases {
            body_addr.push(self.chunk.here());
            bodies_ok = bodies_ok && self.stmts(&case.body).is_some();
        }
        let end = self.chunk.here();

        for (j, addr) in case_jumps.iter().zip(&body_addr) {
            if let Some(j) = j {
                self.chunk.patch_jump(*j, *addr);
            }
        }
        match default_idx {
            Some(i) => self.chunk.patch_jump(to_default, body_addr[i]),
            None => self.chunk.patch_jump(to_default, end),
        }
        if let Some(ctx) = self.loops.pop() {
            for j in ctx.break_jumps {
                self.chunk.patch_jump(j, end);
            }
        }
        self.chunk.emit(Op::PopScope);
        self.switch_depth -= 1;
        bodies_ok.then_some(())
    }

    fn compile_for(
        &mut self,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> Option<()> {
        self.chunk.emit(Op::PushScope);
        match init {
            Some(ForInit::Expr(e)) => {
                self.push_value(e)?;
                self.chunk.emit(Op::Pop);
            }
            Some(ForInit::VarDecl { kind, decls }) => {
                self.stmt(&Stmt::VarDecl {
                    kind: *kind,
                    decls: decls.clone(),
                })?;
            }
            None => {}
        }
        let start = self.chunk.here();
        let to_end = match test {
            Some(t) => {
                self.push_value(t)?;
                Some(self.chunk.emit(Op::JumpIfFalse(0)))
            }
            None => None,
        };
        let label = self.pending_label.take();
        self.loops.push(LoopCtx::new(self.handler_depth, label, false));
        let body_ok = self.stmt(body);
        let cont = self.chunk.here();
        if let Some(u) = update {
            if self.push_value(u).is_none() {
                self.loops.pop();
                return None;
            }
            self.chunk.emit(Op::Pop);
        }
        self.chunk.emit(Op::Jump(start));
        let end = self.chunk.here();
        if let Some(j) = to_end {
            self.chunk.patch_jump(j, end);
        }
        self.finish_loop(cont, end);
        self.chunk.emit(Op::PopScope);
        body_ok
    }

    /// `for (left of right) body` — the iterator is driven **step-wise** so a
    /// `yield` inside the loop keeps the source lazy.
    fn compile_for_of(&mut self, left: &ForHead, right: &Expr, body: &Stmt) -> Option<()> {
        self.compile_for_each(left, right, body, Op::GetIterator)
    }

    /// `for (left in right) body` — iterates the object's enumerable keys.
    fn compile_for_in(&mut self, left: &ForHead, right: &Expr, body: &Stmt) -> Option<()> {
        self.compile_for_each(left, right, body, Op::GetEnumIterator)
    }

    /// Shared `for…of` / `for…in` skeleton: obtain an iterator with `iter_op`,
    /// then step it, binding each value to the loop head before the body.
    fn compile_for_each(
        &mut self,
        left: &ForHead,
        right: &Expr,
        body: &Stmt,
        iter_op: Op,
    ) -> Option<()> {
        self.push_value(right)?;
        self.chunk.emit(iter_op); // stack: [iterator]
        let start = self.chunk.here();
        let to_end = self.chunk.emit(Op::IterNext(0)); // done → jump (leaves iterator)
        // stack: [iterator, value]
        self.chunk.emit(Op::PushScope);
        self.bind_for_head(left)?; // pops the value, binds the loop variable(s)
        let label = self.pending_label.take();
        self.loops.push(LoopCtx::new(self.handler_depth, label, false));
        let body_ok = self.stmt(body);
        self.chunk.emit(Op::PopScope);
        self.chunk.emit(Op::Jump(start));
        let end = self.chunk.here();
        self.chunk.patch_jump(to_end, end);
        self.chunk.emit(Op::Pop); // discard the iterator
        // continue jumps to `start` (re-enter the per-iteration scope cleanly).
        self.finish_loop(start, end);
        body_ok
    }

    /// Bind the per-iteration value (on the stack) to a `for…of`/`for…in` head:
    /// a fresh `let`/`const`/`var` declaration, an existing reference, or a
    /// destructuring pattern.
    fn bind_for_head(&mut self, left: &ForHead) -> Option<()> {
        match left {
            ForHead::Decl {
                kind,
                pat: Pattern::Ident(n),
            } => {
                let idx = self.chunk.add_name(n);
                self.chunk.emit(match kind {
                    VarKind::Var => Op::DeclareVar(idx),
                    VarKind::Let => Op::DeclareLet(idx),
                    VarKind::Const => Op::DeclareConst(idx),
                });
            }
            ForHead::Pattern(Pattern::Ident(n)) => {
                let idx = self.chunk.add_name(n);
                self.chunk.emit(Op::StoreName(idx));
            }
            // Destructuring declaration head (`for (const [a,b] of …)`):
            // delegate the pattern binding to the interpreter.
            ForHead::Decl { kind, pat } => {
                let idx = self.chunk.add_pattern(pat.clone(), *kind);
                self.chunk.emit(Op::BindPattern(idx));
            }
            // `for ([a,b] of …)` (assignment to an existing target) is rare;
            // fall back to the eager interpreter.
            ForHead::Pattern(_) => return None,
        }
        Some(())
    }

    /// Pop the current loop context and patch its `break`/`continue` jumps.
    fn finish_loop(&mut self, continue_target: u32, break_target: u32) {
        if let Some(ctx) = self.loops.pop() {
            for j in ctx.break_jumps {
                self.chunk.patch_jump(j, break_target);
            }
            for j in ctx.continue_jumps {
                self.chunk.patch_jump(j, continue_target);
            }
        }
    }

    // ── expressions ──────────────────────────────────────────────────────

    /// Evaluate `e`, leaving its value on the operand stack. Pure
    /// sub-expressions are delegated to the interpreter via `EvalExpr`; only
    /// suspending ones are decomposed into bytecode.
    fn push_value(&mut self, e: &Expr) -> Option<()> {
        if !contains_suspension(e) {
            let idx = self.chunk.add_expr(e.clone());
            self.chunk.emit(Op::EvalExpr(idx));
            return Some(());
        }
        self.expr(e)
    }

    /// Decompose a suspending expression into bytecode.
    fn expr(&mut self, e: &Expr) -> Option<()> {
        match e {
            Expr::Yield {
                arg,
                delegate: false,
            } => {
                match arg {
                    Some(a) => self.push_value(a)?,
                    None => {
                        self.chunk.emit(Op::PushUndefined);
                    }
                }
                self.chunk.emit(Op::Yield);
                Some(())
            }
            Expr::Yield {
                arg: Some(a),
                delegate: true,
            } => {
                // `yield* it` desugars to a step-wise loop that yields each of
                // `it`'s values, suspending between them — so it stays lazy:
                //   <it>; GetIterator; L: IterNext(END); Yield; Pop; Jump L;
                //   END: Pop; PushUndefined
                self.push_value(a)?;
                self.chunk.emit(Op::GetIterator);
                let start = self.chunk.here();
                let to_end = self.chunk.emit(Op::IterNext(0));
                self.chunk.emit(Op::Yield);
                self.chunk.emit(Op::Pop); // discard the value sent back in
                self.chunk.emit(Op::Jump(start));
                let end = self.chunk.here();
                self.chunk.patch_jump(to_end, end);
                self.chunk.emit(Op::Pop); // discard the iterator
                self.chunk.emit(Op::PushUndefined); // value of the `yield*` expr
                Some(())
            }
            // `yield*` with no argument is a syntax error the parser rejects;
            // guard anyway.
            Expr::Yield { delegate: true, .. } => None,
            Expr::Await(inner) => {
                self.push_value(inner)?;
                self.chunk.emit(Op::Await);
                Some(())
            }
            Expr::Binary { op, left, right } => {
                self.push_value(left)?;
                self.push_value(right)?;
                self.chunk.emit(Op::Binary(*op));
                Some(())
            }
            Expr::Logical { op, left, right } => self.logical(*op, left, right),
            Expr::Conditional { test, cons, alt } => {
                self.push_value(test)?;
                let to_alt = self.chunk.emit(Op::JumpIfFalse(0));
                self.push_value(cons)?;
                let to_end = self.chunk.emit(Op::Jump(0));
                let alt_at = self.chunk.here();
                self.chunk.patch_jump(to_alt, alt_at);
                self.push_value(alt)?;
                let end = self.chunk.here();
                self.chunk.patch_jump(to_end, end);
                Some(())
            }
            Expr::Sequence(es) => {
                for (i, e) in es.iter().enumerate() {
                    self.push_value(e)?;
                    if i + 1 != es.len() {
                        self.chunk.emit(Op::Pop);
                    }
                }
                Some(())
            }
            Expr::Unary { op, arg } => {
                if matches!(op, UnOp::Delete) {
                    return None; // delete needs a reference, not a value
                }
                self.push_value(arg)?;
                self.chunk.emit(Op::Unary(*op));
                Some(())
            }
            Expr::Assign { op, target, value } => self.assign(*op, target, value),
            Expr::Call {
                callee,
                args,
                optional,
            } => self.call(callee, args, *optional),
            Expr::New { callee, args } => {
                if args.iter().any(is_spread) {
                    return None;
                }
                self.push_value(callee)?;
                for a in args {
                    self.push_value(a)?;
                }
                self.chunk.emit(Op::New(args.len() as u32));
                Some(())
            }
            Expr::Member {
                object,
                property,
                computed,
                optional,
            } => {
                if *optional {
                    return None;
                }
                self.push_value(object)?;
                match (&**property, *computed) {
                    (MemberProp::Ident(name), false) => {
                        let idx = self.chunk.add_name(name);
                        self.chunk.emit(Op::GetProp(idx));
                    }
                    (MemberProp::Computed(k), true) => {
                        self.push_value(k)?;
                        self.chunk.emit(Op::GetIndex);
                    }
                    _ => return None,
                }
                Some(())
            }
            Expr::Array(els) => {
                if els.iter().any(Option::is_none) {
                    return None; // sparse arrays (holes) → fallback
                }
                if els.iter().flatten().any(is_spread) {
                    // Build incrementally so `...spread` can be flattened.
                    self.chunk.emit(Op::NewArray);
                    for el in els.iter().flatten() {
                        match el {
                            Expr::Spread(inner) => {
                                self.push_value(inner)?;
                                self.chunk.emit(Op::ArrayAppendSpread);
                            }
                            e => {
                                self.push_value(e)?;
                                self.chunk.emit(Op::ArrayAppend);
                            }
                        }
                    }
                } else {
                    for el in els.iter().flatten() {
                        self.push_value(el)?;
                    }
                    self.chunk.emit(Op::MakeArray(els.len() as u32));
                }
                Some(())
            }
            Expr::Object(props) => {
                use super::ast::PropKind;
                for p in props {
                    if p.kind != PropKind::Init || p.computed {
                        return None;
                    }
                    let key = match &p.key {
                        PropKey::Ident(s) | PropKey::Str(s) => s.clone(),
                        PropKey::Num(n) => super::value::num_to_str(*n),
                        PropKey::Computed(_) => return None,
                    };
                    let k = self.chunk.add_const(super::value::Value::Str(key.into()));
                    self.chunk.emit(Op::PushConst(k));
                    match &p.value {
                        PropValue::Expr(v) => self.push_value(v)?,
                        _ => return None,
                    }
                }
                self.chunk.emit(Op::MakeObject(props.len() as u32));
                Some(())
            }
            Expr::Template { quasis, exprs } => {
                // Build the string by `+`-concatenation, seeded with quasis[0]
                // so coercion is string-wise.
                let first = self
                    .chunk
                    .add_const(super::value::Value::Str(quasis[0].clone().into()));
                self.chunk.emit(Op::PushConst(first));
                for (i, e) in exprs.iter().enumerate() {
                    self.push_value(e)?;
                    self.chunk.emit(Op::Binary(super::ast::BinOp::Add));
                    let q = self
                        .chunk
                        .add_const(super::value::Value::Str(quasis[i + 1].clone().into()));
                    self.chunk.emit(Op::PushConst(q));
                    self.chunk.emit(Op::Binary(super::ast::BinOp::Add));
                }
                Some(())
            }
            // Spread/Update/TaggedTemplate/Super with a suspension inside →
            // fall back to the eager interpreter.
            _ => None,
        }
    }

    fn logical(&mut self, op: super::ast::LogicalOp, left: &Expr, right: &Expr) -> Option<()> {
        use super::ast::LogicalOp;
        self.push_value(left)?;
        let short = match op {
            LogicalOp::And => self.chunk.emit(Op::JumpIfFalsyKeep(0)),
            LogicalOp::Or => self.chunk.emit(Op::JumpIfTruthyKeep(0)),
            LogicalOp::Nullish => self.chunk.emit(Op::JumpIfNullishKeep(0)),
        };
        self.chunk.emit(Op::Pop); // discard the left value, take the right
        self.push_value(right)?;
        let end = self.chunk.here();
        self.chunk.patch_jump(short, end);
        Some(())
    }

    fn assign(&mut self, op: AssignOp, target: &Expr, value: &Expr) -> Option<()> {
        if op != AssignOp::Assign {
            // Arithmetic compound assignment (`x += await v`, …); logical
            // compound (`&&=`/`||=`/`??=`) needs short-circuit reads → fallback.
            return self.compound_assign(compound_binop(op)?, target, value);
        }
        match target {
            Expr::Ident(name) => {
                self.push_value(value)?;
                self.chunk.emit(Op::Dup); // assignment evaluates to the value
                let idx = self.chunk.add_name(name);
                self.chunk.emit(Op::StoreName(idx));
                Some(())
            }
            Expr::Member {
                object,
                property,
                computed,
                optional: false,
            } => {
                self.push_value(object)?;
                match (&**property, *computed) {
                    (MemberProp::Ident(name), false) => {
                        self.push_value(value)?;
                        let idx = self.chunk.add_name(name);
                        self.chunk.emit(Op::SetProp(idx));
                    }
                    (MemberProp::Computed(k), true) => {
                        self.push_value(k)?;
                        self.push_value(value)?;
                        self.chunk.emit(Op::SetIndex);
                    }
                    _ => return None,
                }
                Some(())
            }
            _ => None, // destructuring assignment target → fallback
        }
    }

    /// Arithmetic compound assignment (`target <bop>= value`) where `value`
    /// contains a suspension. Reads the target, combines, and stores — the
    /// target is evaluated once. Supports an identifier or a non-computed
    /// member; other targets fall back.
    fn compound_assign(&mut self, bop: BinOp, target: &Expr, value: &Expr) -> Option<()> {
        match target {
            Expr::Ident(name) => {
                let read = self.chunk.add_expr(Expr::Ident(name.clone()));
                self.chunk.emit(Op::EvalExpr(read)); // current value
                self.push_value(value)?;
                self.chunk.emit(Op::Binary(bop));
                self.chunk.emit(Op::Dup); // the assignment's own value
                let idx = self.chunk.add_name(name);
                self.chunk.emit(Op::StoreName(idx));
                Some(())
            }
            Expr::Member {
                object,
                property,
                computed: false,
                optional: false,
            } => {
                let MemberProp::Ident(name) = &**property else {
                    return None;
                };
                self.push_value(object)?; // object (once)
                self.chunk.emit(Op::Dup); // keep a copy to store back into
                let nidx = self.chunk.add_name(name);
                self.chunk.emit(Op::GetProp(nidx)); // stack: [obj, obj.name]
                self.push_value(value)?;
                self.chunk.emit(Op::Binary(bop)); // stack: [obj, result]
                self.chunk.emit(Op::SetProp(nidx)); // stack: [result]
                Some(())
            }
            _ => None, // computed member / other targets → fallback
        }
    }

    fn call(&mut self, callee: &Expr, args: &[Expr], optional: bool) -> Option<()> {
        if optional {
            return None;
        }
        let spread = args.iter().any(is_spread);
        match callee {
            // Method call `obj.m(...)` → `this = obj`.
            Expr::Member {
                object,
                property,
                computed: false,
                optional: false,
            } => {
                let MemberProp::Ident(name) = &**property else {
                    return None;
                };
                self.push_value(object)?;
                let idx = self.chunk.add_name(name);
                if spread {
                    self.build_args_array(args)?;
                    self.chunk.emit(Op::CallMethodApply(idx));
                } else {
                    for a in args {
                        self.push_value(a)?;
                    }
                    self.chunk.emit(Op::CallMethod(idx, args.len() as u32));
                }
                Some(())
            }
            // Plain call `f(...)` → `this = undefined`.
            Expr::Member { .. } | Expr::Super => None,
            _ => {
                self.push_value(callee)?;
                if spread {
                    self.build_args_array(args)?;
                    self.chunk.emit(Op::CallApply);
                } else {
                    for a in args {
                        self.push_value(a)?;
                    }
                    self.chunk.emit(Op::Call(args.len() as u32));
                }
                Some(())
            }
        }
    }

    /// Build an array of call arguments on the stack, flattening `...spread`s.
    fn build_args_array(&mut self, args: &[Expr]) -> Option<()> {
        self.chunk.emit(Op::NewArray);
        for a in args {
            match a {
                Expr::Spread(inner) => {
                    self.push_value(inner)?;
                    self.chunk.emit(Op::ArrayAppendSpread);
                }
                e => {
                    self.push_value(e)?;
                    self.chunk.emit(Op::ArrayAppend);
                }
            }
        }
        Some(())
    }
}

fn is_spread(e: &Expr) -> bool {
    matches!(e, Expr::Spread(_))
}

/// Map an arithmetic compound-assignment operator to its binary operator;
/// `None` for `=` and the logical compounds (`&&=`/`||=`/`??=`).
fn compound_binop(op: AssignOp) -> Option<BinOp> {
    Some(match op {
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
        AssignOp::Assign | AssignOp::And | AssignOp::Or | AssignOp::Nullish => return None,
    })
}

/// Whether `stmts` can complete abruptly **through** an enclosing `finally`:
/// a `return`, or a `break`/`continue` that leaves the block (i.e. isn't caught
/// by a nested loop/switch). Such cases need pending-completion semantics the
/// VM doesn't model, so the compiler bails to the eager interpreter.
fn crosses_finally(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| stmt_crosses(s, false))
}

fn stmt_crosses(s: &Stmt, in_loop: bool) -> bool {
    match s {
        Stmt::Return(_) => true,
        Stmt::Break(_) | Stmt::Continue(_) => !in_loop,
        Stmt::Block(b) => b.iter().any(|s| stmt_crosses(s, in_loop)),
        Stmt::If { cons, alt, .. } => {
            stmt_crosses(cons, in_loop) || alt.as_ref().is_some_and(|a| stmt_crosses(a, in_loop))
        }
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::For { body, .. }
        | Stmt::ForIn { body, .. }
        | Stmt::ForOf { body, .. } => stmt_crosses(body, true),
        Stmt::Switch { cases, .. } => cases
            .iter()
            .any(|c| c.body.iter().any(|s| stmt_crosses(s, true))),
        Stmt::Labeled { body, .. } => stmt_crosses(body, in_loop),
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            block.iter().any(|s| stmt_crosses(s, in_loop))
                || handler
                    .as_ref()
                    .is_some_and(|h| h.body.iter().any(|s| stmt_crosses(s, in_loop)))
                || finalizer
                    .as_ref()
                    .is_some_and(|f| f.iter().any(|s| stmt_crosses(s, in_loop)))
        }
        // Expr / VarDecl / FuncDecl / ClassDecl / Throw / Empty / Debugger
        // don't transfer control out of the block.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::parser::parse;
    use super::*;
    use crate::js::ast::FuncBody;

    /// Pull the first function declaration's body statements out of a program.
    fn body_of(src: &str) -> Vec<Stmt> {
        let prog = parse(src).unwrap();
        for s in &prog.body {
            if let Stmt::FuncDecl(f) = s {
                if let FuncBody::Block(b) = &f.body {
                    return b.clone();
                }
            }
        }
        panic!("no function body in: {src}");
    }

    fn has_op(c: &Chunk, pred: impl Fn(&Op) -> bool) -> bool {
        c.code.iter().any(pred)
    }

    #[test]
    fn compiles_an_infinite_generator() {
        let body = body_of("function* g(){ let i = 0; while (true) { yield i; i = i + 1; } }");
        let c = compile_body(&body).expect("infinite generator body compiles");
        assert!(has_op(&c, |o| matches!(o, Op::Yield)), "emits Yield");
        assert!(has_op(&c, |o| matches!(o, Op::JumpIfFalse(_))), "while test branch");
        assert!(has_op(&c, |o| matches!(o, Op::EvalExpr(_))), "pure parts delegated");
        assert!(has_op(&c, |o| matches!(o, Op::DeclareLet(_))), "let i declared");
    }

    #[test]
    fn compiles_for_of_with_yield() {
        let body = body_of("function* g(xs){ for (const x of xs) { yield x * 2; } }");
        let c = compile_body(&body).expect("for-of body compiles");
        assert!(has_op(&c, |o| matches!(o, Op::GetIterator)), "iterator obtained");
        assert!(has_op(&c, |o| matches!(o, Op::IterNext(_))), "step-wise iteration");
        assert!(has_op(&c, |o| matches!(o, Op::Yield)), "yields inside the loop");
    }

    #[test]
    fn compiles_await_decomposition() {
        let body = body_of("async function f(p){ const v = await p; return v + 1; }");
        let c = compile_body(&body).expect("async body compiles");
        assert!(has_op(&c, |o| matches!(o, Op::Await)), "emits Await");
        assert!(has_op(&c, |o| matches!(o, Op::DeclareConst(_))), "const v");
        assert!(has_op(&c, |o| matches!(o, Op::Return)), "explicit return");
    }

    #[test]
    fn destructuring_decl_compiles_via_bind_pattern() {
        // Destructuring declarations are delegated to the interpreter through
        // `BindPattern` (no longer an eager-fallback case).
        let body = body_of("function* g(o){ const { a } = o; yield a; }");
        let c = compile_body(&body).expect("destructuring decl compiles");
        assert!(has_op(&c, |o| matches!(o, Op::BindPattern(_))), "uses BindPattern");
    }

    #[test]
    fn finally_with_return_bails_to_eager() {
        // A `return` crossing a `finally` still falls back to the eager model.
        let body = body_of("function* g(){ try { return; } finally { yield 1; } }");
        assert!(compile_body(&body).is_none(), "return through finally → None");
    }
}
