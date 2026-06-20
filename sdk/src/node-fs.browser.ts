/**
 * Browser stub for the Node-only filesystem loaders.
 *
 * This file is substituted for `node-fs.ts` in browser bundles via the
 * package.json `"browser"` field. It contains NO reference to `node:fs`/
 * `node:url`, so browser bundlers (Turbopack, webpack, Vite) have nothing to
 * resolve. Each loader throws — in the browser you must pass bytes or a URL to
 * `GigaPdfEngine.load()` / `loadOcrModel()` instead.
 */

const NODE_ONLY =
  "gigapdf-lib: this loader is Node-only — in the browser, pass bytes or a URL to load()/loadOcrModel() instead";

/** Browser stub — throws; pass bytes/URL to `GigaPdfEngine.load()` instead. */
export async function loadDefaultWasmBytes(): Promise<Uint8Array> {
  throw new Error(NODE_ONLY);
}

/** Browser stub — throws; pass model bytes to `loadOcrModel()` instead. */
export async function readModelFile(_fileName: string): Promise<Uint8Array> {
  throw new Error(NODE_ONLY);
}

/** Browser stub — throws; load OCR models from bytes instead. */
export async function readModelsDir(): Promise<{ name: string; bytes: Uint8Array }[]> {
  throw new Error(NODE_ONLY);
}
