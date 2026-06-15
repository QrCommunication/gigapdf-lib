//! The suspendable execution state for the bytecode VM.
//!
//! A [`Frame`] is a *resumable* activation of a compiled [`Chunk`]: an
//! instruction pointer, an operand stack and the current lexical scope. Because
//! all of that lives on the heap, the VM can stop at a `yield`/`await`
//! ([`Step::Yield`] / [`Step::Await`]), hand control back, and later resume
//! exactly where it left off — which a tree-walker cannot do.
//!
//! The run loop itself lives on [`Interp`](super::interp::Interp)
//! (`run_frame`) so it can reuse the interpreter's private value operations;
//! this module only owns the data.

use std::rc::Rc;

use super::bytecode::Chunk;
use super::value::{Env, Value};

/// An installed handler: where its `catch`/`finally` block begins, and the
/// stack/scope state to restore when unwinding to it.
#[derive(Debug)]
struct Handler {
    catch_ip: u32,
    stack_len: usize,
    env: Env,
    /// `true` for a `finally` (unwinds with a *pending* throw, binds nothing);
    /// `false` for a `catch` (binds the thrown value).
    is_finally: bool,
}

/// A paused-or-running activation of a compiled function body.
#[derive(Debug)]
pub struct Frame {
    /// The compiled body.
    pub chunk: Rc<Chunk>,
    /// The next instruction to execute.
    pub ip: usize,
    /// The operand stack.
    pub stack: Vec<Value>,
    /// The current lexical scope (mutated by `PushScope`/`PopScope`).
    pub env: Env,
    /// `false` until the frame has run once; gates the `next(v)` resume value
    /// (the first `next` of a generator has no suspended `yield` to feed).
    pub started: bool,
    /// The stack of active `try` handlers (innermost last).
    handlers: Vec<Handler>,
    /// A throw waiting to be re-raised once the current `finally` block ends.
    pending_throw: Option<Value>,
}

impl Frame {
    /// A fresh frame for `chunk`, executing in activation scope `env`.
    pub fn new(chunk: Rc<Chunk>, env: Env) -> Self {
        Frame {
            chunk,
            ip: 0,
            stack: Vec::new(),
            env,
            started: false,
            handlers: Vec::new(),
            pending_throw: None,
        }
    }

    /// Install a `catch` handler whose block begins at `catch_ip`.
    pub fn push_handler(&mut self, catch_ip: u32) {
        self.install_handler(catch_ip, false);
    }

    /// Install a `finally` handler whose block begins at `finally_ip`.
    pub fn push_finally(&mut self, finally_ip: u32) {
        self.install_handler(finally_ip, true);
    }

    fn install_handler(&mut self, catch_ip: u32, is_finally: bool) {
        self.handlers.push(Handler {
            catch_ip,
            stack_len: self.stack.len(),
            env: self.env.clone(),
            is_finally,
        });
    }

    /// Discard the innermost handler (normal exit from a `try`/`finally`).
    pub fn pop_handler(&mut self) {
        self.handlers.pop();
    }

    /// Record a throw to re-raise when the current `finally` ends.
    pub fn set_pending_throw(&mut self, value: Value) {
        self.pending_throw = Some(value);
    }

    /// Take any pending throw left for `EndFinally`.
    pub fn take_pending_throw(&mut self) -> Option<Value> {
        self.pending_throw.take()
    }

    /// Route a thrown `value` to the innermost handler: unwind the operand
    /// stack and scope, then jump to its block. A `catch` is given the value on
    /// the stack; a `finally` records it as *pending* (re-raised by
    /// `EndFinally`). `Err(value)` if no handler is installed — the throw
    /// escapes the frame.
    pub fn take_handler(&mut self, value: Value) -> Result<(), Value> {
        match self.handlers.pop() {
            Some(h) => {
                self.stack.truncate(h.stack_len);
                self.env = h.env;
                if h.is_finally {
                    self.pending_throw = Some(value);
                } else {
                    self.stack.push(value);
                }
                self.ip = h.catch_ip as usize;
                Ok(())
            }
            None => Err(value),
        }
    }

    /// Pop the top `n` operands, returned in push order (oldest first).
    pub fn pop_n(&mut self, n: u32) -> Vec<Value> {
        let at = self.stack.len().saturating_sub(n as usize);
        self.stack.split_off(at)
    }

    /// Pop one operand (or `undefined` if the stack is unexpectedly empty).
    pub fn pop(&mut self) -> Value {
        self.stack.pop().unwrap_or(Value::Undefined)
    }

    /// Peek the top operand without removing it.
    pub fn peek(&self) -> Value {
        self.stack.last().cloned().unwrap_or(Value::Undefined)
    }
}

/// The result of running a [`Frame`] until it suspends or finishes.
#[derive(Debug)]
pub enum Step {
    /// `yield value` — the frame is paused; resume by pushing the sent value.
    Yield(Value),
    /// `await value` — the frame is paused awaiting a promise/value.
    Await(Value),
    /// The function returned `value`.
    Done(Value),
    /// The function threw `value`.
    Throw(Value),
}
