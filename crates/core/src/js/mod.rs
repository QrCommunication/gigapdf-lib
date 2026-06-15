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
pub use dom::run_inline_scripts;
pub use interp::{Abrupt, Eval, Interp};
pub use lexer::{tokenize, LexError, Lexer};
pub use parser::{parse, ParseError};
pub use token::{Punct, Tok, Token};
pub use value::Value;
