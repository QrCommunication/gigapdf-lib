# Install & build

The engine has **no third-party crates** and **no native libraries**. You need
only a Rust toolchain; the WebAssembly target is the standard `wasm32` (no
`wasm-pack`, no `wasm-bindgen`).

## Prerequisites

- Rust (stable) — the repo pins it via `rust-toolchain.toml`.
- The wasm target (added once):

  ```bash
  rustup target add wasm32-unknown-unknown
  ```

  You only ever type that triple here; builds use the `cargo wasm` alias below.

- (Optional) Node.js — to run the end-to-end smoke test.

## Build the native core

```bash
cargo build -p gigapdf-core --release
cargo test  -p gigapdf-core            # runs against real fixtures in fixtures/
```

Use this for server-side conversion/rendering directly from Rust.

## Build the WebAssembly module

```bash
cargo wasm   # alias for the release target build (see .cargo/config.toml)
# → target/wasm32-unknown-unknown/release/gigapdf_wasm.wasm  (~540 KB)
```

The `.wasm` is self-contained: instantiate it with an **empty** import object.
No JS glue is generated — you call the `gp_*` exports directly over the linear
memory ABI described in [USAGE.md](USAGE.md).

```js
const { instance } = await WebAssembly.instantiate(wasmBytes, {});
```

### Optional size trimming

The module is already small. If you want it smaller, run `wasm-opt -Oz`
(from binaryen) as a post-step — it is *not* required and not a dependency:

```bash
wasm-opt -Oz gigapdf_wasm.wasm -o gigapdf_wasm.min.wasm
```

## Run the smoke test

```bash
node test/wasm-smoke.mjs
```

It opens real fixtures and exercises every feature end-to-end (edit, render,
encrypt, sign, all forward + reverse conversions, font embedding). The font test
reads a system TTF (`/usr/share/fonts/.../LiberationSans-Regular.ttf`); adjust the
path if your distro differs.

## Integrate into a host app

1. Ship `gigapdf_wasm.wasm` as a static asset.
2. Load it once (cache the `instance`).
3. Wrap the buffer ABI with the helpers in [USAGE.md](USAGE.md).
4. Provide the two host capabilities the sandbox lacks:
   - **Randomness** for `gp_sign` / `gp_save_encrypted` — pass
     `crypto.getRandomValues` bytes.
   - **Network** for Google Fonts — `fetch` the URL the engine computes, then
     hand the TTF bytes to `gp_embed_font`.

## Regenerating bundled data

Two data files are generated from upstream sources (snapshots are committed under
`tools/`):

```bash
# Font catalog (from Google Fonts metadata)
python3 tools/gen_catalog.py            # → crates/core/src/font/catalog.rs

# sRGB ICC profile for PDF/A is embedded from tools/sRGB.icc
#   → crates/core/src/convert/srgb_icc.rs
```

## Project layout

```
crates/core/src/
  document.rs        the Document façade (open/edit/convert/save)
  object.rs lexer.rs parser.rs serialize.rs    object model + (de)serialization
  filters/           inflate + deflate (zlib)
  content/           content-stream interpreter, elements, text runs
  font/              WinAnsi, CMap/ToUnicode, TrueType, CFF, catalog, google, embed
  crypto/            md5 rc4 aes sha256 sha512 bignum rsa
  security/          Standard Security Handler (encrypt/decrypt)
  sign/              ASN.1 DER, X.509, CMS/PKCS#7
  raster/            canvas, png, render
  convert/           zip, office, table, style, web, build, reverse, pdfa, srgb_icc
crates/wasm/src/lib.rs   the extern "C" gp_* ABI
```
