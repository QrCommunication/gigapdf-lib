//! The JavaScript engine for the [`crate::html`] renderer — so the product
//! needs **no headless browser** (Chromium/Playwright) even for script-driven
//! pages.
//!
//! Both entry points run on the embedded **Boa** engine (pure Rust, compiles to
//! `wasm32`; see `THIRD-PARTY-LICENSES.md`):
//! - [`run_inline_scripts`] ([`boa`]) — execute a document's inline `<script>`s
//!   against a live DOM and re-serialise the mutated tree to HTML. This is the
//!   path the renderer calls before layout.
//! - [`eval`] — evaluate a standalone snippet and return its value as a string.
//!
//! A hand-written interpreter previously backed this module; it was retired in
//! favour of Boa (multi-year JS spec maintenance is a liability better delegated
//! to an audited engine) and removed.

pub mod boa;

pub use boa::run_inline_scripts;

/// Evaluate a standalone JavaScript snippet with the embedded **Boa** engine,
/// returning the result value as a string (`Uncaught …` / `SyntaxError: …` on
/// failure). The renderer's inline-`<script>` path is [`run_inline_scripts`].
///
/// This `eval` is the JS engine's own evaluation entry point — it runs a script
/// inside a sandboxed Boa `Context` with no host, network or filesystem access
/// (only the DOM bindings the renderer provides). It is not a host-code eval.
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
