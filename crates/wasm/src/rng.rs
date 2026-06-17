//! WebAssembly entropy backend.
//!
//! `wasm32-unknown-unknown` has no OS RNG, yet RustCrypto (RSA signature
//! blinding) and Boa (`Math.random`) both draw from `getrandom`. The engine is
//! deliberately `wasm-bindgen`-free — it is instantiated with a raw import
//! object — so rather than the `wasm_js` backend (which pulls `wasm-bindgen`
//! glue), both getrandom versions are routed to a single host import,
//! `env.gp_host_random(ptr, len)`, which the SDK fills from
//! `crypto.getRandomValues`. This keeps the architecture's invariant intact:
//! the wasm module never produces entropy itself; the host injects it.
//!
//! Two getrandom majors are in the tree, with distinct custom-backend hooks:
//! - **0.3** (Boa, via `rand` 0.9) — selected by the `getrandom_backend="custom"`
//!   cfg in `.cargo/config.toml`; provides the `__getrandom_v03_custom` symbol.
//! - **0.2** (RSA, via `rand_core` 0.6) — selected by the crate's `custom`
//!   feature; registered with `register_custom_getrandom!`.

#[link(wasm_import_module = "env")]
extern "C" {
    /// Host entropy: write `len` cryptographically-random bytes at `dest`.
    /// Supplied by the SDK's wasm import object as `env.gp_host_random`.
    fn gp_host_random(dest: *mut u8, len: usize);
}

/// getrandom 0.3 custom backend (Boa).
///
/// # Safety
/// `dest` must be valid for writes of `len` bytes; upheld by getrandom's caller.
#[no_mangle]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    gp_host_random(dest, len);
    Ok(())
}

/// getrandom 0.2 custom backend (RSA).
fn host_entropy_v02(buf: &mut [u8]) -> Result<(), getrandom_02::Error> {
    // SAFETY: `buf` is a valid, uniquely-borrowed slice of exactly `buf.len()`
    // bytes; the host writes that many and nothing more.
    unsafe { gp_host_random(buf.as_mut_ptr(), buf.len()) };
    Ok(())
}
getrandom_02::register_custom_getrandom!(host_entropy_v02);
