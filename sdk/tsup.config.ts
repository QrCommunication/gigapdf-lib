import { defineConfig } from "tsup";

export default defineConfig({
  entry: ["src/index.ts", "src/viewer.ts", "src/editor.ts"],
  format: ["cjs", "esm"],
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
