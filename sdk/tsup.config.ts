import { defineConfig } from "tsup";

export default defineConfig({
  entry: [
    "src/index.ts",
    "src/viewer.ts",
    "src/editor.ts",
    // Node-only fs loaders + their throwing browser stub. Built as separate
    // entries so the package.json `"browser"` field can map one to the other.
    "src/node-fs.ts",
    "src/node-fs.browser.ts",
  ],
  format: ["cjs", "esm"],
  // Keep the node-fs module as a real external import in the bundle (instead of
  // inlining it into index.js). This is what lets the package.json `"browser"`
  // field remap it to the throwing stub, so browser bundlers never see `node:*`.
  // The source specifier is `./node-fs.js`; esbuild keeps it verbatim for both
  // outputs (it does not rewrite external specifiers). Node resolves it to the
  // ESM `node-fs.js` in either format (Node 22+ supports require-of-ESM), and
  // browser bundlers remap `./dist/node-fs.js` to the stub via the `"browser"`
  // field. The `"browser"` map also lists `./dist/node-fs.cjs` defensively.
  external: ["./node-fs.js"],
  dts: true,
  clean: true,
  splitting: false,
  sourcemap: true,
  // Inject cross-format `import.meta.url` / `__dirname` shims so
  // `GigaPdfEngine.loadDefault()` resolves the bundled .wasm in BOTH the ESM
  // and CJS outputs (without this, import.meta is empty under cjs).
  shims: true,
  // The engine `.wasm` ships as a static asset (`gigapdf.wasm`, see
  // package.json "files"/exports). `GigaPdfEngine.loadDefault()` reads it from
  // the package dir in Node; browsers pass bytes/URL to `GigaPdfEngine.load()`.
});
