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

async function fsReaddir(): Promise<(p: string) => Promise<string[]>> {
  const { readdir } = (await nodeImport("node:fs/promises")) as {
    readdir: (p: string) => Promise<string[]>;
  };
  return readdir;
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

/** Read a single bundled `.gpocr` model by its filename (under `models/`). */
export async function readModelFile(fileName: string): Promise<Uint8Array> {
  const [readFile, fileURLToPath] = await Promise.all([fsReadFile(), urlToPath()]);
  const path = fileURLToPath(new URL(`../models/${fileName}`, import.meta.url));
  return readFile(path);
}

/**
 * Discover every bundled `.gpocr` model and return its name + bytes.
 * Returns an empty list if the `models/` directory is absent (slimmed install).
 */
export async function readModelsDir(): Promise<{ name: string; bytes: Uint8Array }[]> {
  const [readFile, readdir, fileURLToPath] = await Promise.all([
    fsReadFile(),
    fsReaddir(),
    urlToPath(),
  ]);
  let entries: string[];
  try {
    entries = await readdir(fileURLToPath(new URL("../models/", import.meta.url)));
  } catch {
    return []; // models/ absent (slimmed install) → caller falls back
  }
  const out: { name: string; bytes: Uint8Array }[] = [];
  for (const name of entries) {
    if (!name.endsWith(".gpocr")) continue;
    const bytes = await readFile(
      fileURLToPath(new URL(`../models/${name}`, import.meta.url)),
    );
    out.push({ name, bytes });
  }
  return out;
}
