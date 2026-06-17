//! A zero-dependency JavaScript engine, built to run the `<script>` of an HTML
//! document for the [`crate::html`] renderer — so the product needs **no
//! headless browser** (Chromium/Playwright) even for script-driven pages.
//!
//! Staged, layer-by-layer build (each layer compilable and tested):
//! 1. [`token`] / [`lexer`] — the scanner: source → token stream. **(this layer)**
//! 2. `parser` / `ast` — tokens → an abstract syntax tree.
//! 3. `interp` — a tree-walking evaluator with scopes, closures and prototypes.
//! 4. `builtins` — `Object`/`Array`/`String`/`Math`/`JSON`/`console`, …
//! 5. `dom` — bindings over [`crate::html::dom`] (`document.getElementById`, …),
//!    wired into the renderer to execute scripts before layout.
//!
//! Scope note: the initial target is ES5-core semantics plus widely-used ES2015+
//! syntax (arrow functions, template literals, `let`/`const`, classes). No
//! event loop, timers, async/`await` or `Promise` in the first stages.

pub mod ast;
pub mod boa;
pub mod builtins;
pub mod bytecode;
pub mod compile;
pub mod dom;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod regex;
pub mod token;
pub mod value;
pub mod vm;

pub use ast::Program;
// The HTML renderer's inline-`<script>` path now runs on Boa (see [`boa`]). The
// hand-written `dom`/`interp` engine is retained for now but no longer wired in.
pub use boa::run_inline_scripts;
pub use interp::{Abrupt, Eval, Interp};
pub use lexer::{tokenize, LexError, Lexer};
pub use parser::{parse, ParseError};
pub use token::{Punct, Tok, Token};
pub use value::Value;

/// Evaluate a standalone JavaScript snippet with the embedded **Boa** engine,
/// returning the result value as a string (`Uncaught …` / `SyntaxError: …` on
/// failure). The renderer's inline-`<script>` path is [`run_inline_scripts`].
///
/// This `eval` is the JS engine's own evaluation entry point — it runs a script
/// inside a sandboxed Boa `Context` with no host, network or filesystem access
/// (only the DOM bindings the renderer provides). It is not a host-code eval.
/// The hand-written interpreter behind `run_inline_scripts` is being retired in
/// favour of Boa; this standalone entry is the first consumer moved over.
pub fn eval(src: &str) -> String {
    use boa_engine::{Context, Source};
    let mut ctx = Context::default();
    match ctx.eval(Source::from_bytes(src)) {
        Ok(v) => v
            .to_string(&mut ctx)
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default(),
        Err(e) => {
            let s = e.to_string();
            if s.starts_with("SyntaxError") {
                s
            } else {
                format!("Uncaught {s}")
            }
        }
    }
}

#[cfg(test)]
mod eval_tests {
    #[test]
    fn boa_eval_basic() {
        assert_eq!(super::eval("40 + 2"), "42");
        assert_eq!(super::eval("[1,2,3].map(x => x * 2).join(',')"), "2,4,6");
        assert_eq!(super::eval("'abc'.toUpperCase()"), "ABC");
    }
}
