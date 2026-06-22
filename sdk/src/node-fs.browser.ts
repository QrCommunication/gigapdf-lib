/**
 * Browser stub for the Node-only filesystem loaders.
 *
 * This file is substituted for `node-fs.ts` in browser bundles via the
 * package.json `"browser"` field. It contains NO reference to `node:fs`/
 * `node:url`, so browser bundlers (Turbopack, webpack, Vite) have nothing to
 * resolve. The loader throws — in the browser you must pass bytes or a URL to
 * `GigaPdfEngine.load()` instead.
 */

const NODE_ONLY =
  "gigapdf-lib: this loader is Node-only — in the browser, pass bytes or a URL to load() instead";

/** Browser stub — throws; pass bytes/URL to `GigaPdfEngine.load()` instead. */
export async function loadDefaultWasmBytes(): Promise<Uint8Array> {
  throw new Error(NODE_ONLY);
}
