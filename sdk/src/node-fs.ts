/**
 * Node-only filesystem loaders for gigapdf-lib.
 *
 * This module is the ONLY place that touches Node's `fs`/`url` modules. It is
 * swapped out for `node-fs.browser.ts` via the package.json `"browser"` field
 * so that browser bundlers (Turbopack, webpack, Vite) never see a reference to
 * `node:fs/promises` / `node:url` and never try to resolve them.
 *
 * The relative paths below are resolved from the built module (`dist/node-fs.js`)
 * to the package root via the `import.meta.url` shim injected by tsup (`shims`),
 * exactly like the previous in-`index.ts` resolution did from `dist/index.js`.
 */

/**
 * Indirect the specifier through a variable so any bundler that still evaluates
 * this module (it shouldn't in the browser, thanks to the `"browser"` mapping)
 * doesn't statically resolve these Node-only modules.
 */
const nodeImport = (m: string): Promise<Record<string, unknown>> =>
  import(/* webpackIgnore: true */ /* @vite-ignore */ m);

async function fsReadFile(): Promise<(p: string) => Promise<Uint8Array>> {
  const { readFile } = (await nodeImport("node:fs/promises")) as {
    readFile: (p: string) => Promise<Uint8Array>;
  };
  return readFile;
}

async function urlToPath(): Promise<(u: URL) => string> {
  const { fileURLToPath } = (await nodeImport("node:url")) as {
    fileURLToPath: (u: URL) => string;
  };
  return fileURLToPath;
}

/** Read the bundled `gigapdf.wasm` shipped inside this package. */
export async function loadDefaultWasmBytes(): Promise<Uint8Array> {
  const [readFile, fileURLToPath] = await Promise.all([fsReadFile(), urlToPath()]);
  const wasmPath = fileURLToPath(new URL("../gigapdf.wasm", import.meta.url));
  return readFile(wasmPath);
}

// OCR models are no longer bundled in this package — OCR runs host-side via the
// `gigapdf-ocr-rten` crate (PaddleOCR + RTen). The `.gpocr` readers were removed.
