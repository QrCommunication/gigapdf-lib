/**
 * @qrcommunication/gigapdf-lib — TypeScript SDK for the gigapdf-lib Rust→WASM PDF
 * engine: no third-party PDF/Office/image library (cryptography uses RustCrypto,
 * JavaScript uses Boa). Wraps the flat `extern "C"` `gp_*` ABI behind a typed,
 * ergonomic class. No third-party npm deps; the `.wasm` imports a single host
 * function — `env.gp_host_random` (entropy), which `load()` supplies.
 *
 * Usage:
 *   const giga = await GigaPdfEngine.load(wasmBytesOrUrl);
 *   const doc = giga.open(pdfBytes);
 *   const docx = doc.toDocx();
 *   const png = doc.renderPage(1, 2);
 *   doc.close();
 */

// Node-only filesystem loaders. Swapped for a throwing browser stub via the
// package.json `"browser"` field, so browser bundlers never see `node:fs`/
// `node:url`. The specifier is a plain relative path (well-supported by
// Turbopack/webpack/Vite), unlike the previous inline `import("node:...")`.
import { loadDefaultWasmBytes } from "./node-fs.js";

// FFI boundary: the wasm exports are an untyped table of `gp_*` functions
// (numbers in, numbers out) plus `memory`. `any` here is the documented FFI
// exception — every public method below re-imposes precise types.
export type Exports = {
  memory: WebAssembly.Memory;
  gp_alloc(len: number): number;
  gp_free(ptr: number, len: number): void;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  [k: string]: any;
};

const enc = new TextEncoder();
const dec = new TextDecoder();

/**
 * The eight PDF access permissions carried by an `/Encrypt` dictionary's `/P`
 * entry (ISO 32000-1 §7.6.3.2, Table 22). Each flag is `true` when the action
 * is **granted** to a user opening the document with the user password; the
 * owner password always lifts every restriction.
 */
export type PdfPermissions = {
  /** Print the document (low resolution unless {@link printHighRes} is also set). */
  print: boolean;
  /** Modify the document contents (other than annotating, filling or assembling). */
  modify: boolean;
  /** Copy or otherwise extract text and graphics. */
  copy: boolean;
  /** Add or modify annotations and fill in interactive form fields. */
  annotate: boolean;
  /** Fill in existing interactive form fields (even if {@link annotate} is clear). */
  fillForms: boolean;
  /** Extract text and graphics for accessibility tools. */
  accessibility: boolean;
  /** Assemble the document: insert, rotate and delete pages. */
  assemble: boolean;
  /** Print to a high-resolution device (requires {@link print}). */
  printHighRes: boolean;
};

/** Every permission granted — the unrestricted baseline (`/P` = -196). */
const ALL_PERMISSIONS: PdfPermissions = {
  print: true,
  modify: true,
  copy: true,
  annotate: true,
  fillForms: true,
  accessibility: true,
  assemble: true,
  printHighRes: true,
};

// OCR moved host-side to the `gigapdf-ocr-rten` crate (PaddleOCR PP-OCR via RTen, 12 languages +
// auto script selection + Hebrew). The WASM SDK no longer ships a client-side recognizer; hosts
// call the OCR service/binary directly. The legacy `.gpocr` model-loading API was removed.

/**
 * One source in a {@link GigaPdfEngine.mergePdfs} call. `pdf` is the source
 * bytes; the optional `pages` selects 1-based page numbers (in the given order)
 * to bring in — omit it to append every page.
 */
export type MergePart = { pdf: Uint8Array; pages?: number[] };

/** Loaded engine module. Create documents with {@link open} / {@link openEncrypted}. */
export class GigaPdfEngine {
  private constructor(private readonly ex: Exports) {}

  /** Lazily-built Base64 reverse-lookup table (see {@link _fromBase64}). */
  private static _b64rev?: Int16Array;

  /** Instantiate from raw wasm bytes, a URL/path, or a Response. */
  static async load(source: ArrayBuffer | Uint8Array | string | Response): Promise<GigaPdfEngine> {
    let bytes: ArrayBuffer | Uint8Array;
    if (typeof source === "string") {
      const res = await fetch(source);
      bytes = await res.arrayBuffer();
    } else if (source instanceof Response) {
      bytes = await source.arrayBuffer();
    } else {
      bytes = source;
    }
    // The wasm imports one host function: `env.gp_host_random`. wasm32 has no OS
    // RNG, so the engine (RSA signature blinding, Boa `Math.random`) draws
    // entropy from the host's Web Crypto. This keeps the module wasm-bindgen-free.
    let ex: Exports | undefined;
    const { instance } = await WebAssembly.instantiate(
      bytes instanceof Uint8Array ? bytes.slice().buffer : bytes,
      {
        env: {
          gp_host_random: (ptr: number, len: number): void => {
            const c = (globalThis as { crypto?: Crypto }).crypto;
            if (!c?.getRandomValues) {
              throw new Error(
                "gigapdf: wasm entropy needs Web Crypto (globalThis.crypto.getRandomValues)"
              );
            }
            const mem = new Uint8Array(ex!.memory.buffer, ptr, len);
            // getRandomValues rejects views longer than 65536 bytes; fill in chunks.
            for (let off = 0; off < len; off += 65536) {
              c.getRandomValues(mem.subarray(off, Math.min(off + 65536, len)));
            }
          },
        },
      }
    );
    ex = instance.exports as Exports;
    return new GigaPdfEngine(ex);
  }

  /**
   * Convenience loader for Node: instantiate from the `gigapdf.wasm` shipped
   * inside this package (resolved relative to the built module). In the browser,
   * pass a URL or bytes to {@link load} instead.
   *
   * In Next.js `output: "standalone"`, add the asset to `outputFileTracingIncludes`
   * for the consuming route so the `.wasm` is copied into the standalone bundle.
   */
  static async loadDefault(): Promise<GigaPdfEngine> {
    // The fs access lives in `./node-fs` (Node-only; browser-stubbed via the
    // package.json `"browser"` map) so browser bundlers never resolve `node:*`.
    return GigaPdfEngine.load(await loadDefaultWasmBytes());
  }

  // ── linear-memory ABI helpers (internal) ─────────────────────────────────
  private u8() {
    return new Uint8Array(this.ex.memory.buffer);
  }
  private dv() {
    return new DataView(this.ex.memory.buffer);
  }
  /** Copy host bytes into wasm memory; returns the pointer (caller frees). */
  _toWasm(bytes: Uint8Array): number {
    // If `bytes` aliases our own wasm memory (e.g. a buffer the caller didn't
    // copy out), `gp_alloc` may grow — and detach — that buffer mid-copy.
    // Snapshot to a host array first so the `.set` source stays valid.
    const src = bytes.buffer === this.ex.memory.buffer ? new Uint8Array(bytes) : bytes;
    const ptr = this.ex.gp_alloc(src.length);
    this.u8().set(src, ptr);
    return ptr;
  }
  _free(ptr: number, len: number) {
    this.ex.gp_free(ptr, len);
  }
  /** Call a buffer-returning export `(…, outLenPtr) -> dataPtr`; copies + frees. */
  _buffer(call: (outLenPtr: number) => number): Uint8Array {
    const lenPtr = this.ex.gp_alloc(4);
    const dataPtr = call(lenPtr);
    if (dataPtr === 0) {
      this.ex.gp_free(lenPtr, 4);
      return new Uint8Array(0);
    }
    const len = this.dv().getUint32(lenPtr, true);
    const out = this.u8().slice(dataPtr, dataPtr + len);
    this.ex.gp_free(dataPtr, len);
    this.ex.gp_free(lenPtr, 4);
    return out;
  }
  _str(call: (outLenPtr: number) => number): string {
    return dec.decode(this._buffer(call));
  }
  _json<T = unknown>(call: (outLenPtr: number) => number): T {
    const s = this._str(call);
    return (s ? JSON.parse(s) : []) as T;
  }
  /**
   * Like {@link _json} but distinguishes a **null** result (the export returned a
   * null pointer — e.g. an unrecognized container) from a present-but-empty JSON
   * payload (`[]` / `{}`). Returns `null` only for the null pointer; otherwise the
   * parsed JSON. Used where the Rust side is `Option<…>` and "nothing" and
   * "unrecognized" must stay distinct.
   */
  _jsonOrNull<T = unknown>(call: (outLenPtr: number) => number): T | null {
    const lenPtr = this.ex.gp_alloc(4);
    const dataPtr = call(lenPtr);
    if (dataPtr === 0) {
      this.ex.gp_free(lenPtr, 4);
      return null;
    }
    const len = this.dv().getUint32(lenPtr, true);
    const s = dec.decode(this.u8().slice(dataPtr, dataPtr + len));
    this.ex.gp_free(dataPtr, len);
    this.ex.gp_free(lenPtr, 4);
    return (s ? JSON.parse(s) : []) as T;
  }
  /**
   * Decode standard Base64 (RFC 4648) to bytes. Pure-JS table decode, so it
   * works identically in Node and the browser with no dependency (used to turn
   * the JSON `dataBase64` of {@link GigaPdfDoc.attachments} back into bytes).
   */
  _fromBase64(s: string): Uint8Array {
    let rev = GigaPdfEngine._b64rev;
    if (!rev) {
      rev = new Int16Array(256).fill(-1);
      const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
      for (let i = 0; i < alphabet.length; i++) rev[alphabet.charCodeAt(i)] = i;
      GigaPdfEngine._b64rev = rev;
    }
    let count = s.length;
    while (count > 0 && s[count - 1] === '=') count--;
    const out = new Uint8Array((count * 3) >> 2);
    let acc = 0;
    let bits = 0;
    let oi = 0;
    for (let i = 0; i < count; i++) {
      const v = rev[s.charCodeAt(i)];
      if (v === undefined || v < 0) continue; // skip stray whitespace defensively
      acc = (acc << 6) | v;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out[oi++] = (acc >> bits) & 0xff;
      }
    }
    return oi === out.length ? out : out.subarray(0, oi);
  }
  /** Pass a string argument; runs `fn(ptr, len)` then frees. */
  _withStr<T>(s: string, fn: (ptr: number, len: number) => T): T {
    const b = enc.encode(s);
    const ptr = this._toWasm(b);
    try {
      return fn(ptr, b.length);
    } finally {
      this._free(ptr, b.length);
    }
  }
  /** Pass an optional string; an absent/empty value runs `fn(0, 0)` (no alloc). */
  _withOptStr<T>(s: string | undefined, fn: (ptr: number, len: number) => T): T {
    return s ? this._withStr(s, fn) : fn(0, 0);
  }
  /** Pass a bytes argument; runs `fn(ptr, len)` then frees. */
  _withBytes<T>(bytes: Uint8Array, fn: (ptr: number, len: number) => T): T {
    const ptr = this._toWasm(bytes);
    try {
      return fn(ptr, bytes.length);
    } finally {
      this._free(ptr, bytes.length);
    }
  }
  /** Pass a `u32[]` argument (e.g. page numbers); runs `fn(ptr, count)` then frees. */
  _withU32<T>(values: number[], fn: (ptr: number, count: number) => T): T {
    const ptr = this.ex.gp_alloc(values.length * 4);
    const dv = this.dv();
    for (const [i, v] of values.entries()) dv.setUint32(ptr + i * 4, v >>> 0, true);
    try {
      return fn(ptr, values.length);
    } finally {
      this._free(ptr, values.length * 4);
    }
  }
  /** Pass a flat `f64[]` argument (e.g. ink x,y pairs); runs `fn(ptr, count)` then frees. */
  _withF64<T>(values: number[], fn: (ptr: number, count: number) => T): T {
    const ptr = this.ex.gp_alloc(values.length * 8);
    const dv = this.dv();
    for (const [i, v] of values.entries()) dv.setFloat64(ptr + i * 8, v, true);
    try {
      return fn(ptr, values.length);
    } finally {
      this._free(ptr, values.length * 8);
    }
  }
  get raw(): Exports {
    return this.ex;
  }

  // ── documents ─────────────────────────────────────────────────────────────
  /** Open a PDF. Throws if it can't be parsed. */
  open(pdf: Uint8Array): GigaPdfDoc {
    const handle = this._withBytes(pdf, (p, l) => this.ex.gp_open(p, l));
    if (handle === 0) throw new Error("gigapdf: failed to open PDF");
    return new GigaPdfDoc(this, handle);
  }
  /** Open an encrypted PDF with a password. Returns null if the password is wrong. */
  openEncrypted(pdf: Uint8Array, password: string): GigaPdfDoc | null {
    const b = enc.encode(password);
    const pwPtr = this._toWasm(b);
    const handle = this._withBytes(pdf, (p, l) => this.ex.gp_open_encrypted(p, l, pwPtr, b.length));
    this._free(pwPtr, b.length);
    return handle === 0 ? null : new GigaPdfDoc(this, handle);
  }

  /**
   * Open a **public-key (certificate) encrypted** PDF (`/Filter /Adobe.PubSec`,
   * ISO 32000-1 §7.6.5) with a recipient's DER `certificate` + PKCS#1 RSA
   * `privateKey`. `null` if this key is not a recipient. Counterpart of
   * {@link GigaPdfDoc.encryptForRecipients}.
   */
  openWithPrivateKey(
    pdf: Uint8Array,
    certificate: Uint8Array,
    privateKey: Uint8Array
  ): GigaPdfDoc | null {
    const cP = this._toWasm(certificate);
    const kP = this._toWasm(privateKey);
    const handle = this._withBytes(pdf, (p, l) =>
      this.ex.gp_open_with_private_key(p, l, cP, certificate.length, kP, privateKey.length)
    );
    this._free(cP, certificate.length);
    this._free(kP, privateKey.length);
    return handle === 0 ? null : new GigaPdfDoc(this, handle);
  }

  /**
   * Inspect a PDF's encryption **without decrypting it** (no password needed):
   * whether it has an `/Encrypt` dictionary, plus its `/P` permission bitmask
   * and handler version/revision (`0` when not encrypted).
   */
  encryptionInfo(pdf: Uint8Array): {
    encrypted: boolean;
    permissions: number;
    version: number;
    revision: number;
  } {
    const json = this._withBytes(pdf, (p, l) =>
      this._buffer((o) => this.ex.gp_encryption_info(p, l, o))
    );
    return JSON.parse(dec.decode(json));
  }

  /**
   * Decode the eight access-permission flags from a `/P` permission bitmask
   * (ISO 32000-1 Table 22) into named booleans (`true` = the action is
   * granted). Reserved bits are ignored.
   */
  decodePermissions(p: number): PdfPermissions {
    const json = dec.decode(this._buffer((o) => this.ex.gp_permissions_from_p(p, o)));
    return JSON.parse(json) as PdfPermissions;
  }

  /**
   * Read a PDF's access permissions **without decrypting it** (no password
   * needed). Returns the eight named flags decoded from the `/Encrypt`
   * dictionary's `/P`; an unencrypted document grants everything.
   */
  getPermissions(pdf: Uint8Array): PdfPermissions {
    const { encrypted, permissions } = this.encryptionInfo(pdf);
    return encrypted ? this.decodePermissions(permissions) : { ...ALL_PERMISSIONS };
  }

  /**
   * Pack eight access-permission flags into a signed 32-bit `/P` value
   * (ISO 32000-1 Table 22). Omitted flags default to **granted**, so an empty
   * object means "all allowed". Feed the result to {@link GigaPdfDoc.saveEncrypted}
   * via `opts.permissions`, or pass `opts.flags` directly.
   */
  permissionsToP(flags: Partial<PdfPermissions> = {}): number {
    const f = { ...ALL_PERMISSIONS, ...flags };
    return this.ex.gp_permissions_to_p(
      f.print ? 1 : 0,
      f.modify ? 1 : 0,
      f.copy ? 1 : 0,
      f.annotate ? 1 : 0,
      f.fillForms ? 1 : 0,
      f.accessibility ? 1 : 0,
      f.assemble ? 1 : 0,
      f.printHighRes ? 1 : 0
    );
  }

  /**
   * Width of `text` set in standard Helvetica at `size` points (AFM metrics) —
   * place watermark/header text without embedding a font.
   */
  helveticaWidth(size: number, text: string): number {
    return this._withStr(text, (p, l) => this.ex.gp_helvetica_width(p, l, size));
  }

  // ── stateless <format> → PDF conversions ───────────────────────────────────
  txtToPdf(text: string): Uint8Array {
    return this._withStr(text, (p, l) => this._buffer((o) => this.ex.gp_txt_to_pdf(p, l, o)));
  }
  htmlToPdf(html: string): Uint8Array {
    return this._withStr(html, (p, l) => this._buffer((o) => this.ex.gp_html_to_pdf(p, l, o)));
  }
  rtfToPdf(rtf: string): Uint8Array {
    return this._withStr(rtf, (p, l) => this._buffer((o) => this.ex.gp_rtf_to_pdf(p, l, o)));
  }
  /** Office (DOCX/ODT/PPTX/XLSX/ODS) → PDF, auto-detected. Empty array if unrecognized. */
  officeToPdf(office: Uint8Array): Uint8Array {
    return this._withBytes(office, (p, l) =>
      this._buffer((o) => this.ex.gp_office_to_pdf(p, l, o))
    );
  }
  /**
   * Phase 1 for {@link officeToPdf} — the Google/system fonts an Office container
   * **references but doesn't embed**. Download each `url` (→ TTF) and supply the
   * bytes back to the host font cache so {@link officeToPdf}'s styled runs lay out
   * with the right metrics. Faces the container embeds itself are de-obfuscated
   * and used automatically (not listed here). Returns `null` if the bytes are not
   * a recognized Office container, `[]` if it needs no host fonts.
   */
  officeNeededFonts(office: Uint8Array): HtmlFontRequest[] | null {
    return this._withBytes(office, (p, l) =>
      this._jsonOrNull<HtmlFontRequest[]>((o) => this.ex.gp_office_needed_fonts(p, l, o))
    );
  }
  /**
   * Phase 2 for {@link officeNeededFonts} — render an Office container to PDF with
   * the host-fetched `fonts` embedded, so families the container **references but
   * doesn't embed** (reported by {@link officeNeededFonts}) lay out with the right
   * metrics (e.g. Carlito for a Calibri reference) instead of drifting onto the
   * bundled fallback. Faces the container embeds itself win on conflict, so an
   * empty `fonts` array yields exactly {@link officeToPdf}'s output. Empty array
   * if the bytes are not a recognized Office container.
   *
   * @example
   * const reqs = giga.officeNeededFonts(docx);            // phase 1: what to fetch
   * const fonts = await Promise.all(                       // host fetches each url → TTF
   *   (reqs ?? []).map(async (r) => ({ ...r, ttf: await fetchTtf(r.url) }))
   * );
   * const pdf = giga.officeToPdfWith(docx, fonts);         // phase 2: render with them
   */
  officeToPdfWith(office: Uint8Array, fonts: HtmlFont[] = []): Uint8Array {
    const blob = packHtmlFonts(fonts);
    return this._withBytes(office, (op, ol) =>
      this._withBytes(blob, (fp, fl) =>
        this._buffer((o) => this.ex.gp_office_to_pdf_with_fonts(op, ol, fp, fl, o))
      )
    );
  }
  /**
   * Image (PNG/JPEG/GIF/WebP/AVIF) → one-page PDF, format auto-detected: the
   * image is centred and scaled to fit on an A4 portrait page. PNG/JPEG embed
   * directly; GIF/WebP/AVIF are transcoded to PNG first — all in pure Rust/WASM,
   * no third-party image library. Empty array if the bytes are not a recognized
   * image. To combine many images into a single document, pipe each result
   * through {@link mergePdfs}.
   */
  imageToPdf(image: Uint8Array): Uint8Array {
    return this._withBytes(image, (p, l) =>
      this._buffer((o) => this.ex.gp_image_to_pdf(p, l, o))
    );
  }
  /**
   * Merge several PDFs into one by appending their pages in order. Each input is
   * either raw bytes (every page) or a {@link MergePart} `{ pdf, pages? }` that
   * selects 1-based page numbers (ISO 32000-1 §7.7.3) — so the two forms can be
   * mixed (e.g. `[whole, { pdf: b, pages: [1, 3] }]`). Returns an empty document
   * for an empty list, or — when the only input is a whole PDF (no `pages`) —
   * that input unchanged. Each subsequent part is appended onto the first via
   * {@link GigaPdfDoc.appendPages}; the merged bytes are returned and the working
   * document is closed.
   */
  mergePdfs(parts: (Uint8Array | MergePart)[]): Uint8Array {
    if (parts.length === 0) return new Uint8Array(0);
    const part = (p: Uint8Array | MergePart): MergePart =>
      p instanceof Uint8Array ? { pdf: p } : p;
    const first = part(parts[0]!);
    if (parts.length === 1 && first.pages === undefined) return first.pdf;
    // Seed the working document from the first part — reducing it to just its
    // selected pages first, so a page-range on the first source is honoured too.
    let seed = first.pdf;
    if (first.pages !== undefined) {
      const head = this.open(first.pdf);
      try {
        seed = head.extractPages(first.pages);
      } finally {
        head.close();
      }
    }
    const base = this.open(seed);
    try {
      for (let i = 1; i < parts.length; i++) {
        const p = part(parts[i]!);
        base.appendPages(p.pdf, p.pages);
      }
      return base.save();
    } finally {
      base.close();
    }
  }
  /**
   * Write a host-built grid (`pages[rows][cells]`) to an `.xlsx` workbook — one
   * sheet per page — with the engine's native spreadsheet writer (no
   * third-party library). Supply your own table reconstruction and still emit
   * Office output. `sheetNames` (index-aligned to `grids`) overrides the default
   * `Page <n>` titles; a missing/empty name falls back to the default.
   * {@link gridsToOds} is the OpenDocument (`.ods`) counterpart.
   */
  gridsToXlsx(grids: string[][][], sheetNames: string[] = []): Uint8Array {
    return this._withStr(JSON.stringify(grids), (gp, gl) =>
      this._withStr(JSON.stringify(sheetNames), (np, nl) =>
        this._buffer((o) => this.ex.gp_grids_to_xlsx(gp, gl, np, nl, o))
      )
    );
  }
  /** Write a host-built grid (`pages[rows][cells]`) to an `.ods` spreadsheet
   * (optional `sheetNames`, default `Page <n>`). */
  gridsToOds(grids: string[][][], sheetNames: string[] = []): Uint8Array {
    return this._withStr(JSON.stringify(grids), (gp, gl) =>
      this._withStr(JSON.stringify(sheetNames), (np, nl) =>
        this._buffer((o) => this.ex.gp_grids_to_ods(gp, gl, np, nl, o))
      )
    );
  }
  /**
   * Read an `.xlsx` workbook back into per-sheet `{ name, rows }` grids — the
   * inverse of {@link gridsToXlsx} / {@link GigaPdfDoc.toXlsx}. Cell text is
   * decoded from inline strings, shared strings (`sharedStrings.xml`) or plain
   * values; sheets come in workbook order. `[]` for a non-xlsx input.
   */
  xlsxToGrids(xlsx: Uint8Array): XlsxSheet[] {
    return this._withBytes(xlsx, (p, l) =>
      this._json((o) => this.ex.gp_xlsx_to_grids(p, l, o))
    );
  }
  /**
   * Encode raw **RGBA** pixels (`width*height*4` bytes, row-major,
   * non-premultiplied) to a PNG with the engine's native encoder — no
   * third-party image library. Returns an empty array if the buffer length
   * doesn't match `width*height*4`.
   */
  rgbaToPng(rgba: Uint8Array, width: number, height: number): Uint8Array {
    return this._withBytes(rgba, (p, l) =>
      this._buffer((o) => this.ex.gp_rgba_to_png(width, height, p, l, o))
    );
  }
  /**
   * Resample raw **RGBA** pixels (`sw`×`sh`) to `dw`×`dh` with the engine's
   * native alpha-correct resampler (triangle kernel, footprint scaled for
   * down/up) — no third-party image library. Returns the resized RGBA
   * (`dw*dh*4`), or an empty array on a bad input.
   */
  resizeRgba(rgba: Uint8Array, sw: number, sh: number, dw: number, dh: number): Uint8Array {
    return this._withBytes(rgba, (p, l) =>
      this._buffer((o) => this.ex.gp_resize_rgba(p, l, sw, sh, dw, dh, o))
    );
  }
  /**
   * Encode raw **RGBA** pixels to a baseline JPEG at `quality` (1–100) with the
   * engine's native encoder — no third-party image library. Alpha is composited
   * onto white. Empty array on a bad input.
   */
  encodeJpeg(rgba: Uint8Array, width: number, height: number, quality = 82): Uint8Array {
    return this._withBytes(rgba, (p, l) =>
      this._buffer((o) => this.ex.gp_encode_jpeg(width, height, p, l, quality, o))
    );
  }
  /** Decode a baseline JPEG to `{ width, height, rgba }`, or `null` on failure. */
  decodeJpeg(jpeg: Uint8Array): DecodedImage | null {
    return this._decodeFramed(jpeg, (p, l, o) => this.ex.gp_decode_jpeg(p, l, o));
  }
  /** Decode a PNG to `{ width, height, rgba }`, or `null` on failure. */
  decodePng(png: Uint8Array): DecodedImage | null {
    return this._decodeFramed(png, (p, l, o) => this.ex.gp_decode_png(p, l, o));
  }
  /** Decode a GIF (first frame) to `{ width, height, rgba }`, or `null`. */
  decodeGif(gif: Uint8Array): DecodedImage | null {
    return this._decodeFramed(gif, (p, l, o) => this.ex.gp_decode_gif(p, l, o));
  }
  /**
   * Encode raw RGBA pixels to a **lossless** WebP (VP8L) with the engine's native
   * encoder — no third-party image library. Empty array on a bad input.
   */
  encodeWebp(rgba: Uint8Array, width: number, height: number): Uint8Array {
    return this._withBytes(rgba, (p, l) =>
      this._buffer((o) => this.ex.gp_encode_webp(width, height, p, l, o))
    );
  }
  /**
   * Decode a WebP to `{ width, height, rgba }`, or `null`. Handles both
   * **lossless** (VP8L) and **lossy** (VP8 keyframe) WebP with the engine's
   * native decoder — no third-party image library. Extended/animated WebP
   * (`VP8X`) is not handled (returns `null`).
   */
  decodeWebp(webp: Uint8Array): DecodedImage | null {
    return this._decodeFramed(webp, (p, l, o) => this.ex.gp_decode_webp(p, l, o));
  }
  /**
   * Decode a still **AVIF** (AV1 intra) to `{ width, height, rgba }`, or `null`.
   * Pure-Rust AV1 decoder — no third-party library. Supports lossy + lossless
   * transforms, in-loop deblocking (AV1 §7.14) and CDEF (§7.15, including the
   * multi-strength `cdef_bits > 0` path), screen-content **palette** mode
   * (§5.11.46-50), and both `reduced_still_picture_header` and the full
   * streaming sequence/frame headers — all validated bit-exact against dav1d.
   * Not yet covered: animated AVIF, film grain, loop restoration (§7.17), the
   * fully bit-exact directional top-right/bottom-left intra edge, and the
   * lossless WHT path at very-high quality (`q ≤ 20`).
   */
  decodeAvif(avif: Uint8Array): DecodedImage | null {
    return this._decodeFramed(avif, (p, l, o) => this.ex.gp_decode_avif(p, l, o));
  }
  /** Unpack a `[w:u32 LE][h:u32 LE][rgba]` decode buffer; `null` if empty. */
  _decodeFramed(
    bytes: Uint8Array,
    fn: (p: number, l: number, o: number) => number
  ): DecodedImage | null {
    const framed = this._withBytes(bytes, (p, l) => this._buffer((o) => fn(p, l, o)));
    if (framed.length < 8) return null;
    const dv = new DataView(framed.buffer, framed.byteOffset, framed.byteLength);
    return { width: dv.getUint32(0, true), height: dv.getUint32(4, true), rgba: framed.subarray(8) };
  }

  // ── fonts (catalog / Google Fonts URL — the host performs the fetch) ───────
  fontCatalog(): FontInfo[] {
    return this._json((o) => this.ex.gp_font_catalog_json(o));
  }
  /** Google Fonts CSS2 URL for a family/weight/italic (host fetches it). */
  fontRequestUrl(family: string, weight = 400, italic = false): string {
    return this._withStr(family, (p, l) =>
      this._str((o) => this.ex.gp_font_request_url(p, l, weight, italic ? 1 : 0, o))
    );
  }
  /** Extract the trusted gstatic font URL from a Google Fonts CSS2 response. */
  parseCssFontUrl(css: string): string {
    return this._withStr(css, (p, l) => this._str((o) => this.ex.gp_parse_css_font_url(p, l, o)));
  }

  // ── JavaScript engine (no headless browser) ────────────────────────────────
  /**
   * Evaluate a JavaScript snippet with the built-in engine and return the
   * result value as a string (or `Uncaught …` / `SyntaxError: …`).
   */
  evalJs(src: string): string {
    return this._withStr(src, (p, l) => this._str((o) => this.ex.gp_js_eval(p, l, o)));
  }
  /**
   * Run a document's inline `<script>`s and return the resulting HTML. The
   * `htmlRender`/`htmlNeededFonts` paths already do this automatically; use this
   * only when you want the post-script HTML on its own.
   */
  runInlineScripts(html: string): string {
    return this._withStr(html, (p, l) =>
      this._str((o) => this.ex.gp_run_inline_scripts(p, l, o))
    );
  }

  // ── HTML rendering engine (replaces a headless browser for HTML→PDF) ───────
  /**
   * Phase 1 — the Google fonts the document references. Download each `url`
   * (→ TTF) and pass the bytes back to {@link htmlRender} for an identical render.
   */
  htmlNeededFonts(html: string): HtmlFontRequest[] {
    return this._withStr(html, (p, l) =>
      this._json((o) => this.ex.gp_html_needed_fonts(p, l, o))
    ) as HtmlFontRequest[];
  }

  /**
   * Phase 2 — render HTML + CSS to PDF, with the supplied fonts embedded (real
   * Google fonts, real metrics → identical or nearest match). Block, inline and
   * table layout with pagination. Page size and margin are in points
   * (US-Letter portrait, 0.5in margins by default). JavaScript is not executed.
   */
  htmlRender(
    html: string,
    fonts: HtmlFont[] = [],
    pageWidth = 612,
    pageHeight = 792,
    margin = 36
  ): Uint8Array {
    const blob = packHtmlFonts(fonts);
    return this._withStr(html, (hp, hl) =>
      this._withBytes(blob, (fp, fl) =>
        this._buffer((o) => this.ex.gp_html_render(hp, hl, fp, fl, pageWidth, pageHeight, margin, o))
      )
    );
  }

  /**
   * Resolve a named paper size — `"A4"`, `"a3-landscape"`, `"letter"`, `"legal"`,
   * `"tabloid"`, `"b5"`, … — to `{ w, h }` in points (portrait unless the name
   * has a `-landscape` suffix). Returns `null` for an unknown name.
   */
  pageSize(name: string): { w: number; h: number } | null {
    const outPtr = this.ex.gp_alloc(16);
    try {
      const ok = this._withStr(name, (p, l) =>
        this.ex.gp_page_size(p, l, outPtr, outPtr + 8)
      );
      if (!ok) return null;
      const dv = this.dv();
      return { w: dv.getFloat64(outPtr, true), h: dv.getFloat64(outPtr + 8, true) };
    } finally {
      this._free(outPtr, 16);
    }
  }

  /**
   * Phase 1 variant that also scans the running `header`/`footer` HTML, so the
   * Google fonts they reference are requested alongside the body's.
   */
  htmlNeededFontsWith(html: string, header?: string, footer?: string): HtmlFontRequest[] {
    return this._withStr(html, (hp, hl) =>
      this._withOptStr(header, (hdp, hdl) =>
        this._withOptStr(footer, (ftp, ftl) =>
          this._json((o) => this.ex.gp_html_needed_fonts_ex(hp, hl, hdp, hdl, ftp, ftl, o))
        )
      )
    ) as HtmlFontRequest[];
  }

  /**
   * Phase 2 with full page control: named/explicit size, per-side margins, and a
   * running header/footer painted in the page margins. `{{page}}` and `{{pages}}`
   * in the header/footer are replaced with the current / total page number.
   *
   * ```ts
   * const fonts = await fetchFonts(giga.htmlNeededFontsWith(html, header, footer));
   * const pdf = giga.htmlRenderWith(html, fonts, {
   *   pageSize: "A4",
   *   margin: { top: 72, bottom: 72, left: 54, right: 54 },
   *   header: `<div style="text-align:center">My Report</div>`,
   *   footer: `<div style="text-align:center">Page {{page}} / {{pages}}</div>`,
   * });
   * ```
   */
  htmlRenderWith(
    html: string,
    fonts: HtmlFont[] = [],
    options: HtmlRenderOptions = {}
  ): Uint8Array {
    let pw = options.pageWidth ?? 612;
    let ph = options.pageHeight ?? 792;
    if (options.pageSize) {
      const sz = this.pageSize(options.pageSize);
      if (!sz) throw new Error(`gigapdf: unknown page size "${options.pageSize}"`);
      pw = sz.w;
      ph = sz.h;
    }
    const m = options.margin ?? 36;
    const mg =
      typeof m === "number"
        ? { top: m, right: m, bottom: m, left: m }
        : { top: m.top ?? 36, right: m.right ?? 36, bottom: m.bottom ?? 36, left: m.left ?? 36 };
    const headerOffset = options.headerOffset ?? 18;
    const footerOffset = options.footerOffset ?? 18;
    const start = options.startPageNumber ?? 1;
    const blob = packHtmlFonts(fonts);
    const resBlob = packHtmlResources(options.resources ?? []);

    return this._withStr(html, (hp, hl) =>
      this._withBytes(blob, (fp, fl) =>
        this._withOptStr(options.header, (hdp, hdl) =>
          this._withOptStr(options.footer, (ftp, ftl) =>
            this._withBytes(resBlob, (rp, rl) =>
              this._buffer((o) =>
                this.ex.gp_html_render_opts(
                  hp,
                  hl,
                  fp,
                  fl,
                  pw,
                  ph,
                  mg.top,
                  mg.right,
                  mg.bottom,
                  mg.left,
                  hdp,
                  hdl,
                  ftp,
                  ftl,
                  headerOffset,
                  footerOffset,
                  start,
                  rp,
                  rl,
                  o
                )
              )
            )
          )
        )
      )
    );
  }

  /**
   * Phase 1 (unified) — every external resource the document needs the host to
   * fetch, in one list: fonts (`{kind:"font",family,weight,italic,url}`) and
   * external images (`{kind:"image",url}`). Run **one** fetch loop, then pass
   * fonts back as the `fonts` argument to {@link GigaPdfEngine.htmlRenderWith}
   * (download each `url` → TTF) and images via
   * {@link HtmlRenderOptions.resources} (`{url, bytes}`). `data:` image URIs are
   * inlined and never listed.
   */
  htmlNeededResources(html: string, header?: string, footer?: string): HtmlResourceNeed[] {
    return this._withStr(html, (hp, hl) =>
      this._withOptStr(header, (hdp, hdl) =>
        this._withOptStr(footer, (ftp, ftl) =>
          this._json((o) => this.ex.gp_html_needed_resources(hp, hl, hdp, hdl, ftp, ftl, o))
        )
      )
    ) as HtmlResourceNeed[];
  }

  // ── unified editable model: lower / edit / raise ───────────────────────────
  //
  // The {@link GigaDocument} model is a format-neutral tree (sections → pages →
  // blocks → runs). Lower any format into it (`*ToModel`), edit it with
  // {@link applyModelOps}, then raise it back to any format (`modelTo*`). This is
  // the substrate for a universal editor that edits every format the same way.

  /** Decode a model-JSON buffer returned by a `gp_model_*` export; `null` on an
   * empty (error) result. */
  private _modelOrNull(call: (outLenPtr: number) => number): GigaDocument | null {
    const s = this._str(call);
    return s ? (JSON.parse(s) as GigaDocument) : null;
  }

  /**
   * Lower an Office document (DOCX/XLSX/PPTX/ODT/ODS/ODP, auto-detected) into the
   * unified {@link GigaDocument} model. Returns `null` if the bytes aren't a
   * recognized Office container.
   */
  officeToModel(office: Uint8Array): GigaDocument | null {
    return this._withBytes(office, (p, l) =>
      this._modelOrNull((o) => this.ex.gp_model_from_office(p, l, o))
    );
  }

  /** Lower an HTML string into the unified {@link GigaDocument} model. */
  htmlToModel(html: string): GigaDocument {
    return this._withStr(html, (p, l) =>
      JSON.parse(this._str((o) => this.ex.gp_model_from_html(p, l, o)))
    ) as GigaDocument;
  }

  /**
   * Lower a Markdown string into the unified {@link GigaDocument} model
   * (CommonMark-ish: headings, lists, GFM tables, fenced code, emphasis/links).
   */
  mdToModel(md: string): GigaDocument {
    return this._withStr(md, (p, l) =>
      JSON.parse(this._str((o) => this.ex.gp_model_from_md(p, l, o)))
    ) as GigaDocument;
  }

  /**
   * Lower a CSV file (UTF-8 bytes, RFC 4180, auto-detected `,`/`;`/tab/`|`
   * delimiter) into the unified {@link GigaDocument} model as a single editable
   * table. Returns `null` if the bytes contain no parseable fields.
   */
  csvToModel(csv: Uint8Array): GigaDocument | null {
    return this._withBytes(csv, (p, l) =>
      this._modelOrNull((o) => this.ex.gp_model_from_csv(p, l, o))
    );
  }

  /**
   * Apply a batch of {@link ModelOp} edits to a model and return the edited
   * model. Ops run in order; out-of-range addresses (and any op that can't be
   * parsed) are silently skipped, so a partially-valid batch never throws.
   */
  applyModelOps(model: GigaDocument, ops: ModelOp[]): GigaDocument {
    return this._withStr(JSON.stringify(model), (mp, ml) =>
      this._withStr(JSON.stringify(ops), (op, ol) =>
        JSON.parse(this._str((o) => this.ex.gp_model_apply_ops(mp, ml, op, ol, o)))
      )
    ) as GigaDocument;
  }

  /** Raise a {@link GigaDocument} model to an editable Word document (`.docx`). */
  modelToDocx(model: GigaDocument): Uint8Array {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._buffer((o) => this.ex.gp_model_to_docx(p, l, o))
    );
  }
  /** Raise a model to an Excel workbook (`.xlsx`). */
  modelToXlsx(model: GigaDocument): Uint8Array {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._buffer((o) => this.ex.gp_model_to_xlsx(p, l, o))
    );
  }
  /** Raise a model to a PowerPoint presentation (`.pptx`). */
  modelToPptx(model: GigaDocument): Uint8Array {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._buffer((o) => this.ex.gp_model_to_pptx(p, l, o))
    );
  }
  /** Raise a model to an OpenDocument Text (`.odt`). */
  modelToOdt(model: GigaDocument): Uint8Array {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._buffer((o) => this.ex.gp_model_to_odt(p, l, o))
    );
  }
  /** Raise a model to an OpenDocument Spreadsheet (`.ods`). */
  modelToOds(model: GigaDocument): Uint8Array {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._buffer((o) => this.ex.gp_model_to_ods(p, l, o))
    );
  }
  /** Raise a model to an OpenDocument Presentation (`.odp`). */
  modelToOdp(model: GigaDocument): Uint8Array {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._buffer((o) => this.ex.gp_model_to_odp(p, l, o))
    );
  }
  /** Raise a model back to a PDF. */
  modelToPdf(model: GigaDocument): Uint8Array {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._buffer((o) => this.ex.gp_model_to_pdf(p, l, o))
    );
  }
  /** Raise a model to standalone HTML (decoded UTF-8 string). */
  modelToHtml(model: GigaDocument): string {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._str((o) => this.ex.gp_model_to_html(p, l, o))
    );
  }
  /** Raise a model to RTF (decoded UTF-8 string). */
  modelToRtf(model: GigaDocument): string {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._str((o) => this.ex.gp_model_to_rtf(p, l, o))
    );
  }
  /** Raise a model to Markdown (decoded UTF-8 string). */
  modelToMarkdown(model: GigaDocument): string {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._str((o) => this.ex.gp_model_to_md(p, l, o))
    );
  }
  /** Raise a model to CSV (decoded UTF-8 string). */
  modelToCsv(model: GigaDocument): string {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._str((o) => this.ex.gp_model_to_csv(p, l, o))
    );
  }
  /** Raise a model to an EPUB e-book (`.epub`). */
  modelToEpub(model: GigaDocument): Uint8Array {
    return this._withStr(JSON.stringify(model), (p, l) =>
      this._buffer((o) => this.ex.gp_model_to_epub(p, l, o))
    );
  }
}

/**
 * Default TSA round trip for {@link GigaPdfDoc.signTimestamped}: POST the
 * RFC 3161 `TimeStampReq` to `url` as `application/timestamp-query` and return
 * the raw `TimeStampResp` (`application/timestamp-reply`) body. Works in both
 * Node and the browser via the global `fetch`.
 *
 * No SSRF allow-listing is performed here — the URL is host-supplied. Consumers
 * that need to restrict it should pass their own `tsaFetch`.
 */
export async function defaultTsaPost(url: string, req: Uint8Array): Promise<Uint8Array> {
  const res = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/timestamp-query" },
    body: req as BodyInit,
    redirect: "error",
  });
  if (!res.ok) {
    throw new Error(`TSA HTTP ${res.status}`);
  }
  return new Uint8Array(await res.arrayBuffer());
}

/** Pack `HtmlResource[]` (host-fetched URLs) into the little-endian blob
 * `gp_html_render_opts` expects: `u32 count`, then per entry `u32 url_len, url,
 * u32 data_len, data`. */
function packHtmlResources(resources: HtmlResource[]): Uint8Array {
  let size = 4;
  for (const r of resources) size += 4 + enc.encode(r.url).length + 4 + r.bytes.length;
  const buf = new Uint8Array(size);
  const dv = new DataView(buf.buffer);
  let o = 0;
  dv.setUint32(o, resources.length, true);
  o += 4;
  for (const r of resources) {
    const url = enc.encode(r.url);
    dv.setUint32(o, url.length, true);
    o += 4;
    buf.set(url, o);
    o += url.length;
    dv.setUint32(o, r.bytes.length, true);
    o += 4;
    buf.set(r.bytes, o);
    o += r.bytes.length;
  }
  return buf;
}

/** Pack `HtmlFont[]` into the little-endian blob `gp_html_render` expects. */
function packHtmlFonts(fonts: HtmlFont[]): Uint8Array {
  let size = 4;
  for (const f of fonts) size += 4 + enc.encode(f.family).length + 2 + 1 + 4 + f.ttf.length;
  const buf = new Uint8Array(size);
  const dv = new DataView(buf.buffer);
  let o = 0;
  dv.setUint32(o, fonts.length, true);
  o += 4;
  for (const f of fonts) {
    const fam = enc.encode(f.family);
    dv.setUint32(o, fam.length, true);
    o += 4;
    buf.set(fam, o);
    o += fam.length;
    dv.setUint16(o, f.weight, true);
    o += 2;
    buf[o] = f.italic ? 1 : 0;
    o += 1;
    dv.setUint32(o, f.ttf.length, true);
    o += 4;
    buf.set(f.ttf, o);
    o += f.ttf.length;
  }
  return buf;
}

/** A Google font the HTML engine needs (resolved from the catalogue). */
export interface HtmlFontRequest {
  family: string;
  weight: number;
  italic: boolean;
  /** Google Fonts CSS URL — the host fetches it to obtain the TTF. */
  url: string;
}

/** A downloaded font handed to {@link GigaPdfEngine.htmlRender}. */
export interface HtmlFont {
  family: string;
  weight: number;
  italic: boolean;
  ttf: Uint8Array;
}

/**
 * A host-downloaded external resource (image) handed to
 * {@link GigaPdfEngine.htmlRenderWith} via {@link HtmlRenderOptions.resources}.
 * `url` must match the URL referenced in the HTML exactly.
 */
export interface HtmlResource {
  url: string;
  bytes: Uint8Array;
}

/**
 * One entry from {@link GigaPdfEngine.htmlNeededResources}: a `font` (with its
 * Google-Fonts download metadata) or an external `image` URL the host must fetch.
 */
export type HtmlResourceNeed =
  | { kind: "font"; family: string; weight: number; italic: boolean; url: string }
  | { kind: "image"; url: string };

/** Per-side page margins in points; omitted sides default to 36pt. */
export interface HtmlMargins {
  top?: number;
  right?: number;
  bottom?: number;
  left?: number;
}

/** Page setup for {@link GigaPdfEngine.htmlRenderWith}. */
export interface HtmlRenderOptions {
  /** Named paper size (`"A4"`, `"a3-landscape"`, `"letter"`, …) — wins over width/height. */
  pageSize?: string;
  /** Explicit page width in points (default 612 = US Letter). Ignored if `pageSize` is set. */
  pageWidth?: number;
  /** Explicit page height in points (default 792). Ignored if `pageSize` is set. */
  pageHeight?: number;
  /** Uniform margin (points) or per-side margins. Default 36pt (0.5in). */
  margin?: number | HtmlMargins;
  /** Running header HTML painted in the top margin (`{{page}}` / `{{pages}}` tokens). */
  header?: string;
  /** Running footer HTML painted in the bottom margin (same tokens). */
  footer?: string;
  /** Distance from the top edge to the header block, in points (default 18). */
  headerOffset?: number;
  /** Distance from the bottom edge to the footer block, in points (default 18). */
  footerOffset?: number;
  /** Number assigned to the first page for `{{page}}` (default 1). */
  startPageNumber?: number;
  /**
   * Host-downloaded external images, keyed by the exact URL referenced in the
   * HTML (`<img src>`). Obtain the list with
   * {@link GigaPdfEngine.htmlNeededResources}, fetch each, and pass the bytes
   * here — the engine never touches the network. `data:` URIs need no entry.
   */
  resources?: HtmlResource[];
}

export interface FontInfo {
  family: string;
  category: string;
  google: boolean;
  weights: number[];
}
/** A font embedded in a document (from {@link GigaPdfDoc.embeddedFonts}). */
export interface EmbeddedFont {
  /** The `/BaseFont` name (may carry a `ABCDEF+` subset prefix). */
  baseFont: string;
  /**
   * Embedded program format. `truetype` (glyf) and a full OpenType `cff`
   * (`OTTO`) re-embed directly via {@link GigaPdfDoc.embedFont}; bare `cff`
   * (Type1C) and `type1` are read-only here.
   */
  format: "truetype" | "cff" | "type1";
}
export interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}
export interface Element extends Partial<Box> {
  index: number;
  kind: "text" | "image" | "shape";
  label: string;
}
/**
 * A text element from {@link GigaPdfDoc.textElements}: the decoded text plus
 * everything a host editor needs to recreate the run. `index` is the text-run
 * index accepted by {@link GigaPdfDoc.replaceText}; the bounding box is page
 * user space (origin bottom-left).
 */
export interface TextElementInfo {
  index: number;
  text: string;
  x: number;
  y: number;
  width: number;
  height: number;
  /** Resolved `/BaseFont` family (e.g. "Helvetica", "Times New Roman"). */
  fontFamily: string;
  bold: boolean;
  italic: boolean;
  /** Effective glyph size in points. */
  fontSize: number;
  /** RGB fill colour, `0..1` per channel. */
  color: [number, number, number];
  /** Baseline rotation in degrees (0 = upright). */
  rotation: number;
  /**
   * Reading direction of this run by its strong characters: `"rtl"` for
   * Arabic/Hebrew, `"ltr"` for Latin/Greek/Cyrillic/CJK, `"neutral"` when the
   * run is only digits/punctuation/whitespace.
   */
  direction: 'ltr' | 'rtl' | 'neutral';
}
/**
 * The aggregate language signal of a document from
 * {@link GigaPdfDoc.documentLanguage}: its dominant reading {@link
 * TextElementInfo.direction | direction}, writing system, and a best-effort
 * ISO-639-1 language code (`"ar"`, `"he"`, `"zh"`/`"ja"`…), `undefined` when
 * the script does not pin a single language (e.g. plain Latin).
 */
export interface DocumentLanguage {
  direction: 'ltr' | 'rtl' | 'neutral';
  /** Dominant script: `"arabic" | "hebrew" | "latin" | "greek" | "cyrillic" | "cjk" | "other"`. */
  script: string;
  /** Best-effort ISO-639-1 code, or `undefined` when undecidable. */
  lang?: string;
}

// ── unified editable model (structural mirror of crate::model::Document JSON) ──
//
// These interfaces are a permissive, partial mirror of the model's stable JSON
// envelope. They give a host enough structure to read and rebuild a model (the
// fields it edits) without re-declaring every leaf; opaque sub-objects are typed
// loosely. The producers ({@link GigaPdfDoc.toModel}, {@link
// GigaPdfEngine.officeToModel}, {@link GigaPdfEngine.htmlToModel}) and the
// consumers ({@link GigaPdfEngine.modelToDocx} …) all round-trip this shape.

/** A portable font fallback class (mirrors `convert::style::Generic`). */
export type GigaGeneric = 'sans' | 'serif' | 'mono';

/** A run's character style (mirror of `model::style::CharStyle`'s JSON). */
export interface GigaCharStyle {
  family: string;
  generic: GigaGeneric;
  size_pt: number;
  bold: boolean;
  italic: boolean;
  underline: boolean;
  strike: boolean;
  /** RGB `0..=1`, or `null` for default black. */
  color: [number, number, number] | null;
  valign: 'baseline' | 'super' | 'sub';
}

/** An axis-aligned placement box, lower-left `(x,y)` + size, in PDF points. */
export interface GigaRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** A hyperlink destination (tagged; mirror of `model::LinkTarget`). */
export type GigaLinkTarget =
  | { t: 'url'; v: string }
  | { t: 'page'; v: number };

/**
 * An inline (within-paragraph) node, tagged by `t` (mirror of `model::Inline`).
 * `run` carries the styled text + its `source_index` back to the editable
 * content-stream operator; `br` is a hard line break; `image` an inline image;
 * `link` wraps children with a destination.
 */
export type GigaInline =
  | { t: 'run'; v: GigaInlineRun }
  | { t: 'br' }
  | { t: 'image'; v: GigaImageRef }
  | { t: 'link'; href: GigaLinkTarget; children: GigaInline[] };

/** A styled span of text (mirror of `model::InlineRun`). */
export interface GigaInlineRun {
  text: string;
  style: GigaCharStyle;
  /** Index of the source content-stream run for in-place round-tripping, or `null`. */
  source_index: number | null;
}

/** Paragraph-level formatting (mirror of `model::style::ParagraphStyle`). */
export interface GigaParagraphStyle {
  align: 'left' | 'center' | 'right' | 'justify';
  space_before_pt: number;
  space_after_pt: number;
  indent_left_pt: number;
  indent_right_pt: number;
  /** First-line indent (positive) or hanging indent (negative), in points. */
  first_line_pt: number;
  /** Leading policy: font-natural, a size multiple, or a fixed point value. */
  line_height:
    | { t: 'normal' }
    | { t: 'multiple'; v: number }
    | { t: 'points'; v: number };
}

/** A paragraph: its own style, an optional named-style ref, and its inline runs. */
export interface GigaParagraph {
  style: GigaParagraphStyle;
  /** Named style this paragraph derives from, if any. */
  style_ref: string | null;
  runs: GigaInline[];
}

/** A heading (`level` 1..=6) wrapping a paragraph (mirror of `model::Heading`). */
export interface GigaHeading {
  level: number;
  para: GigaParagraph;
}

/** A list bullet/number style (tagged; mirror of `model::ListMarker`). */
export type GigaListMarker =
  | { t: 'bullet'; v: string }
  | { t: 'decimal' }
  | { t: 'lower_alpha' }
  | { t: 'upper_alpha' }
  | { t: 'lower_roman' }
  | { t: 'upper_roman' };

/** One list item: nested blocks at a given `level` (mirror of `model::ListItem`). */
export interface GigaListItem {
  blocks: GigaBlock[];
  level: number;
}

/** An ordered or unordered list (mirror of `model::List`). */
export interface GigaList {
  ordered: boolean;
  marker: GigaListMarker;
  items: GigaListItem[];
}

/** A table cell: block content, span, and optional RGB shading (`model::Cell`). */
export interface GigaTableCell {
  blocks: GigaBlock[];
  col_span: number;
  row_span: number;
  /** RGB `0..=1` background, or `null` for no shading. */
  shading: [number, number, number] | null;
}

/** A table row: its cells and an optional fixed height in points (`model::Row`). */
export interface GigaTableRow {
  cells: GigaTableCell[];
  height: number | null;
}

/** A table/cell border (mirror of `model::BorderStyle`). */
export interface GigaBorderStyle {
  width: number;
  color: [number, number, number];
}

/** A table: rows of cells, explicit column widths, and a border (`model::Table`). */
export interface GigaTable {
  rows: GigaTableRow[];
  col_widths: number[];
  border: GigaBorderStyle;
}

/** A reference to an image blob in the document's resource table (`model::ImageRef`). */
export interface GigaImageRef {
  /** Content-hash key into `GigaDocument.resources.images`. */
  resource: number;
  alt: string | null;
}

/** A single vector path segment (tagged; mirror of `content::vector::PathSeg`). */
export type GigaPathSeg =
  | { t: 'm'; x: number; y: number }
  | { t: 'l'; x: number; y: number }
  | { t: 'c'; x1: number; y1: number; x2: number; y2: number; x: number; y: number }
  | { t: 'z' };

/** A vector shape: a path with fill/stroke styling (mirror of `model::Shape`). */
export interface GigaShape {
  segments: GigaPathSeg[];
  /** RGB `0..=1` fill, or `null` when unfilled. */
  fill: [number, number, number] | null;
  /** RGB `0..=1` stroke, or `null` when unstroked. */
  stroke: [number, number, number] | null;
  stroke_width: number;
  dash: number[];
}

/** A free-floating text box holding a list of blocks (mirror of `model::TextBox`). */
export interface GigaTextBox {
  blocks: GigaBlock[];
}

/** A typed spreadsheet cell (mirror of `model::SheetCell`). */
export interface GigaSheetCell {
  value: GigaCellValue;
  number_format: string | null;
  /** RGB `0..=1` cell fill, or `null` for none. */
  fill: [number, number, number] | null;
  style: GigaCharStyle;
}

/** One spreadsheet row (mirror of `model::SheetRow`). */
export interface GigaSheetRow {
  cells: GigaSheetCell[];
}

/** An inclusive merged-cell rectangle `(r0,c0)..=(r1,c1)`, zero-based. */
export interface GigaMergeRange {
  r0: number;
  c0: number;
  r1: number;
  c1: number;
}

/** A single named worksheet (mirror of `model::Sheet`). */
export interface GigaSheet {
  name: string;
  rows: GigaSheetRow[];
  merges: GigaMergeRange[];
  col_widths: number[];
}

/** A block of spreadsheet content: one or more sheets (mirror of `model::SheetBlock`). */
export interface GigaSheetBlock {
  sheets: GigaSheet[];
}

/** A slide layout placeholder role (tagged; mirror of `model::PlaceholderRole`). */
export type GigaPlaceholderRole =
  | { t: 'title' }
  | { t: 'subtitle' }
  | { t: 'body' }
  | { t: 'other'; v: string };

/** A slide placeholder: a block tagged with its semantic role (`model::Placeholder`). */
export interface GigaPlaceholder {
  role: GigaPlaceholderRole;
  block: GigaBlock;
}

/** Resolved page size + margins, in points (mirror of `model::geom::PageGeometry`). */
export interface GigaPageGeometry {
  width: number;
  height: number;
  margins: { top: number; right: number; bottom: number; left: number };
}

/** A single slide (mirror of `model::Slide`). */
export interface GigaSlide {
  geometry: GigaPageGeometry;
  shapes: GigaBlock[];
  placeholders: GigaPlaceholder[];
  notes: GigaBlock[] | null;
}

/** A block of presentation content: an ordered list of slides (`model::SlideBlock`). */
export interface GigaSlideBlock {
  slides: GigaSlide[];
}

/**
 * A block payload, **fully typed and discriminated by `t`** (mirror of
 * `model::BlockKind`'s JSON). Narrow on `kind.t` to read the variant body in
 * `kind.v` — e.g. a `paragraph` exposes `v.runs` (each `run` carrying
 * `style.bold`/`style.italic`/`style.size_pt`/`style.color`), a `heading` its
 * `v.level`, a `table` its `v.rows[].cells[]` (with `col_span`/`row_span`), and a
 * `list` its `v.ordered` + `v.items`. This is what lets a thin editor render the
 * recognised structure (bold, headings, tables, lists) 1:1.
 */
export type GigaBlockKind =
  | { t: 'paragraph'; v: GigaParagraph }
  | { t: 'heading'; v: GigaHeading }
  | { t: 'list'; v: GigaList }
  | { t: 'table'; v: GigaTable }
  | { t: 'image'; v: GigaImageRef }
  | { t: 'shape'; v: GigaShape }
  | { t: 'textbox'; v: GigaTextBox }
  | { t: 'sheet'; v: GigaSheetBlock }
  | { t: 'slide'; v: GigaSlideBlock };

/** Block rotation (tagged; mirror of `model::geom::Rotation`). */
export type GigaRotation =
  | { t: 'd0' }
  | { t: 'd90' }
  | { t: 'd180' }
  | { t: 'd270' }
  | { t: 'deg'; v: number };

/** A block: a stable id, an optional placement frame + rotation, and its kind. */
export interface GigaBlock {
  id: number;
  frame: GigaRect | null;
  rotation: GigaRotation;
  kind: GigaBlockKind;
}

/** A page: a list of blocks; `absolute` flags slide/form (positioned) layout. */
export interface GigaPage {
  blocks: GigaBlock[];
  absolute: boolean;
}

/** A section: one page geometry, optional running header/footer, and its pages. */
export interface GigaSection {
  geometry: GigaPageGeometry;
  header: GigaBlock[] | null;
  footer: GigaBlock[] | null;
  pages: GigaPage[];
}

/**
 * A document outline (bookmark) entry — a label, a zero-based destination page,
 * and nested children (mirror of `model::OutlineNode`). Populated by
 * {@link GigaPdfEngine.officeToModel}/reconstruction from the source's own bookmarks
 * (PDF `/Outlines`) or, lacking those, from detected headings.
 */
export interface GigaOutlineNode {
  /** The bookmark label. */
  title: string;
  /** Zero-based destination page in the document's flattened page sequence. */
  page: number;
  /** Nested sub-bookmarks. */
  children: GigaOutlineNode[];
}

/**
 * The unified editable document model — the format-neutral tree every format
 * lowers into and is reconstructed from. `v` is the envelope version. Leaves
 * beyond what a host typically edits (`styles`, `resources`) are carried
 * opaquely so a round-trip preserves them.
 */
export interface GigaDocument {
  v: number;
  meta: {
    title: string | null;
    author: string | null;
    subject: string | null;
    keywords: string[];
    lang: string | null;
  };
  styles: unknown;
  sections: GigaSection[];
  /** Bookmark / chapter hierarchy (empty when the source has none). */
  outline: GigaOutlineNode[];
  resources: unknown;
}

/**
 * A positional block address `[section, page, index]` (all zero-based) — the
 * stable handle an edit op uses to target a block (mirrors `model::BlockAddr`).
 */
export type GigaBlockAddr = [section: number, page: number, index: number];

/**
 * A character-style patch for restyle/insert ops: only the present fields are
 * applied (mirror of `model::edit::StylePatch`). `color: null` clears the colour
 * (→ default black); omitting `color` leaves it unchanged.
 */
export interface GigaStylePatch {
  family?: string;
  generic?: GigaGeneric;
  size_pt?: number;
  bold?: boolean;
  italic?: boolean;
  underline?: boolean;
  strike?: boolean;
  color?: [number, number, number] | null;
}

/**
 * A paragraph-style patch for {@link ModelOp} `setParagraphStyle`: only the
 * present fields are applied (mirror of `model::edit::ParaPatch`). Keys are the
 * op's short names (`indent_left`, not `indent_left_pt`); values in PDF points.
 * Omitting a field leaves the existing value unchanged.
 */
export interface GigaParaPatch {
  align?: 'left' | 'center' | 'right' | 'justify';
  indent_left?: number;
  indent_right?: number;
  /** First-line indent (positive) or hanging indent (negative), in points. */
  first_line?: number;
  space_before?: number;
  space_after?: number;
  /** Leading policy: font-natural, a size multiple, or a fixed point value. */
  line_height?: { t: 'normal' } | { t: 'multiple'; v: number } | { t: 'points'; v: number };
}

/** A typed spreadsheet cell value (tagged; mirror of `model::CellValue`). */
export type GigaCellValue =
  | { t: 'empty' }
  | { t: 'text'; v: string }
  | { t: 'number'; v: number }
  | { t: 'bool'; v: boolean };

/**
 * A single editing operation against a {@link GigaDocument} model, mirroring the
 * JSON shape of `model::edit::ModelOp`. Pass an array to
 * {@link GigaPdfEngine.applyModelOps}; ops run in order and out-of-range
 * addresses are no-ops.
 */
export type ModelOp =
  | { op: 'setRunText'; addr: GigaBlockAddr; run: number; text: string }
  | { op: 'restyleRun'; addr: GigaBlockAddr; run: number; style: GigaStylePatch }
  | { op: 'insertRun'; addr: GigaBlockAddr; run: number; text: string; style?: GigaStylePatch }
  | { op: 'deleteRun'; addr: GigaBlockAddr; run: number }
  | { op: 'insertBlock'; addr: GigaBlockAddr; block: GigaBlock }
  | { op: 'deleteBlock'; addr: GigaBlockAddr }
  | { op: 'moveBlock'; addr: GigaBlockAddr; to: GigaBlockAddr }
  | { op: 'setBlockText'; addr: GigaBlockAddr; text: string }
  | { op: 'restyleBlock'; addr: GigaBlockAddr; style: GigaStylePatch }
  | { op: 'setCellText'; addr: GigaBlockAddr; row: number; col: number; text: string }
  | {
      op: 'setSheetCell';
      addr: GigaBlockAddr;
      sheet: number;
      row: number;
      col: number;
      value: GigaCellValue;
    }
  // Structural table edits. These keep the column geometry (`col_widths` +
  // per-cell spans) coherent: an inserted row spans every logical column, an
  // inserted column passes through any merge it lands inside, and deletes shrink
  // or drop the cells/spans they touch. `at` is a grid index (clamped).
  | { op: 'insertTableRow'; addr: GigaBlockAddr; at: number }
  | { op: 'deleteTableRow'; addr: GigaBlockAddr; at: number }
  | { op: 'insertTableColumn'; addr: GigaBlockAddr; at: number }
  | { op: 'deleteTableColumn'; addr: GigaBlockAddr; at: number }
  | {
      op: 'setCellSpan';
      addr: GigaBlockAddr;
      /** Row index in `rows`. */
      row: number;
      /** Cell index in `rows[row].cells` (not a grid column). */
      col: number;
      /** Columns the cell spans (clamped to ≥ 1). */
      col_span: number;
      /** Rows the cell spans (clamped to ≥ 1). */
      row_span: number;
    }
  // Structural spreadsheet edits: shift cells and re-map merge ranges. `at` is a
  // row/column index (clamped); merges that collapse are dropped.
  | { op: 'insertSheetRow'; addr: GigaBlockAddr; sheet: number; at: number }
  | { op: 'deleteSheetRow'; addr: GigaBlockAddr; sheet: number; at: number }
  | { op: 'insertSheetColumn'; addr: GigaBlockAddr; sheet: number; at: number }
  | { op: 'deleteSheetColumn'; addr: GigaBlockAddr; sheet: number; at: number }
  // ── paragraph formatting ──
  | { op: 'setParagraphStyle'; addr: GigaBlockAddr; patch: GigaParaPatch }
  // ── list ──
  | { op: 'setListLevel'; addr: GigaBlockAddr; level: number }
  | { op: 'setListMarker'; addr: GigaBlockAddr; marker: GigaListMarker }
  | { op: 'setListOrdered'; addr: GigaBlockAddr; ordered: boolean }
  // ── absolute block placement ──
  | { op: 'setBlockFrame'; addr: GigaBlockAddr; rect: GigaRect }
  /** `deg` is CCW; 0/90/180/270 snap to exact rotations, else an arbitrary angle. */
  | { op: 'setBlockRotation'; addr: GigaBlockAddr; deg: number }
  // ── table shading & geometry ──
  /** `color: null` clears the cell's shading; a triple sets it. */
  | {
      op: 'setCellShading';
      addr: GigaBlockAddr;
      row: number;
      col: number;
      color: [number, number, number] | null;
    }
  | { op: 'setRowHeight'; addr: GigaBlockAddr; row: number; height: number }
  | { op: 'setColWidth'; addr: GigaBlockAddr; col: number; width: number }
  | { op: 'setTableBorder'; addr: GigaBlockAddr; border: GigaBorderStyle };
/**
 * An image element from {@link GigaPdfDoc.imageElements}: its placement box
 * (page user space, origin bottom-left), the embeddable encoded bytes + format,
 * and the source pixel dimensions. `data` is empty when `format` is `"unknown"`.
 */
export interface ImageElementInfo {
  /**
   * The image's **unified element index** — the same value accepted by
   * {@link GigaPdfDoc.removeElement} / {@link GigaPdfDoc.transformElement} /
   * {@link GigaPdfDoc.duplicateElement} / {@link GigaPdfDoc.moveElement}. Extract
   * an image here and pass this index to edit *that exact* image. It is **not** an
   * image-local 0,1,2 counter, so it is correct on pages that also have text/paths.
   */
  index: number;
  x: number;
  y: number;
  width: number;
  height: number;
  /** `"jpeg"` | `"png"` | `"jp2"` | `"unknown"`. */
  format: string;
  pixelWidth: number;
  pixelHeight: number;
  /** Embeddable encoded image bytes (empty when `format === "unknown"`). */
  data: Uint8Array;
  /** Rotation in degrees from the placement CTM (`0` = upright). */
  rotation: number;
  /** Non-stroking fill alpha (`/ExtGState` `/ca`), `0..=1` (`1` = opaque). */
  opacity: number;
}
/**
 * One path segment from {@link GigaPdfDoc.vectorPaths} (page user space, origin
 * bottom-left). `op` is `"M"` (move, 2 pts), `"L"` (line, 2 pts), `"C"` (cubic
 * Bézier, 6 pts: cp1 cp2 end) or `"Z"` (close, 0 pts). `pts` is the flat
 * coordinate list.
 */
export interface PathSegment {
  op: "M" | "L" | "C" | "Z";
  pts: number[];
}
/**
 * A painted vector path from {@link GigaPdfDoc.vectorPaths}: its geometry
 * (segments, bounds) plus the graphics state — fill/stroke RGB (`0..=1`, `null`
 * when the paint op doesn't fill/stroke), line width, alpha and dash. Clip-only
 * paths are omitted. The native equivalent of a reader's shape/vector layer.
 */
export interface VectorPathInfo {
  /**
   * The path's **unified element index** — the same value accepted by
   * {@link GigaPdfDoc.setPathStyle} / {@link GigaPdfDoc.removeElement} /
   * {@link GigaPdfDoc.transformElement}. Extract a path here and pass this index
   * to restyle or remove *that exact* path. Clip-only paths are not reported, so
   * the painted path you see is the one your index targets — not a path-local ordinal.
   */
  index: number;
  /** Whether `x0..y1` describe a real box (`false` for a degenerate path). */
  hasBounds: boolean;
  x0: number;
  y0: number;
  x1: number;
  y1: number;
  segments: PathSegment[];
  /** Fill colour `[r,g,b]` in `0..=1`, or `null` when not filled. */
  fill: [number, number, number] | null;
  /** Stroke colour `[r,g,b]` in `0..=1`, or `null` when not stroked. */
  stroke: [number, number, number] | null;
  /** Line width (`w`) in user-space units. */
  strokeWidth: number;
  /** Non-stroking alpha (`/ca`), `0..=1`. */
  fillAlpha: number;
  /** Stroking alpha (`/CA`), `0..=1`. */
  strokeAlpha: number;
  /** Dash pattern (`d` array); empty for a solid line. */
  dash: number[];
}
export interface TextLine extends Box {
  text: string;
}
export interface SearchHit extends Box {
  page: number;
  text: string;
}
export interface TextRunInfo {
  index: number;
  operator: string;
  text: string;
}
/** Signature-dictionary metadata for {@link GigaPdfDoc.signP12}. */
export interface SignP12Options {
  /** `/Name` — human-readable signer name. */
  name?: string;
  /** `/Reason` — why the document is being signed. */
  reason?: string;
  /** `/M` — signing time as a PDF date string, e.g. `D:20260616120000Z`. */
  date?: string;
  /** `/Location` — physical or logical signing location. */
  location?: string;
  /** `/ContactInfo` — how to reach the signer. */
  contactInfo?: string;
}
/**
 * Options for {@link GigaPdfDoc.signTimestamped} — a PAdES-B-T signature with an
 * RFC 3161 trusted timestamp embedded in the SignerInfo (`ETSI.CAdES.detached`
 * subfilter, `signing-certificate-v2` signed attribute, `id-aa-timeStampToken`
 * unsigned attribute).
 *
 * The signing identity is either an imported PKCS#12 (`p12` + `password`) or, if
 * `p12` is omitted, a freshly generated self-signed digital ID (`random` +
 * `notBefore`/`notAfter`).
 */
export interface SignTsaOptions extends SignP12Options {
  /** TSA endpoint URL, e.g. `"https://freetsa.org/tsr"`. */
  tsaUrl: string;
  /**
   * Optional override for the TSA round trip — lets the host add auth headers,
   * proxies, retries, **and apply its own SSRF allow-list** (the engine only
   * emits the request; the URL is host-supplied). Receives the `TimeStampReq`
   * DER and the URL, must resolve to the raw `TimeStampResp` bytes. When omitted,
   * {@link defaultTsaPost} POSTs `application/timestamp-query` via `fetch`.
   */
  tsaFetch?: (req: Uint8Array, url: string) => Promise<Uint8Array>;
  /** PKCS#12 identity bytes. Omit to sign with a generated self-signed ID. */
  p12?: Uint8Array;
  /** PKCS#12 passphrase. */
  password?: string;
  /** Self-signed path: ≥ 256 bytes from `crypto.getRandomValues`. */
  random?: Uint8Array;
  /** Self-signed path: RSA modulus size in bits (default 2048). */
  keyBits?: number;
  /** Self-signed path: certificate `notBefore`, UTCTime `YYMMDDHHMMSSZ`. */
  notBefore?: string;
  /** Self-signed path: certificate `notAfter`, UTCTime `YYMMDDHHMMSSZ`. */
  notAfter?: string;
  /** Optional 8–16 random bytes echoed by the TSA (request/response correlation). */
  nonce?: Uint8Array;
}

/**
 * Options for {@link GigaPdfDoc.signLtv} — a PAdES long-term validation
 * signature: a B-T signature, then a `/DSS` (Document Security Store) carrying
 * the certificate chain + OCSP/CRL revocation material the host fetched (B-LT),
 * optionally finished with a document timestamp over the whole file (B-LTA).
 *
 * Extends {@link SignTsaOptions}: the B-T signature is produced exactly as
 * {@link GigaPdfDoc.signTimestamped}, then the LTV material is added. The OCSP/CRL
 * URLs come from the **certificates**, so the host fetches them — supply
 * `revocationFetch`/`crlFetch` to add auth, proxies, or an SSRF allow-list (the
 * engine only computes which URLs to fetch).
 */
export interface SignLtvOptions extends SignTsaOptions {
  /**
   * Add a B-LTA **document timestamp** over the whole file (DSS included) after
   * the DSS. Requires a second TSA round trip. Default `false` (B-LT only).
   */
  archiveTimestamp?: boolean;
  /**
   * Override the OCSP round trip — receives the DER `OCSPRequest` and the
   * responder URL, must resolve to the raw `OCSPResponse` bytes. When omitted,
   * {@link defaultOcspPost} POSTs `application/ocsp-request` via `fetch`. Throw to
   * skip an unreachable responder (the DSS is built from whatever succeeds).
   */
  revocationFetch?: (req: Uint8Array, url: string) => Promise<Uint8Array>;
  /**
   * Override the CRL fetch — receives the CRL distribution-point URL, must
   * resolve to the raw `CertificateList` (CRL) bytes. When omitted,
   * {@link defaultCrlGet} GETs the URL. Throw to skip an unreachable CRL.
   */
  crlFetch?: (url: string) => Promise<Uint8Array>;
}

/**
 * Default OCSP round trip for {@link GigaPdfDoc.signLtv}: POST the DER
 * `OCSPRequest` to `url` as `application/ocsp-request` and return the raw
 * `OCSPResponse` body. No SSRF allow-listing — the URL comes from the
 * certificate's AIA extension; pass `revocationFetch` to restrict it.
 */
export async function defaultOcspPost(req: Uint8Array, url: string): Promise<Uint8Array> {
  const res = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/ocsp-request" },
    body: req as BodyInit,
    redirect: "error",
  });
  if (!res.ok) {
    throw new Error(`OCSP HTTP ${res.status}`);
  }
  return new Uint8Array(await res.arrayBuffer());
}

/**
 * Default CRL fetch for {@link GigaPdfDoc.signLtv}: GET the `CertificateList`
 * (CRL) from `url`. No SSRF allow-listing — the URL comes from the certificate's
 * CRL-DP extension; pass `crlFetch` to restrict it.
 */
export async function defaultCrlGet(url: string): Promise<Uint8Array> {
  const res = await fetch(url, { method: "GET", redirect: "error" });
  if (!res.ok) {
    throw new Error(`CRL HTTP ${res.status}`);
  }
  return new Uint8Array(await res.arrayBuffer());
}

/** Decode a lowercase/uppercase hex string into bytes (LTV targets carry binary
 * fields — certificate DER, OCSP requests — as hex inside JSON). */
function hexToBytes(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length >> 1);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(hex.substr(i * 2, 2), 16);
  }
  return out;
}

/** Length-frame a list of byte blobs as `[u32 LE count]([u32 LE len][bytes])*`
 * — the binary multi-blob form `gp_apply_dss` reads for certs/OCSPs/CRLs. */
function frameBlobs(blobs: Uint8Array[]): Uint8Array {
  let total = 4;
  for (const b of blobs) total += 4 + b.length;
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  view.setUint32(0, blobs.length, true);
  let pos = 4;
  for (const b of blobs) {
    view.setUint32(pos, b.length, true);
    pos += 4;
    out.set(b, pos);
    pos += b.length;
  }
  return out;
}

/** One certificate's LTV fetch plan, decoded from the `gp_ltv_targets` JSON. */
interface LtvTarget {
  certHex: string;
  sources: Array<
    | { kind: "ocsp"; url: string; requestHex: string }
    | { kind: "crl"; url: string }
  >;
}
/**
 * A markup annotation (rect corners in PDF user space, origin bottom-left).
 * Carries the common metadata plus the type-specific geometry a host editor
 * needs: text-markup `quadPoints`, freehand `inkList`, `stamp` name, and the
 * link target (`linkUri` / `linkPage`).
 */
export interface AnnotationInfo {
  index: number;
  subtype: string;
  x0: number;
  y0: number;
  x1: number;
  y1: number;
  contents: string;
  /** `/T` — author / title. Empty when absent. */
  author: string;
  /** `/Subj` — subject. Empty when absent. */
  subject: string;
  /** `/CreationDate` — raw PDF date (`D:YYYYMMDD…`). Empty when absent. */
  created: string;
  /** `/M` — raw PDF modification date. Empty when absent. */
  modified: string;
  /** `/Name` — stamp name (e.g. `"Approved"`). Empty when absent. */
  name: string;
  /** `/CA` non-stroking opacity, `0..=1` (`1` = opaque). */
  opacity: number;
  /** `/C` normalised to RGB `0..=1` (`[]` when no colour). */
  color: number[];
  /** `/QuadPoints` (8 values per quad) for text-markup annotations. */
  quadPoints: number[];
  /** `/InkList` — one inner array per freehand stroke (`x y x y …`). */
  inkList: number[][];
  /** Link external URI (`/A /URI`). Empty when internal/absent. */
  linkUri: string;
  /** Link internal destination page (1-based); `0` when external/absent. */
  linkPage: number;
}
/** A hyperlink annotation; `kind` discriminates the target. */
export interface LinkInfo {
  index: number;
  x0: number;
  y0: number;
  x1: number;
  y1: number;
  kind: "uri" | "page" | "unknown";
  uri?: string;
  page?: number;
}
export type FieldKind =
  | "text"
  | "checkbox"
  | "radio"
  | "pushbutton"
  | "combo"
  | "list"
  | "signature"
  | "unknown";
/** An AcroForm field with its flags and (for choices) options. */
/** One signature on the document (a `/Sig` field's `/V`), from {@link GigaPdfDoc.signatures}. */
export interface SignatureInfo {
  /** The signature field's `/T` name. */
  fieldName: string;
  /** The signer name (`/Name`), or `null`. */
  signerName: string | null;
  /** The stated reason (`/Reason`), or `null`. */
  reason: string | null;
  /** The stated location (`/Location`), or `null`. */
  location: string | null;
  /** The signing date string (`/M`, e.g. `D:20260624…`), or `null`. */
  date: string | null;
  /** The `/SubFilter` (e.g. `adbe.pkcs7.detached`), or `null`. */
  subFilter: string | null;
  /** The `/ByteRange` `[a, b, c, d]` the signature covers. */
  byteRange: [number, number, number, number];
}

/** The cryptographic verdict for one signature, from {@link GigaPdfDoc.verifySignatures}. */
export interface SignatureReport {
  /** The signature field's `/T` name. */
  fieldName: string;
  /** The `/ByteRange` is well-formed and within the file bounds. */
  byteRangeOk: boolean;
  /** The CMS `messageDigest` equals SHA-256 of the covered bytes (integrity). */
  digestOk: boolean;
  /** The SignerInfo signature validates under the signer certificate's key. */
  signatureOk: boolean;
  /** The signature covers the **whole** current file (nothing appended after it). */
  coversWholeDocument: boolean;
  /** The signer certificate's Common Name, or `null`. */
  signerCommonName: string | null;
  /** Number of certificates embedded in the CMS. */
  certCount: number;
  /** The recognised signature algorithm (`RSA+SHA-256`) or an unsupported note. */
  algorithm: string;
}

export interface FieldInfo {
  name: string;
  type: string;
  kind: FieldKind;
  flags: number;
  readOnly: boolean;
  required: boolean;
  multiline: boolean;
  fillable: boolean;
  /**
   * Whether this is a **comb** text field (`/Ff` bit 25): the value is laid out
   * one character per equally-spaced cell across {@link maxLen} cells (SSN, dates
   * and reference numbers on official forms). A host reproducing the field's
   * original spacing must honour this — the cells can't be inferred from the
   * value alone.
   */
  comb: boolean;
  /** Text alignment from `/Q` (AcroForm default applied): 0 = left, 1 = centre, 2 = right. */
  quadding: number;
  /**
   * Font resource name from the field's `/DA` default appearance (e.g. `"Helv"`,
   * `"ZaDb"`), resolved against the AcroForm; absent when no `Tf` is present.
   */
  daFont?: string;
  /** Font size in points from the `/DA` (`0` = auto-size), AcroForm default applied. */
  daSize: number;
  /**
   * `/MaxLen` for text fields; for a comb field this is the number of
   * equally-spaced cells the value is drawn into.
   */
  maxLen?: number;
  /** 1-based page the widget sits on (from its `/P`); absent if it has no widget. */
  page?: number;
  /**
   * Widget rectangle `[x, y, width, height]` in points, **top-left origin**
   * (already Y-flipped from the PDF's bottom-left `/Rect` for direct host use).
   * Absent when the field carries no `/Rect`.
   */
  bounds?: [number, number, number, number];
  value: string;
  options: string[];
}
/**
 * A view destination (ISO 32000-1 §12.3.2). `page` is 1-based; optional
 * coordinates are in the target page's user space (omit to leave unchanged).
 */
export type Destination =
  | { fit: "xyz"; page: number; left?: number; top?: number; zoom?: number }
  | { fit: "fit"; page: number }
  | { fit: "fitH"; page: number; top?: number }
  | { fit: "fitV"; page: number; left?: number }
  | { fit: "fitR"; page: number; rect: [number, number, number, number] }
  | { fit: "fitB"; page: number }
  | { fit: "fitBH"; page: number; top?: number }
  | { fit: "fitBV"; page: number; left?: number }
  | { fit: "named"; name: string };

/**
 * A PDF action (ISO 32000-1 §12.6) usable on a link, the document open-action,
 * or an outline bookmark — passed to {@link GigaPdfDoc.addLink} /
 * {@link GigaPdfDoc.setOpenAction} / {@link GigaPdfDoc.setBookmarks}.
 */
export type Action =
  | { type: "goto"; dest: Destination }
  | { type: "gotoR"; file: string; dest: Destination }
  | { type: "uri"; uri: string }
  | { type: "named"; action: "nextPage" | "prevPage" | "firstPage" | "lastPage" }
  | { type: "launch"; file: string }
  | { type: "javascript"; js: string }
  | { type: "submitForm"; url: string }
  | { type: "resetForm" };

/** One outline bookmark for {@link GigaPdfDoc.setBookmarks}. */
export interface Bookmark {
  /** The bookmark label. */
  title: string;
  /** Nesting depth (0 = top); deeper consecutive items become children. */
  level: number;
  /** What clicking the bookmark does (a `goto` becomes `/Dest`, else `/A`). */
  action?: Action;
}

/** One outline (bookmark) entry; `level` is the nesting depth (0 = top). */
export interface OutlineEntry {
  level: number;
  title: string;
  page?: number;
  /** `/F` flag bit 2 — label drawn bold (present when read via {@link GigaPdfDoc.outline}). */
  bold?: boolean;
  /** `/F` flag bit 1 — label drawn italic. */
  italic?: boolean;
  /** `/C` RGB label colour, `0..1` per channel (black when absent in the PDF). */
  color?: [number, number, number];
  /** Destination fit type, lowercased (`"xyz"`/`"fit"`/`"fith"`/`"fitv"`/…). */
  destKind?: string;
  /** `/XYZ` top-left X (when `destKind === "xyz"`). */
  x?: number;
  /** `/XYZ` top-left Y. */
  y?: number;
  /** `/XYZ` magnification. */
  zoom?: number;
}
/**
 * How a reader arranges pages on screen — the catalog `/PageLayout` name
 * (ISO 32000-1 §7.7.2). Passed to {@link GigaPdfDoc.setPageLayout}.
 */
export type PageLayout =
  | 'SinglePage'
  | 'OneColumn'
  | 'TwoColumnLeft'
  | 'TwoColumnRight'
  | 'TwoPageLeft'
  | 'TwoPageRight';

/**
 * Which panel (if any) a reader shows on open — the catalog `/PageMode` name
 * (ISO 32000-1 §7.7.2). Passed to {@link GigaPdfDoc.setPageMode}.
 */
export type PageMode =
  | 'UseNone'
  | 'UseOutlines'
  | 'UseThumbs'
  | 'FullScreen'
  | 'UseOC'
  | 'UseAttachments';

/**
 * Authoring options for the catalog `/ViewerPreferences` dictionary
 * (ISO 32000-1 §12.2) — UX hints a reader honours when it opens the document.
 *
 * Every field is optional: an omitted (or `undefined`) field leaves whatever
 * the document already had untouched; a boolean sets the corresponding key.
 * Passed to {@link GigaPdfDoc.setViewerPreferences}.
 */
export interface ViewerPreferences {
  /** `/HideToolbar` — hide the reader's tool bars. */
  hideToolbar?: boolean;
  /** `/HideMenubar` — hide the reader's menu bar. */
  hideMenubar?: boolean;
  /** `/HideWindowUI` — hide scroll bars / navigation controls, page only. */
  hideWindowUI?: boolean;
  /** `/FitWindow` — resize the window to fit the first page. */
  fitWindow?: boolean;
  /** `/CenterWindow` — centre the document window on screen. */
  centerWindow?: boolean;
  /** `/DisplayDocTitle` — show the doc title (not the file name) in the title bar. */
  displayDocTitle?: boolean;
  /** `/Direction` — predominant reading order: `"L2R"` or `"R2L"`. */
  direction?: 'L2R' | 'R2L';
}

/** A named destination from {@link GigaPdfDoc.namedDests}: a name → page anchor. */
export interface NamedDest {
  name: string;
  page: number;
}
/**
 * One document-level JavaScript read back by {@link GigaPdfDoc.documentJavascripts}:
 * the `/Names /JavaScript` name-tree key and the script source.
 */
export interface DocumentJavascript {
  /** The name-tree key the script is registered under. */
  name: string;
  /** The JavaScript source. */
  script: string;
}
/**
 * One embedded file attachment read back by {@link GigaPdfDoc.attachments}.
 * `data` is the decoded file bytes; `mime`/`description`/dates are `null` when
 * the PDF didn't record them.
 */
export interface Attachment {
  /** The `/EmbeddedFiles` name-tree key the file was registered under. */
  name: string;
  /** The filespec display filename (`/UF` preferred, else `/F`). */
  filename: string;
  /** The embedded stream's `/Subtype` MIME (e.g. `application/pdf`), or null. */
  mime: string | null;
  /** The filespec `/Desc` human description, or null. */
  description: string | null;
  /** The `/Params /CreationDate` PDF date string, or null. */
  creationDate: string | null;
  /** The `/Params /ModDate` PDF date string, or null. */
  modDate: string | null;
  /** The decoded (filters applied) file bytes. */
  data: Uint8Array;
}

/**
 * The relationship an **associated file** (`/AF`) bears to the document (ISO
 * 32000-2 / PDF/A-3). Hybrid e-invoices (Factur-X, ZUGFeRD, Order-X) embed their
 * XML payload as `"alternative"`.
 */
export type AfRelationship = "source" | "data" | "alternative" | "supplement" | "unspecified";

/** Maps an {@link AfRelationship} to the discriminant the engine expects. */
const AF_RELATIONSHIP_CODE: Record<AfRelationship, number> = {
  source: 0,
  data: 1,
  alternative: 2,
  supplement: 3,
  unspecified: 4,
};

/** Options for embedding a file attachment (see {@link GigaPdfDoc.addAttachment}). */
export interface AttachmentOptions {
  /** The embedded stream `/Subtype` MIME type (e.g. `"application/pdf"`). */
  mime?: string;
  /** A human-readable description (`/Desc`). */
  description?: string;
}

/** The visual marker of a {@link GigaPdfDoc.addFileAttachmentAnnot} annotation. */
export type FileAttachmentIcon = "PushPin" | "Paperclip" | "Graph" | "Tag";

/**
 * The standard document-information fields (ISO 32000-1 §14.3.3), shared by the
 * `/Info` dictionary and the XMP `/Metadata` packet. Passed to
 * {@link GigaPdfDoc.setInfo}, which writes both and keeps them in sync. Every
 * field is optional; on `setInfo` an omitted field is left unchanged (a partial
 * update). Dates are PDF date strings (`"D:YYYYMMDDHHmmSS+HH'mm'"`).
 */
export interface InfoFields {
  /** `/Title` → `dc:title`. */
  title?: string;
  /** `/Author` → `dc:creator`. */
  author?: string;
  /** `/Subject` → `dc:description`. */
  subject?: string;
  /** `/Keywords` → `pdf:Keywords`. */
  keywords?: string;
  /** `/Creator` (authoring app) → `xmp:CreatorTool`. */
  creator?: string;
  /** `/Producer` (PDF producer) → `pdf:Producer`. */
  producer?: string;
  /** `/CreationDate` (PDF date string) → `xmp:CreateDate`. */
  creationDate?: string;
  /** `/ModDate` (PDF date string) → `xmp:ModifyDate`. */
  modDate?: string;
}

/** One sheet read back from an `.xlsx` by {@link GigaPdfEngine.xlsxToGrids}. */
export interface XlsxSheet {
  name: string;
  rows: string[][];
}
/** A decoded raster image (`rgba` is `width*height*4`, row-major, RGBA8). */
export interface DecodedImage {
  width: number;
  height: number;
  rgba: Uint8Array;
}
/** An optional-content layer (calque): toggle `visible`/`locked` to persist in the PDF. */
export interface LayerInfo {
  id: number;
  name: string;
  visible: boolean;
  locked: boolean;
  order: number;
}
/** A page's geometry: size in points and `/Rotate` (0/90/180/270). */
export interface PageInfo {
  width: number;
  height: number;
  rotation: number;
  /** Raw `/MediaBox` `[x0, y0, x1, y1]` in user-space points (preserves origin). */
  mediaBox: [number, number, number, number];
}

/** Per-side page margins, in points. */
export interface PageMargins {
  top: number;
  right: number;
  bottom: number;
  left: number;
}

/**
 * The five page boundary boxes (ISO 32000-1 §14.11.2), in display/source order.
 * Used as the `kind` selector for {@link GigaPdfDoc.setPageBox} and as the keys of
 * {@link PageBoxes.declared}.
 */
export const PAGE_BOX_KINDS = ["media", "crop", "bleed", "trim", "art"] as const;

/** One of the five page boundary boxes — see {@link PAGE_BOX_KINDS}. */
export type PageBoxKind = (typeof PAGE_BOX_KINDS)[number];

/**
 * A page's five boundary boxes (see {@link GigaPdfDoc.getPageBoxes}). Each box is
 * the **effective** rectangle `[x0, y0, x1, y1]` in user-space points, with ISO
 * 32000-1 inheritance and the per-box default chain already applied — so `crop`
 * equals `media` when no `/CropBox` is declared, and `bleed`/`trim`/`art` each
 * fall back to `crop`. Values are reported verbatim (not clamped to their
 * intersection with the media box), so the source file round-trips faithfully.
 */
export interface PageBoxes {
  /** `/MediaBox` (inherited if absent; defaults to US Letter `[0, 0, 612, 792]`). */
  media: [number, number, number, number];
  /** `/CropBox` (inherited if absent; defaults to the media box). */
  crop: [number, number, number, number];
  /** `/BleedBox` (defaults to the crop box). */
  bleed: [number, number, number, number];
  /** `/TrimBox` (defaults to the crop box). */
  trim: [number, number, number, number];
  /** `/ArtBox` (defaults to the crop box). */
  art: [number, number, number, number];
  /**
   * Which boxes are **explicitly declared** on the page dictionary (vs inherited
   * from an ancestor `/Pages` node or defaulted by the rules above) — lets a host
   * tell a real `/TrimBox` from one defaulted to the crop box.
   */
  declared: Record<PageBoxKind, boolean>;
}

/**
 * The numbering style of a page-label range (ISO 32000-1 §12.4.2):
 * `decimal` (1,2,3…), `romanLower` (i,ii,iii…), `romanUpper` (I,II,III…),
 * `alphaLower` (a…z,aa…), `alphaUpper` (A…Z,AA…), or `none` (the prefix alone,
 * with no numeric portion).
 */
export type PageLabelStyle =
  | "decimal"
  | "romanLower"
  | "romanUpper"
  | "alphaLower"
  | "alphaUpper"
  | "none";

/** Maps a {@link PageLabelStyle} to the single-letter token the engine expects. */
const PAGE_LABEL_STYLE_TOKEN: Record<PageLabelStyle, string> = {
  decimal: "D",
  romanLower: "r",
  romanUpper: "R",
  alphaLower: "a",
  alphaUpper: "A",
  none: "-",
};

/**
 * One page-label range (ISO 32000-1 §12.4.2). From {@link startPage} onward (until
 * the next range, or the end of the document), pages are labelled {@link prefix}
 * followed by the {@link style}-formatted number counting up from
 * {@link startNumber}.
 */
export interface PageLabelRange {
  /** 1-based page number where this labelling range begins. */
  startPage: number;
  /** The numbering style of the numeric portion. */
  style: PageLabelStyle;
  /** A label prefix prepended to every page in the range (may be empty). */
  prefix: string;
  /** The value the range's first page is numbered with (≥ 1; default 1). */
  startNumber: number;
}

/**
 * The visual style of a page transition (ISO 32000-1 §12.4.4, the `/S` entry).
 * Index order matches the engine's enum and selects which optional fields of
 * {@link PageTransition} apply.
 */
export const PAGE_TRANSITION_STYLES = [
  "split",
  "blinds",
  "box",
  "wipe",
  "dissolve",
  "glitter",
  "fly",
  "push",
  "cover",
  "uncover",
  "fade",
  "replace",
] as const;

/** A page transition style — see {@link PAGE_TRANSITION_STYLES}. */
export type PageTransitionStyle = (typeof PAGE_TRANSITION_STYLES)[number];

/** Orientation of a `split`/`blinds` transition (the `/Dm` entry). */
export type PageTransitionDimension = "horizontal" | "vertical";

/** Motion direction of a `split`/`box` transition (the `/M` entry). */
export type PageTransitionMotion = "inward" | "outward";

/**
 * The sweep direction of a directional transition (the `/Di` entry). A number
 * of degrees — only the discrete angles `0`, `90`, `180`, `270`, `315` are
 * valid — or `"none"` (the `/Di /None` name, used by `fly` to mean "fly toward
 * the viewer").
 */
export type PageTransitionDirection = 0 | 90 | 180 | 270 | 315 | "none";

/**
 * A presentation transition + auto-advance for a page (ISO 32000-1 §12.4.4),
 * passed to {@link GigaPdfDoc.setPageTransition}. {@link style} picks the
 * effect; the remaining fields are optional and only the ones that apply to the
 * chosen style are written (`dimension`/`motion` for `split`, `direction`/
 * `scale`/`flyAreaOpaque` for `fly`, …). {@link displayDuration} is the page's
 * `/Dur` auto-advance time and is independent of the transition itself.
 */
export interface PageTransition {
  /** The transition style (`/S`). */
  style: PageTransitionStyle;
  /** `/D` — the transition effect duration in **seconds** (viewer default: 1). */
  duration?: number;
  /** `/Dm` — orientation; `split`/`blinds` only. */
  dimension?: PageTransitionDimension;
  /** `/M` — motion direction; `split`/`box` only. */
  motion?: PageTransitionMotion;
  /** `/Di` — sweep direction; `wipe`/`glitter`/`fly`/`cover`/`uncover`/`push`. */
  direction?: PageTransitionDirection;
  /** `/SS` — starting/ending scale for `fly` (PDF 1.5; default 1.0). */
  scale?: number;
  /** `/B` — whether the `fly` area is rectangular + opaque (PDF 1.5; default false). */
  flyAreaOpaque?: boolean;
  /** `/Dur` — the page's display time in **seconds** before auto-advancing. */
  displayDuration?: number;
}

/** Horizontal alignment of header/footer text within the printable width. */
export type HeaderFooterAlign = "left" | "center" | "right";

/**
 * A running header/footer to bake onto an existing PDF (see {@link GigaPdfDoc.setHeader} /
 * {@link GigaPdfDoc.setFooter}). `text` may contain the tokens `{{page}}` (1-based page
 * number) and `{{pages}}` (total page count), substituted per page. Text is drawn in
 * standard Helvetica inside the top (header) / bottom (footer) margin band.
 */
export interface HeaderFooterSpec {
  /** Template text, with `{{page}}` / `{{pages}}` tokens. */
  text: string;
  /** Horizontal alignment (default `"left"`). */
  align?: HeaderFooterAlign;
  /** Font size in points (default `10`). */
  fontSize?: number;
  /** RGB fill colour, `0..1` per channel (default black `[0,0,0]`). */
  color?: [number, number, number];
  /** Inclusive 1-based page range `[first, last]`; omit for every page. */
  pageRange?: [number, number] | null;
  /** Draw on the first page of the range too (default `true`). */
  showOnFirstPage?: boolean;
  /** Band height from the page edge, in points (default `36`). */
  bandHeight?: number;
}

const RGB = (rgb: number) => rgb & 0xffffff;

/** `n` bytes of Web Crypto randomness (for encryption seeds; the engine has none). */
function cryptoRandom(n: number): Uint8Array {
  const c = (globalThis as { crypto?: Crypto }).crypto;
  if (!c?.getRandomValues) {
    throw new Error(
      "this operation needs Web Crypto (globalThis.crypto.getRandomValues) or an explicit seed"
    );
  }
  const out = new Uint8Array(n);
  c.getRandomValues(out);
  return out;
}

/** One colour stop of a {@link GradientSpec}. */
export interface GradientStop {
  /** Position along the gradient axis, `0`…`1`. */
  offset: number;
  /** Colour, packed `0xRRGGBB`. */
  rgb: number;
}

/** A gradient fill for {@link GigaPdfDoc.addGradient}. */
export interface GradientSpec {
  /** Axial (`"linear"`) or radial geometry. */
  kind: "linear" | "radial";
  /** `[x0,y0,x1,y1]` for `"linear"`, `[x0,y0,r0,x1,y1,r1]` for `"radial"`. */
  coords: number[];
  /** At least two colour stops. */
  stops: GradientStop[];
  /** The rectangle `{ x, y, w, h }` filled with the gradient. */
  rect: Box;
  /** Extend before the first / after the last stop (`/Extend`; default `[true, true]`). */
  extend?: [boolean, boolean];
  /** Fill opacity `0`…`1` (default `1`). */
  opacity?: number;
}

/**
 * A fill colour in any authored colour space (ISO 32000-1 §8.6), for
 * {@link GigaPdfDoc.addFilledRectangle}/{@link GigaPdfDoc.addFilledPolygon}/
 * {@link GigaPdfDoc.addTextColor}. `cmyk`/`gray` are press-ready; `separation`
 * is a spot ink (with its `DeviceCMYK` tint approximation); `icc` embeds a
 * profile. CMYK/gray/tint components are `0`…`1`; `rgb` is packed `0xRRGGBB`.
 */
export type Color =
  | { space: "rgb"; rgb: number }
  | { space: "cmyk"; c: number; m: number; y: number; k: number }
  | { space: "gray"; gray: number }
  | {
      space: "separation";
      name: string;
      tint: number;
      cmyk: [number, number, number, number];
    }
  | { space: "icc"; components: number[]; profile: Uint8Array };

/** Visual styling for a newly-created form field. */
export interface FieldStyle {
  /** Text size in points; `0` (default) auto-sizes to the field box. */
  fontSize?: number;
  /** Text / mark colour `0xRRGGBB` (default black). */
  color?: number;
  /** Border colour `0xRRGGBB`, or `null` for no border (default black). */
  border?: number | null;
  /** Background fill `0xRRGGBB`, or `null` for transparent (default none). */
  background?: number | null;
  /** Border width in points (default `1`). */
  borderWidth?: number;
}

/** One option of a radio group: its export value and on-page rectangle. */
export interface RadioOption {
  /** The export value stored in the field when this button is selected. */
  export: string;
  /** `[x0, y0, x1, y1]` in PDF user space. */
  rect: [number, number, number, number];
}

/** Expand a {@link FieldStyle} into the 7 packed ABI arguments. */
function styleArgs(s: FieldStyle = {}): [number, number, number, number, number, number, number] {
  const hasBorder = s.border === null ? 0 : 1;
  const borderRgb = s.border == null ? 0x000000 : s.border;
  const hasBg = s.background == null ? 0 : 1;
  const bgRgb = s.background == null ? 0 : s.background;
  return [
    s.fontSize ?? 0,
    RGB(s.color ?? 0x000000),
    RGB(borderRgb),
    hasBorder,
    RGB(bgRgb),
    hasBg,
    s.borderWidth ?? 1,
  ];
}

/** A live document handle. Call {@link close} when done. */
export class GigaPdfDoc {
  constructor(
    private readonly g: GigaPdfEngine,
    private readonly h: number
  ) {}
  private ex() {
    return this.g.raw;
  }

  close() {
    this.ex().gp_close(this.h);
  }
  pageCount(): number {
    return this.ex().gp_page_count(this.h);
  }
  save(): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_save(this.h, o));
  }
  saveCompressed(): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_save_compressed(this.h, o));
  }

  /**
   * Serialize with PDF 1.5+ **object streams** + a **cross-reference stream**
   * (ISO 32000-1 §7.5.7 / §7.5.8) — the most compact output. `objectStreams`
   * (default `true`) packs non-stream objects into compressed `/ObjStm`s (needs
   * the xref stream); `xrefStreams` (default `true`) writes a `/XRef` stream.
   * Uncompressed streams are Flate-compressed first, like {@link saveCompressed}.
   * Linearization (Fast Web View) is not performed.
   */
  saveOptimized(opts: { objectStreams?: boolean; xrefStreams?: boolean } = {}): Uint8Array {
    const obj = opts.objectStreams ?? true;
    const xref = opts.xrefStreams ?? true;
    return this.g._buffer((o) =>
      this.ex().gp_save_optimized(this.h, obj ? 1 : 0, xref ? 1 : 0, o)
    );
  }

  // text intelligence
  textRuns(page: number): TextRunInfo[] {
    return this.g._json((o) => this.ex().gp_text_runs_json(this.h, page, o));
  }
  elements(page: number): Element[] {
    return this.g._json((o) => this.ex().gp_elements_json(this.h, page, o));
  }
  /**
   * Every text element on a page, enriched for an editor: the decoded text, its
   * bounding box (user space, bottom-left), the resolved font family +
   * bold/italic, the effective point size, the RGB fill colour and the baseline
   * rotation. `index` is the text-run index for {@link replaceText}, so a host
   * can extract, render and edit text from one model — the native replacement
   * for a reader's per-run text layer (which `elements()` omits font + colour).
   */
  textElements(page: number): TextElementInfo[] {
    return this.g._json((o) => this.ex().gp_text_elements_json(this.h, page, o));
  }
  /**
   * The document's aggregate language signal — its dominant reading direction
   * (`ltr`/`rtl`/`neutral`), writing system, and a best-effort ISO-639-1
   * language code — computed across every page's decoded text. Lets a host
   * pick fonts, set the UI direction or label the document without its own bidi
   * pass (e.g. detect an Arabic/Hebrew/Japanese PDF). `lang` is omitted when the
   * script does not pin a single language.
   */
  documentLanguage(): DocumentLanguage {
    const raw = this.g._json<{ direction: 'ltr' | 'rtl' | 'neutral'; script: string; lang: string | null }>(
      (o) => this.ex().gp_document_language(this.h, o)
    );
    return {
      direction: raw.direction,
      script: raw.script,
      ...(raw.lang != null ? { lang: raw.lang } : {}),
    };
  }
  /**
   * Every image element on a page: its placement box (user space, bottom-left),
   * the embeddable encoded bytes (`data`) + `format` (`jpeg`/`png`/`jp2`/
   * `unknown`), and the source pixel dimensions. DCTDecode/JPXDecode images pass
   * through as jpeg/jp2; Flate/raw DeviceRGB|DeviceGray are re-encoded to PNG.
   * The native replacement for a reader's image extraction (bytes + placement).
   *
   * Each result's `index` is the **unified element index** usable directly with
   * {@link removeElement} / {@link transformElement} / {@link duplicateElement} /
   * {@link moveElement} — so you can extract an image and edit *that exact* image.
   */
  imageElements(page: number): ImageElementInfo[] {
    const raw = this.g._json<Array<Omit<ImageElementInfo, 'data'> & { dataBase64: string }>>((o) =>
      this.ex().gp_image_elements_json(this.h, page, o)
    );
    return raw.map(({ dataBase64, ...rest }) => ({
      ...rest,
      data: this.g._fromBase64(dataBase64),
    }));
  }
  /**
   * Every painted vector path on `page` (frames, rules, lines, filled shapes…)
   * as geometry + style: segments and bounds in user space (origin bottom-left),
   * fill/stroke RGB, line width, alpha and dash. Clip-only paths are omitted.
   * The native replacement for walking a reader's operator list to rebuild the
   * shape layer.
   *
   * Each result's `index` is the **unified element index** usable directly with
   * {@link setPathStyle} / {@link removeElement} / {@link transformElement} — so
   * you can extract a path and restyle/remove *that exact* path.
   */
  vectorPaths(page: number): VectorPathInfo[] {
    return this.g._json((o) => this.ex().gp_vector_paths_json(this.h, page, o));
  }
  structuredText(page: number): TextLine[] {
    return this.g._json((o) => this.ex().gp_structured_text_json(this.h, page, o));
  }
  /**
   * The **layout blocks** of a single page — the structural reconstruction
   * (paragraphs, headings, lists, tables, shapes, images) of the page's flat
   * glyph/path geometry, in reading order (column-major), each {@link GigaBlock}
   * keeping a top-down `frame` and every text run its `source_index` back to the
   * editable content-stream operator.
   *
   * The **per-page** counterpart of {@link GigaPdfDoc.toModel} (which
   * reconstructs the whole document at once): a continuous / lazily-virtualized
   * editor calls this one page at a time. Identifies 1- and 2-column layouts,
   * merges lines into justified/left/centred/right paragraphs, promotes large
   * isolated lines to headings, and recovers ruled tables. An out-of-range page
   * yields `[]`.
   */
  pageBlocks(page: number): GigaBlock[] {
    return this.g._json((o) => this.ex().gp_page_blocks_json(this.h, page, o));
  }
  search(query: string, caseInsensitive = true): SearchHit[] {
    return this.g._withStr(query, (p, l) =>
      this.g._json((o) => this.ex().gp_search_json(this.h, p, l, caseInsensitive ? 1 : 0, o))
    );
  }

  // editing
  /**
   * Replace text run `index` on `page` with `text`. Font-aware: works with
   * **any** font — a run set in an embedded Type0/Identity-H face (TrueType or
   * OpenType-CFF) is re-encoded through that font's char→glyph map; base-14 and
   * simple fonts use WinAnsi. Returns `false` if the run/page doesn't exist.
   */
  replaceText(page: number, index: number, text: string): boolean {
    return (
      this.g._withStr(text, (p, l) => this.ex().gp_replace_text(this.h, page, index, p, l)) === 0
    );
  }
  removeElement(page: number, index: number): boolean {
    return this.ex().gp_remove_element(this.h, page, index) === 0;
  }
  moveElement(page: number, index: number, dx: number, dy: number): boolean {
    return this.ex().gp_move_element(this.h, page, index, dx, dy) === 0;
  }
  /**
   * Apply a full affine transform to element `index` on `page`, wrapping it in
   * `q … cm … Q` with the matrix `m = [a, b, c, d, e, f]`. This **generalises**
   * {@link moveElement} (whose matrix is the pure translate `[1,0,0,1,dx,dy]`)
   * to scale, rotation, shear and translation in one call. Because it is purely
   * matrix-based it works identically for text, images and shapes — their
   * internal coordinates are never touched. Returns `false` if the element/page
   * doesn't exist.
   */
  transformElement(
    page: number,
    index: number,
    m: [number, number, number, number, number, number]
  ): boolean {
    return (
      this.ex().gp_transform_element(this.h, page, index, m[0], m[1], m[2], m[3], m[4], m[5]) === 0
    );
  }
  /**
   * Re-style the **path** element `index` on `page` in place: any provided field
   * overrides that part of the graphics state for the path's paint; omitted
   * fields keep the inherited state. Implemented by wrapping the path's op range
   * in `q … Q` and injecting the requested state operators (`rg`/`RG`/`w`/`d`)
   * before its construction + paint ops, so the original paint op now draws with
   * the override and following content is unaffected. RGB colours are `[r,g,b]`
   * in `0..=1`; `dash` is the PDF dash array (`[]` = solid). Returns `false` if
   * the element is not a path (or the page/index doesn't exist).
   *
   * Opacity: `fillAlpha`/`strokeAlpha` (`0..=1`) are fully supported — an
   * `/ExtGState` carrying `/ca`/`/CA` is registered on the page and a `/<gs> gs`
   * is injected into the path's `q … Q` wrap, so the alpha applies to that path
   * run only. (For non-path elements such as images, use
   * {@link setElementOpacity}.)
   */
  setPathStyle(
    page: number,
    index: number,
    style: {
      fill?: [number, number, number];
      stroke?: [number, number, number];
      strokeWidth?: number;
      fillAlpha?: number;
      strokeAlpha?: number;
      dash?: number[];
    }
  ): boolean {
    return (
      this.g._withStr(JSON.stringify(style), (p, l) =>
        this.ex().gp_set_path_style_json(this.h, page, index, p, l)
      ) === 0
    );
  }
  /**
   * Re-style **sub-ranges** of text run `index` on `page` in place — the
   * by-character-run companion of {@link setPathStyle}. Each span sets the style
   * of the `[start, end)` UTF-16 slice of the run's *decoded* text (bold / italic
   * / underline / strike / colour / size); the run is split so the rest keeps its
   * original style and **positioning is preserved** (the original glyph codes,
   * including `TJ` kerning, are sliced and re-emitted — never re-encoded — and
   * each styled slice is wrapped in `q … Q`). Spans may be given in any order and
   * are clamped to the run's length.
   *
   * Notes on each field: `color` is `[r,g,b]` in `0..=1` (text fill); `sizePt`
   * rescales the slice's font in the run's own text scale; `bold` is faux-bold
   * (fill+stroke render mode) when no bold variant font is wired; `italic` is a
   * no-op without an italic/oblique variant (a stream edit can't shear glyphs
   * without disturbing advances); `underline`/`strike` draw a thin rule in page
   * space. Returns `false` when `index` does not resolve to a top-level text run
   * (e.g. it addresses form-XObject text), like {@link setPathStyle} for a
   * non-matching element.
   */
  setTextRunStyle(
    page: number,
    index: number,
    spans: Array<{
      start: number;
      end: number;
      color?: [number, number, number];
      sizePt?: number;
      bold?: boolean;
      italic?: boolean;
      underline?: boolean;
      strike?: boolean;
    }>
  ): boolean {
    return (
      this.g._withStr(JSON.stringify(spans), (p, l) =>
        this.ex().gp_set_text_run_style_json(this.h, page, index, p, l)
      ) === 0
    );
  }
  /**
   * Set a constant opacity on element `index` on `page` — text, image **or**
   * shape — by registering an `/ExtGState` (`/ca` = `/CA` = `fillAlpha`, clamped
   * to `0..=1`) on the page and wrapping the element's op range in
   * `q /<gs> gs … Q`. This is the way to set an **image**'s alpha in place;
   * shapes may also use {@link setPathStyle}'s `fillAlpha`/`strokeAlpha` (same
   * underlying `/ExtGState` mechanism). Returns `false` if the page/index doesn't
   * exist.
   */
  setElementOpacity(page: number, index: number, fillAlpha: number): boolean {
    return this.ex().gp_set_element_opacity(this.h, page, index, fillAlpha) === 0;
  }
  /**
   * Change the paint order (z-order) of element `index` on `page`. `toFront`
   * brings it visually on top (its op range is moved to the end of the content
   * stream, painted last); otherwise it is sent behind everything (moved to the
   * start, painted first). The moved range is re-wrapped in `q … Q` so it neither
   * inherits nor leaks graphics state. Works for text, image and shape elements.
   * The element's index changes after the move — re-read {@link GigaPdfDoc.elements}.
   * Returns `false` if the page/index doesn't exist.
   */
  reorderElement(page: number, index: number, toFront: boolean): boolean {
    return this.ex().gp_reorder_element(this.h, page, index, toFront ? 1 : 0) === 0;
  }
  duplicateElement(page: number, index: number): boolean {
    return this.ex().gp_duplicate_element(this.h, page, index) === 0;
  }
  /** Index of the element at page point `(x, y)`, or -1 if none. */
  elementAt(page: number, x: number, y: number): number {
    return this.ex().gp_element_at(this.h, page, x, y);
  }
  /**
   * Draw a vector rectangle. Pass an `0xRRGGBB` colour for `stroke`/`fill`, or
   * `null` to omit that paint. 0 → success.
   */
  addRectangle(
    page: number,
    x: number,
    y: number,
    w: number,
    h: number,
    stroke: number | null = null,
    fill: number | null = 0,
    lineWidth = 1,
    opacity = 1
  ): boolean {
    return (
      this.ex().gp_add_rectangle(
        this.h,
        page,
        x,
        y,
        w,
        h,
        RGB(stroke ?? 0),
        stroke === null ? 0 : 1,
        RGB(fill ?? 0),
        fill === null ? 0 : 1,
        lineWidth,
        opacity
      ) === 0
    );
  }

  /** Draw a straight line from `(x1,y1)` to `(x2,y2)`. `stroke` is `0xRRGGBB`. */
  drawLine(
    page: number,
    x1: number,
    y1: number,
    x2: number,
    y2: number,
    stroke = 0,
    lineWidth = 1,
    opacity = 1
  ): boolean {
    return (
      this.ex().gp_draw_line(this.h, page, x1, y1, x2, y2, RGB(stroke), lineWidth, opacity) === 0
    );
  }

  /**
   * Draw an ellipse (circle when `rx === ry`) centred at `(cx,cy)`. Pass an
   * `0xRRGGBB` colour for `stroke`/`fill`, or `null` to omit that paint.
   */
  addEllipse(
    page: number,
    cx: number,
    cy: number,
    rx: number,
    ry: number,
    stroke: number | null = null,
    fill: number | null = 0,
    lineWidth = 1,
    opacity = 1
  ): boolean {
    return (
      this.ex().gp_add_ellipse(
        this.h,
        page,
        cx,
        cy,
        rx,
        ry,
        RGB(stroke ?? 0),
        stroke === null ? 0 : 1,
        RGB(fill ?? 0),
        fill === null ? 0 : 1,
        lineWidth,
        opacity
      ) === 0
    );
  }

  /**
   * Paint a **linear** or **radial** gradient over `rect` on `page` (ISO 32000-1
   * §8.7.4 shading + §8.7.3 pattern). `coords` is `[x0,y0,x1,y1]` for `"linear"`
   * or `[x0,y0,r0,x1,y1,r1]` for `"radial"`; `stops` is ≥ 2 colour stops
   * (`offset` 0…1, `rgb` packed `0xRRGGBB`). `extend` (default `[true,true]`)
   * sets `/Extend`; `opacity` (default `1`) is the fill alpha.
   *
   * @example
   * doc.addGradient(1, {
   *   kind: "linear", coords: [50, 50, 250, 50],
   *   stops: [{ offset: 0, rgb: 0xff0000 }, { offset: 1, rgb: 0x0000ff }],
   *   rect: { x: 50, y: 40, w: 200, h: 60 },
   * });
   */
  addGradient(page: number, spec: GradientSpec): boolean {
    const kind = spec.kind === "radial" ? 1 : 0;
    const offsets = spec.stops.map((s) => s.offset);
    const colors = spec.stops.map((s) => RGB(s.rgb));
    const [es, ee] = spec.extend ?? [true, true];
    const opacity = spec.opacity ?? 1;
    const r = spec.rect;
    return (
      this.g._withF64(spec.coords, (cp, cc) =>
        this.g._withF64(offsets, (op, _oc) =>
          this.g._withU32(colors, (clp, _clc) =>
            this.ex().gp_add_gradient(
              this.h, page, kind, cp, cc, op, clp, offsets.length,
              r.x, r.y, r.w, r.h, es ? 1 : 0, ee ? 1 : 0, opacity
            )
          )
        )
      ) === 0
    );
  }

  /**
   * Encode a {@link Color} into the ABI's `(kind, comps, name, profile)` buffers
   * and invoke `fn` with the resulting pointers. Internal.
   */
  private _color<T>(
    color: Color,
    fn: (
      kind: number,
      compsPtr: number,
      compsCount: number,
      namePtr: number,
      nameLen: number,
      profPtr: number,
      profLen: number
    ) => T
  ): T {
    let kind: number;
    let comps: number[];
    let name = "";
    let profile: Uint8Array | null = null;
    switch (color.space) {
      case "cmyk":
        kind = 1;
        comps = [color.c, color.m, color.y, color.k];
        break;
      case "gray":
        kind = 2;
        comps = [color.gray];
        break;
      case "separation":
        kind = 3;
        comps = [color.tint, ...color.cmyk];
        name = color.name;
        break;
      case "icc":
        kind = 4;
        comps = color.components;
        profile = color.profile;
        break;
      default: {
        const c = RGB(color.rgb);
        kind = 0;
        comps = [((c >> 16) & 0xff) / 255, ((c >> 8) & 0xff) / 255, (c & 0xff) / 255];
      }
    }
    return this.g._withF64(comps, (cp, cc) =>
      this.g._withOptStr(name, (np, nl) =>
        profile
          ? this.g._withBytes(profile, (pp, pl) => fn(kind, cp, cc, np, nl, pp, pl))
          : fn(kind, cp, cc, np, nl, 0, 0)
      )
    );
  }

  /**
   * Fill a rectangle with `color` in **any** colour space — the press-ready
   * counterpart of {@link addRectangle} (CMYK, spot `Separation`, gray, ICC).
   *
   * @example
   * doc.addFilledRectangle(1, { x: 40, y: 700, w: 200, h: 40 },
   *   { space: "cmyk", c: 0.1, m: 0.8, y: 0.9, k: 0 });
   */
  addFilledRectangle(page: number, rect: Box, color: Color, opacity = 1): boolean {
    return (
      this._color(color, (kind, cp, cc, np, nl, pp, pl) =>
        this.ex().gp_add_filled_rectangle(
          this.h, page, rect.x, rect.y, rect.w, rect.h, kind, cp, cc, np, nl, pp, pl, opacity
        )
      ) === 0
    );
  }

  /**
   * Fill a polygon through flat `[x0,y0,x1,y1,…]` `points` (≥ 3 vertices) with
   * `color` in any colour space. The path is closed automatically.
   */
  addFilledPolygon(page: number, points: number[], color: Color, opacity = 1): boolean {
    return (
      this.g._withF64(points, (ptsPtr, ptsCount) =>
        this._color(color, (kind, cp, cc, np, nl, pp, pl) =>
          this.ex().gp_add_filled_polygon(
            this.h, page, ptsPtr, ptsCount, kind, cp, cc, np, nl, pp, pl, opacity
          )
        )
      ) === 0
    );
  }

  /**
   * Draw a base-14 text run in **any** colour space — the press-ready
   * counterpart of {@link addTextStandard}. `font` is a base-14 family
   * (`Helvetica`, `Times-Roman`, `Courier`, …).
   */
  addTextColor(
    page: number,
    x: number,
    y: number,
    size: number,
    text: string,
    font: string,
    color: Color,
    opts: { opacity?: number; rotation?: number; underline?: boolean; strikethrough?: boolean } = {}
  ): boolean {
    const { opacity = 1, rotation = 0, underline = false, strikethrough = false } = opts;
    return (
      this.g._withStr(text, (tp, tl) =>
        this.g._withStr(font, (fp, fl) =>
          this._color(color, (kind, cp, cc, np, nl, pp, pl) =>
            this.ex().gp_add_text_color(
              this.h, page, x, y, size, tp, tl, fp, fl, kind, cp, cc, np, nl, pp, pl,
              opacity, rotation, underline ? 1 : 0, strikethrough ? 1 : 0
            )
          )
        )
      ) === 0
    );
  }

  /**
   * Turn **overprint** on/off for content drawn **after** this call (`/op` fill,
   * `/OP` stroke, `/OPM` mode — `0` independent colorants, `1` non-erasing).
   * Essential for prepress black-overprint / trapping.
   */
  setOverprint(page: number, fill: boolean, stroke: boolean, mode = 1): boolean {
    return this.ex().gp_set_overprint(this.h, page, fill ? 1 : 0, stroke ? 1 : 0, mode) === 0;
  }

  /**
   * Add a document-level **OutputIntent** embedding the ICC `profile` (its `/N`
   * is read from the profile), with `condition` as the output-condition
   * identifier (e.g. `"Coated FOGRA39"`). Decoupled from the PDF/A path.
   */
  addOutputIntent(profile: Uint8Array, condition: string): boolean {
    return (
      this.g._withBytes(profile, (pp, pl) =>
        this.g._withStr(condition, (cp, cl) =>
          this.ex().gp_add_output_intent(this.h, pp, pl, cp, cl)
        )
      ) === 0
    );
  }

  /**
   * Draw a polyline/polygon through flat `[x0,y0,x1,y1,…]` points. `close` joins
   * the last vertex back to the first. `0xRRGGBB` colours, or `null` to omit.
   */
  addPolygon(
    page: number,
    points: number[],
    close = true,
    stroke: number | null = null,
    fill: number | null = 0,
    lineWidth = 1,
    opacity = 1
  ): boolean {
    return (
      this.g._withF64(points, (p, c) =>
        this.ex().gp_add_polygon(
          this.h,
          page,
          p,
          c,
          close ? 1 : 0,
          RGB(stroke ?? 0),
          stroke === null ? 0 : 1,
          RGB(fill ?? 0),
          fill === null ? 0 : 1,
          lineWidth,
          opacity
        )
      ) === 0
    );
  }

  /**
   * Draw an SVG path (`M`/`L`/`C`/`Q`/`Z`…) anchored so the SVG origin maps to
   * `(ox,oy)` with the Y axis flipped — same convention as `pdf-lib`'s
   * `drawSvgPath`. Covers freeform/polygon/triangle shapes.
   */
  addPath(
    page: number,
    svgPath: string,
    ox: number,
    oy: number,
    stroke: number | null = null,
    fill: number | null = 0,
    lineWidth = 1,
    opacity = 1
  ): boolean {
    return (
      this.g._withStr(svgPath, (p, l) =>
        this.ex().gp_add_path(
          this.h,
          page,
          p,
          l,
          ox,
          oy,
          RGB(stroke ?? 0),
          stroke === null ? 0 : 1,
          RGB(fill ?? 0),
          fill === null ? 0 : 1,
          lineWidth,
          opacity
        )
      ) === 0
    );
  }

  /**
   * Embed a raster image (PNG or JPEG bytes) at `(x,y)` sized `(w,h)` in PDF
   * user space. PNG alpha is honoured; `opacity` (0..1) sets an overall alpha.
   */
  addImage(
    page: number,
    data: Uint8Array,
    x: number,
    y: number,
    w: number,
    h: number,
    opacity = 1
  ): boolean {
    return (
      this.g._withBytes(data, (p, l) =>
        this.ex().gp_add_image(this.h, page, p, l, x, y, w, h, opacity)
      ) === 0
    );
  }
  /**
   * Replace the pixels of an **existing image XObject in place** — swap a logo or
   * photo while every reference to it stays intact (ISO 32000-1 §8.9). `index` is
   * the **unified element index** of an image on `page`, exactly the `index`
   * reported by {@link imageElements} (and accepted by {@link removeElement} /
   * {@link transformElement}); `data` is a fresh PNG or JPEG.
   *
   * Unlike a delete-then-re-add, the image keeps its object number, **every `/Do`
   * placement, and its position / scale / rotation / clip matrix** — only the
   * stream bytes and the image dictionary (`/Width`, `/Height`, `/ColorSpace`,
   * `/BitsPerComponent`, `/Filter`) are rewritten. The new raster is re-encoded
   * through the same path as {@link addImage} (PNG alpha → a fresh `/SMask`; JPEG
   * → `/DCTDecode` passthrough). It need not match the old pixel size — it is
   * drawn into the same box (transform the element first to re-fit it).
   *
   * Returns `false` if `page`/`index` doesn't resolve to a top-level image, or the
   * bytes aren't a decodable PNG/JPEG.
   */
  replaceImage(page: number, index: number, data: Uint8Array): boolean {
    return (
      this.g._withBytes(data, (p, l) => this.ex().gp_replace_image(this.h, page, index, p, l)) === 0
    );
  }
  /**
   * Stamp an **image watermark** across pages from raw image bytes. Accepts the
   * same five formats the engine decodes — **PNG, JPEG, WebP, GIF, AVIF**. The
   * image is embedded **once** and referenced on every target page.
   *
   * `opts.pages` is a list of 1-based page numbers; omit it (or pass `[]`) to
   * stamp every page. `opts.anchor` positions the image (`'center'` default, or a
   * corner) and `opts.offsetX`/`offsetY` nudge it (in points; in `tile` mode they
   * become the gaps between tiles). `opts.width`/`height` set the target size in
   * points (height keeps the source aspect ratio when omitted). `opts.rotationDeg`
   * rotates about the image centre and `opts.opacity` (0–1) sets the alpha.
   * Returns `false` if the image can't be decoded.
   */
  addImageWatermark(
    data: Uint8Array,
    opts: {
      pages?: number[];
      anchor?: 'center' | 'top-left' | 'top-right' | 'bottom-left' | 'bottom-right';
      offsetX?: number;
      offsetY?: number;
      width?: number;
      height?: number;
      rotationDeg?: number;
      opacity?: number;
      tile?: boolean;
    } = {}
  ): boolean {
    const anchorTag = {
      center: 0,
      'top-left': 1,
      'top-right': 2,
      'bottom-left': 3,
      'bottom-right': 4,
    }[opts.anchor ?? 'center'];
    const pages = opts.pages ?? [];
    const call = (pp: number, pc: number) =>
      this.g._withBytes(data, (p, l) =>
        this.ex().gp_add_image_watermark(
          this.h,
          p,
          l,
          pp,
          pc,
          anchorTag,
          opts.offsetX ?? 0,
          opts.offsetY ?? 0,
          opts.width ?? 0,
          opts.height ?? 0,
          opts.rotationDeg ?? 0,
          opts.opacity ?? 0.25,
          opts.tile ? 1 : 0
        )
      );
    const rc = pages.length === 0 ? call(0, 0) : this.g._withU32(pages, (pp, pc) => call(pp, pc));
    return rc === 0;
  }
  /**
   * Draw SVG markup on a page as **native vector paths** (crisp at any zoom, not
   * rasterized), fitting its `viewBox` into the box `(x, y, w, h)` in PDF points
   * (origin bottom-left). Supports shapes, `<path>`, groups, transforms and
   * fill/stroke/opacity. Returns `false` if the SVG can't be parsed.
   */
  addSvg(page: number, svg: string, x: number, y: number, w: number, h: number): boolean {
    return (
      this.g._withStr(svg, (p, l) => this.ex().gp_add_svg(this.h, page, p, l, x, y, w, h)) === 0
    );
  }
  /** True redaction: delete content intersecting the region (no opaque cover by default). */
  redact(
    page: number,
    x: number,
    y: number,
    w: number,
    h: number,
    coverRgb = 0,
    hasCover = false
  ): number {
    return this.ex().gp_redact_region(this.h, page, x, y, w, h, RGB(coverRgb), hasCover ? 1 : 0);
  }

  /**
   * True **PII redaction** of one or more rectangles `(x, y, width, height)` in
   * PDF points (origin bottom-left), in a single call. For every rect this:
   *
   * - **deletes** the overlapping text/vector elements from the content stream —
   *   the glyphs and their `/ToUnicode` mapping are gone, so copy/paste and text
   *   extraction reveal nothing in the area;
   * - **overwrites the pixels** of any underlying image (a scan/photo) with opaque
   *   black — only the intersecting sub-rectangle, so the rest of the page image
   *   survives — and re-encodes the image, erasing the sensitive pixels from the
   *   bytes (not merely covering them);
   * - strips overlapping annotations and clears their form-field values;
   * - paints an opaque **black** box over the rect as the visible redaction mark.
   *
   * The black cover is the default for PII (unlike {@link redact}). Pass
   * `opts.coverRgb` to change the mark colour, or `opts.cover = false` to remove
   * the content/pixels with no visible box. Returns the number of content
   * elements deleted across all rects.
   */
  redactPii(
    page: number,
    rects: { x: number; y: number; width: number; height: number }[],
    opts: { cover?: boolean; coverRgb?: number } = {}
  ): number {
    if (rects.length === 0) return 0;
    const flat: number[] = [];
    for (const r of rects) flat.push(r.x, r.y, r.width, r.height);
    const cover = opts.cover ?? true;
    const coverRgb = opts.coverRgb ?? 0x000000;
    return this.g._withF64(flat, (p, c) =>
      this.ex().gp_redact_pii(this.h, page, p, c, RGB(coverRgb), cover ? 1 : 0)
    );
  }

  // pages
  rotatePage(page: number, degrees: number): boolean {
    return this.ex().gp_rotate_page(this.h, page, degrees) === 0;
  }
  deletePage(page: number): boolean {
    return this.ex().gp_delete_page(this.h, page) === 0;
  }
  movePage(from: number, to: number): boolean {
    return this.ex().gp_move_page(this.h, from, to) === 0;
  }
  /**
   * Append pages of another PDF onto this document. With no `pages`, every page
   * is appended (powers *merge*). With `pages` — 1-based page numbers, in the
   * order given — only that selection is brought in (ISO 32000-1 §7.7.3), each
   * page keeping its content, resources, annotations and box geometry. Numbers
   * out of range are skipped; an empty/all-out-of-range selection returns
   * `false`.
   */
  appendPages(otherPdf: Uint8Array, pages?: number[]): boolean {
    if (pages === undefined) {
      return this.g._withBytes(otherPdf, (p, l) => this.ex().gp_append_pages(this.h, p, l)) === 0;
    }
    return (
      this.g._withBytes(otherPdf, (bp, bl) =>
        this.g._withU32(pages, (pp, pc) =>
          this.ex().gp_append_pages_subset(this.h, bp, bl, pp, pc)
        )
      ) === 0
    );
  }
  /**
   * Add an invisible (text render mode 3) standard-Helvetica text layer to
   * `page` in a SINGLE content append — for OCR. Each run is `{x, y, size,
   * text, rotation?}` (PDF user space, baseline-anchored, `rotation`° CCW).
   * Runs whose text has any non-WinAnsi glyph are skipped. Returns the number
   * of runs actually written (0 on engine error).
   */
  addTextLayer(
    page: number,
    runs: { x: number; y: number; size: number; text: string; rotation?: number }[]
  ): number {
    const parts: Uint8Array[] = [];
    let total = 0;
    for (const r of runs) {
      const t = enc.encode(r.text);
      const head = new Uint8Array(36);
      const dv = new DataView(head.buffer);
      dv.setFloat64(0, r.x, true);
      dv.setFloat64(8, r.y, true);
      dv.setFloat64(16, r.size, true);
      dv.setFloat64(24, r.rotation ?? 0, true);
      dv.setUint32(32, t.length, true);
      parts.push(head, t);
      total += 36 + t.length;
    }
    const buf = new Uint8Array(total);
    let off = 0;
    for (const p of parts) {
      buf.set(p, off);
      off += p.length;
    }
    const written = this.g._withBytes(buf, (p, l) =>
      this.ex().gp_add_text_layer(this.h, page, p, l)
    );
    return written < 0 ? 0 : written;
  }
  /** Extract the given 1-based page numbers into a NEW standalone PDF. */
  extractPages(pages: number[]): Uint8Array {
    return this.g._withU32(pages, (p, c) =>
      this.g._buffer((o) => this.ex().gp_extract_pages(this.h, p, c, o))
    );
  }
  /**
   * Resize a page's `/MediaBox` to `width`×`height` points — **box geometry
   * only**. The drawn content keeps its own coordinates (it is *not* rescaled to
   * the new box, so it may clip or float). To scale the page content along with
   * the box (keeping annotations aligned) use {@link scalePageContent} or
   * {@link scalePageTo}; for large-format authoring see {@link setUserUnit}.
   */
  resizePage(page: number, width: number, height: number): boolean {
    return this.ex().gp_resize_page(this.h, page, width, height) === 0;
  }
  /**
   * Scale a page's **content** uniformly by `factor` about the page origin (ISO
   * 32000-1 §8.3.4) — a true zoom of the whole page, not just the box. Three
   * things move together: (1) the content stream is wrapped in
   * `q <factor 0 0 factor 0 0> cm … Q`; (2) `/MediaBox` and `/CropBox` (resolved
   * through inheritance) plus any locally declared `/BleedBox`/`/TrimBox`/
   * `/ArtBox` are scaled about the origin; (3) every annotation `/Rect` is scaled,
   * so widget/stamp appearances — mapped from their `/BBox` into the `/Rect` per
   * §12.5.5 — rescale to stay aligned automatically. `factor` must be finite and
   * positive. `false` on engine error (bad page number / non-positive factor).
   */
  scalePageContent(page: number, factor: number): boolean {
    return this.ex().gp_scale_page_content(this.h, page, factor) === 0;
  }
  /**
   * Anisotropic {@link scalePageContent}: scale a page's content by `sx`
   * horizontally and `sy` vertically about the origin, scaling the boxes and
   * annotation `/Rect`s by the same factors. Both must be finite and positive.
   * `false` on engine error.
   */
  scalePageContentXY(page: number, sx: number, sy: number): boolean {
    return this.ex().gp_scale_page_content_xy(this.h, page, sx, sy) === 0;
  }
  /**
   * Scale a page's content to **fit within** `width`×`height` points (shrink- or
   * grow-to-fit), preserving aspect ratio — equivalent to {@link scalePageContent}
   * with `min(width / currentWidth, height / currentHeight)` over the page's
   * displayed size (`/MediaBox` with `/Rotate` applied). Returns the uniform
   * factor applied, or `null` on engine error (bad page number, non-positive
   * target, or zero-area page).
   */
  scalePageTo(page: number, width: number, height: number): number | null {
    const factor = this.ex().gp_scale_page_to(this.h, page, width, height);
    return factor < 0 ? null : factor;
  }
  /**
   * Set a page's `/UserUnit` (ISO 32000-1 §14.11.2) for **large-format
   * authoring**: one default user-space unit becomes `unit`⁄72 inch, so the same
   * coordinates render larger (e.g. `2.0` doubles the physical size, lifting the
   * ~200-inch coordinate ceiling). `1.0` is the default and removes the key. This
   * is distinct from {@link scalePageContent}: it rescales the coordinate system
   * (a viewer-level zoom of the medium), not the content or boxes. `unit` must be
   * finite and positive. `false` on engine error.
   */
  setUserUnit(page: number, unit: number): boolean {
    return this.ex().gp_set_user_unit(this.h, page, unit) === 0;
  }
  /**
   * A page's `/UserUnit` (ISO 32000-1 §14.11.2), defaulting to `1.0` when absent.
   * Returns `null` on engine error (bad page number).
   */
  pageUserUnit(page: number): number | null {
    const unit = this.ex().gp_page_user_unit(this.h, page);
    return unit < 0 ? null : unit;
  }
  /** Insert a blank page after the 1-based `after` page (0 = front); returns its id. */
  addPage(width: number, height: number, after = 0): number {
    return this.ex().gp_add_page(this.h, width, height, after);
  }
  /** Duplicate a page, inserting the copy right after it; returns the new page's id. */
  copyPage(page: number): number {
    return this.ex().gp_copy_page(this.h, page);
  }
  /**
   * N-up imposition (ISO 32000-1 §8.10): draw the **content** of the 1-based
   * `source` page, scaled and translated, onto the 1-based `target` page — the
   * basis for 2-up/4-up, booklets, contact sheets and page-on-page stamping. The
   * source page becomes a Form XObject (its content + `/Resources`, so its fonts
   * and images come along) and is drawn with `q <cm> /Fmn Do Q` appended to the
   * target. `(x, y)` is where the visible page's lower-left corner lands and
   * `(scaleX, scaleY)` scale the page **as displayed** (its `/MediaBox` origin
   * and `/Rotate` are absorbed into the matrix). Wrapped in `q`/`Q`, so it never
   * disturbs existing target content and is **composable** — call it repeatedly
   * (different sources and/or cells) to build an N-up sheet. `source` and
   * `target` may be the same page. `false` on engine error (bad page number).
   */
  placePage(
    target: number,
    source: number,
    x: number,
    y: number,
    scaleX: number,
    scaleY: number
  ): boolean {
    return this.ex().gp_place_page(this.h, target, source, x, y, scaleX, scaleY) === 0;
  }
  /**
   * Place `source` onto `target` (both 1-based) using an explicit content-stream
   * matrix `[a, b, c, d, e, f]` — the low-level primitive behind
   * {@link placePage} for full control of the affine (shear, arbitrary rotation,
   * reflection). The matrix maps the source page's form space (its user space,
   * including the `/MediaBox` origin) to the target; unlike {@link placePage} no
   * origin/rotation normalisation is applied, so identity draws the source 1:1
   * at the target origin. `false` on engine error.
   */
  placePageMatrix(target: number, source: number, matrix: [number, number, number, number, number, number]): boolean {
    const [a, b, c, d, e, f] = matrix;
    return this.ex().gp_place_page_matrix(this.h, target, source, a, b, c, d, e, f) === 0;
  }
  /**
   * Impose **all** existing pages `cols × rows` per sheet onto freshly added
   * sheets (2-up, 4-up, booklet thumbnails, contact sheets). Pages go
   * left-to-right, top-to-bottom; each is scaled uniformly to fit its cell
   * (aspect preserved) and centred. The new sheets are appended after the last
   * original page and the **originals are removed**, leaving only the imposed
   * sheets. Returns the number of sheets produced (negative on error: `cols`/
   * `rows` 0, or an empty document). `sheetWidth`/`sheetHeight` default to A4
   * portrait; `margin`/`gutter` to 14 pt.
   */
  nUp(
    cols: number,
    rows: number,
    opts: { sheetWidth?: number; sheetHeight?: number; margin?: number; gutter?: number } = {}
  ): number {
    return this.ex().gp_n_up(
      this.h,
      cols,
      rows,
      opts.sheetWidth ?? 595.28,
      opts.sheetHeight ?? 841.89,
      opts.margin ?? 14,
      opts.gutter ?? 14
    );
  }
  /** A page's size (points) and `/Rotate` (0/90/180/270). */
  pageInfo(page: number): PageInfo {
    return this.g._json((o) => this.ex().gp_page_info_json(this.h, page, o));
  }

  // margins + running header/footer

  /**
   * A page's margins (points): the gap between `/CropBox` and `/MediaBox` when a
   * CropBox exists, else estimated from the content bounding box.
   */
  pageMargins(page: number): PageMargins {
    return this.g._json<PageMargins>((o) => this.ex().gp_page_margins(this.h, page, o));
  }

  /**
   * Set a page's margins (points) by insetting its `/CropBox` from the `/MediaBox`
   * — a real, visible margin change. Returns `true` on success.
   */
  setPageMargins(page: number, m: PageMargins): boolean {
    return this.ex().gp_set_page_margins(this.h, page, m.top, m.right, m.bottom, m.left) === 0;
  }

  /**
   * All five page boundary boxes (`media`/`crop`/`bleed`/`trim`/`art`) for the
   * 1-based `page`, each as `[x0, y0, x1, y1]` in points, with ISO 32000-1
   * inheritance and defaults applied. See {@link PageBoxes} for the exact
   * default chain and the `declared` flags.
   */
  getPageBoxes(page: number): PageBoxes {
    return this.g._json<PageBoxes>((o) => this.ex().gp_page_boxes_json(this.h, page, o));
  }

  /**
   * Set one of a page's boundary boxes. `kind` is one of {@link PAGE_BOX_KINDS}
   * and `box` is given as `{ x, y, w, h }` (origin + size, points); it is written
   * as `[x, y, x+w, y+h]`, normalised so reversed sizes are accepted. Sibling
   * boxes are preserved. Returns `true` on success, `false` for an unknown kind,
   * a degenerate box (zero/negative area), or a bad page number.
   *
   * Setting `"trim"`/`"bleed"` is the prerequisite for PDF/X and commercial-print
   * (imposition, bleed, finished-size) pipelines.
   */
  setPageBox(page: number, kind: PageBoxKind, box: Box): boolean {
    const k = PAGE_BOX_KINDS.indexOf(kind);
    if (k < 0) return false;
    return (
      this.ex().gp_set_page_box(this.h, page, k, box.x, box.y, box.x + box.w, box.y + box.h) === 0
    );
  }

  /**
   * The document's page-label ranges (`/PageLabels`, ISO 32000-1 §12.4.2), sorted
   * by `startPage` (1-based). Empty when the document declares no page labels.
   */
  getPageLabels(): PageLabelRange[] {
    return this.g._json<PageLabelRange[]>((o) => this.ex().gp_page_labels_json(this.h, o));
  }

  /**
   * Replace the document's page labels. Pass an **empty** array to remove all
   * labels. Ranges are sorted by `startPage` and collapsed to one entry per page
   * (last wins). Returns `true` on success.
   *
   * @example
   * // Front matter in lowercase roman, body in decimal, appendix "A-1, A-2…".
   * doc.setPageLabels([
   *   { startPage: 1, style: "romanLower", prefix: "", startNumber: 1 },
   *   { startPage: 5, style: "decimal",    prefix: "", startNumber: 1 },
   *   { startPage: 20, style: "decimal",   prefix: "A-", startNumber: 1 },
   * ]);
   */
  setPageLabels(ranges: PageLabelRange[]): boolean {
    const text = ranges
      .map(
        (r) =>
          `${r.startPage}\t${PAGE_LABEL_STYLE_TOKEN[r.style] ?? "-"}\t${r.startNumber ?? 1}\t${
            r.prefix ?? ""
          }`
      )
      .join("\n");
    return this.g._withOptStr(text, (p, l) => this.ex().gp_set_page_labels(this.h, p, l)) === 0;
  }

  /**
   * The viewer-visible label string for the 1-based `page` (e.g. `"iv"`, `"A-3"`),
   * resolving the applicable `/PageLabels` range; the decimal page number when no
   * range applies (including a document with no page labels).
   */
  pageLabel(page: number): string {
    return this.g._str((o) => this.ex().gp_page_label(this.h, page, o));
  }

  /**
   * Author a presentation transition + auto-advance on `page` (1-based),
   * ISO 32000-1 §12.4.4. Writes the page's `/Trans` dictionary from
   * {@link PageTransition.style} plus only the optional sub-keys that apply to
   * that style (`dimension`/`motion` for `split`, `direction`/`scale`/
   * `flyAreaOpaque` for `fly`, …) — inapplicable fields are dropped.
   * {@link PageTransition.displayDuration} becomes the page's `/Dur`
   * (auto-advance); omitting it removes any existing `/Dur`. Re-calling replaces
   * the prior transition in full (idempotent). Returns `true` on success,
   * `false` for an unknown style, a negative/non-finite `duration`/`scale`/
   * `displayDuration`, or a bad page number.
   *
   * @example
   * // Kiosk slideshow: every page wipes in over 0.5 s, auto-advancing after 5 s.
   * for (let p = 1; p <= doc.pageCount(); p++) {
   *   doc.setPageTransition(p, { style: "wipe", duration: 0.5, displayDuration: 5 });
   * }
   */
  setPageTransition(page: number, trans: PageTransition): boolean {
    const style = PAGE_TRANSITION_STYLES.indexOf(trans.style);
    if (style < 0) return false;
    const dimension =
      trans.dimension === "horizontal" ? 0 : trans.dimension === "vertical" ? 1 : -1;
    const motion = trans.motion === "inward" ? 0 : trans.motion === "outward" ? 1 : -1;
    const direction =
      trans.direction === undefined ? -2 : trans.direction === "none" ? -1 : trans.direction;
    const flyB = trans.flyAreaOpaque === undefined ? -1 : trans.flyAreaOpaque ? 1 : 0;
    return (
      this.ex().gp_set_page_transition(
        this.h,
        page,
        style,
        trans.duration ?? Number.NaN,
        dimension,
        motion,
        direction,
        trans.scale ?? Number.NaN,
        flyB,
        trans.displayDuration ?? Number.NaN
      ) === 0
    );
  }

  /**
   * The presentation transition declared on `page` (1-based), or `null` when the
   * page has no `/Trans` (ISO 32000-1 §12.4.4). Optional fields that do not apply
   * to the recovered style are omitted from the result.
   */
  getPageTransition(page: number): PageTransition | null {
    return this.g._jsonOrNull<PageTransition>((o) => this.ex().gp_page_transition_json(this.h, page, o));
  }

  /**
   * Remove any presentation transition from `page` (1-based) — drops the page's
   * `/Trans` dictionary and its `/Dur` auto-advance entry. A no-op if neither is
   * present. Returns `true` on success.
   */
  clearPageTransition(page: number): boolean {
    return this.ex().gp_clear_page_transition(this.h, page) === 0;
  }

  /**
   * Bake a running header onto every in-range page (idempotent: re-baking
   * replaces the prior header). Returns `true` on success.
   */
  setHeader(spec: HeaderFooterSpec): boolean {
    return (
      this.g._withStr(JSON.stringify(spec), (p, l) => this.ex().gp_set_header(this.h, p, l)) === 0
    );
  }

  /** Bake a running footer onto every in-range page (idempotent). */
  setFooter(spec: HeaderFooterSpec): boolean {
    return (
      this.g._withStr(JSON.stringify(spec), (p, l) => this.ex().gp_set_footer(this.h, p, l)) === 0
    );
  }

  /** Remove every previously-baked running header from all pages. */
  removeHeaders(): boolean {
    return this.ex().gp_remove_headers(this.h) === 0;
  }

  /** Remove every previously-baked running footer from all pages. */
  removeFooters(): boolean {
    return this.ex().gp_remove_footers(this.h) === 0;
  }

  /**
   * Detect the running header/footer already baked into this PDF — the reader
   * counterpart of {@link GigaPdfDoc.setHeader} / {@link GigaPdfDoc.setFooter}.
   * Each side is a {@link HeaderFooterSpec} (with its recovered `text`) when a
   * baked `/GPHF` span is present, or `null` when absent. The `text` is faithful
   * (the per-page-substituted text of the first matching page); `align`,
   * `fontSize`, `color`, etc. are best-effort defaults, since the bake records
   * only the drawn text. Use it to reflect existing document state (e.g. a
   * Word-like editor toggle).
   */
  headerFooter(): { header: HeaderFooterSpec | null; footer: HeaderFooterSpec | null } {
    return this.g._json<{ header: HeaderFooterSpec | null; footer: HeaderFooterSpec | null }>((o) =>
      this.ex().gp_header_footer(this.h, o),
    );
  }

  // render
  renderPage(page: number, scale = 1): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_render_page(this.h, page, scale, o));
  }

  /**
   * Rasterize a page to a PNG **without the page content stream's text** — glyphs
   * are suppressed while gradients, shadings, images, vectors and patterns are
   * preserved. Form-field **widget** appearances are omitted (the editor re-shows
   * their values as an editable overlay, so baking them in would double every
   * field); other annotation appearances (stamps, highlights, ink) are still
   * painted. Use this to paint a text-free raster background the editor can
   * overlay real, editable text on top of.
   */
  renderPageNoText(page: number, scale = 1): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_render_page_no_text(this.h, page, scale, o));
  }

  /**
   * Rasterize a page to a PNG while **omitting** the given top-level element
   * `indices` (from {@link GigaPdfDoc.elements}). Each excluded element paints nothing
   * — fills, strokes, shadings, images and text alike — while everything else
   * (including the non-text content of non-excluded elements) renders normally.
   * Use it to paint a background without specific elements and overlay live,
   * editable versions on top. Generalises {@link renderPageNoText} (which
   * suppresses *all* text). Like {@link renderPageNoText}, form-field **widget**
   * appearances are omitted (the editor re-shows them as an editable overlay);
   * other annotation appearances are painted. An empty `indices` renders the full
   * page; unknown indices are ignored.
   */
  renderPageExcluding(page: number, indices: number[], scale = 1): Uint8Array {
    return this.g._buffer((o) =>
      this.g._withU32(indices, (p, c) =>
        this.ex().gp_render_page_excluding(this.h, page, p, c, scale, o)
      )
    );
  }

  // fonts — embed a downloaded font, then add real selectable text
  /**
   * Embed an outline font program as a Type0 font and return its object number
   * (pass to {@link addText} / re-encoded by {@link replaceText}). Accepts
   * **any** font file — glyf **TrueType** (`.ttf`) or **OpenType-CFF**
   * (`.otf`/`OTTO`), flavour auto-detected — so `font` may be a Google Font the
   * host fetched, an `.otf` you supply, or a face pulled out of a document with
   * {@link extractFont}. Returns 0 on a malformed/unsupported program.
   */
  embedFont(family: string, font: Uint8Array): number {
    const b = enc.encode(family);
    const famPtr = this.g._toWasm(b);
    const obj = this.g._withBytes(font, (p, l) =>
      this.ex().gp_embed_font(this.h, famPtr, b.length, p, l)
    );
    this.g._free(famPtr, b.length);
    return obj;
  }
  /**
   * Draw real, selectable text at `(x, y)` (PDF points, origin bottom-left) in a
   * font embedded with {@link embedFont} (`fontObj`). Works with **any** embedded
   * face — glyf TrueType or OpenType-CFF — encoding each character through the
   * font's char→glyph map (Identity-H, 2-byte glyph ids). `rgb` is packed
   * `0xRRGGBB`; `rotationDeg` rotates CCW about `(x, y)`. For a built-in base-14
   * family with no embedding, use {@link addStandardText}.
   *
   * Pass `opts` to bake **text decorations** into the run: `{ underline: true }`
   * draws a rule just below the baseline, `{ strikethrough: true }` a rule near
   * the x-height — both span the run's advance and are filled in the text colour.
   * Omitting `opts` is fully backward-compatible (no decoration).
   */
  addText(
    page: number,
    x: number,
    y: number,
    size: number,
    text: string,
    fontObj: number,
    rgb = 0,
    opacity = 1,
    rotationDeg = 0,
    opts?: { underline?: boolean; strikethrough?: boolean }
  ): boolean {
    const underline = opts?.underline ? 1 : 0;
    const strikethrough = opts?.strikethrough ? 1 : 0;
    return (
      this.g._withStr(text, (p, l) =>
        this.ex().gp_add_text_styled(
          this.h,
          page,
          x,
          y,
          size,
          p,
          l,
          fontObj,
          RGB(rgb),
          opacity,
          rotationDeg,
          underline,
          strikethrough
        )
      ) === 0
    );
  }
  /**
   * Draw `text` at `(x, y)` in a built-in **base-14 standard font** (`fontName`,
   * e.g. `"Times-Bold"`, `"Courier-Oblique"`, `"Symbol"`) — no embedding needed,
   * every viewer ships these 14. For any other family embed a TrueType with
   * {@link embedFont} (a Google Font fetched by the host, or one pulled out of
   * the document with {@link extractFont}) and use {@link addText}. Returns
   * `false` on an unknown font name or bad page.
   *
   * Pass `opts` to bake **text decorations** into the run: `{ underline: true }`
   * and/or `{ strikethrough: true }` draw filled rules in the text colour.
   * Omitting `opts` is fully backward-compatible (no decoration).
   */
  addStandardText(
    page: number,
    x: number,
    y: number,
    size: number,
    text: string,
    fontName: string,
    rgb = 0,
    opacity = 1,
    rotationDeg = 0,
    opts?: { underline?: boolean; strikethrough?: boolean }
  ): boolean {
    const underline = opts?.underline ? 1 : 0;
    const strikethrough = opts?.strikethrough ? 1 : 0;
    return (
      this.g._withStr(text, (tp, tl) =>
        this.g._withStr(fontName, (fp, fl) =>
          this.ex().gp_add_text_standard_styled(
            this.h,
            page,
            x,
            y,
            size,
            tp,
            tl,
            fp,
            fl,
            RGB(rgb),
            opacity,
            rotationDeg,
            underline,
            strikethrough
          )
        )
      ) === 0
    );
  }
  /**
   * Stamp a standard-**Helvetica** watermark (no font embed needed): `text` at
   * `(x, y)`, rotated `rotationDeg`° counter-clockwise, `rgb` packed `0xRRGGBB`,
   * `opacity` 0–1. Pair with {@link GigaPdfEngine.helveticaWidth} for centring.
   */
  addWatermark(
    page: number,
    x: number,
    y: number,
    size: number,
    text: string,
    rgb = 0x808080,
    opacity = 0.25,
    rotationDeg = 0
  ): boolean {
    return (
      this.g._withStr(text, (p, l) =>
        this.ex().gp_add_watermark(this.h, page, x, y, size, p, l, RGB(rgb), opacity, rotationDeg)
      ) === 0
    );
  }
  neededFonts(): string[] {
    return this.g._json((o) => this.ex().gp_needed_fonts(this.h, o));
  }
  /**
   * Extract an embedded font program by (fuzzy) `/BaseFont` name — so a host
   * editor can re-embed the document's own font when re-baking edited text and
   * keep the original glyphs. Returns the raw decoded bytes and the program
   * format (`truetype` embeds directly; `cff`/`type1` need a TTF conversion),
   * or `null` when nothing embedded matches.
   */
  extractFont(
    name: string
  ): { format: "truetype" | "cff" | "type1"; bytes: Uint8Array } | null {
    const buf = this.g._withStr(name, (p, l) =>
      this.g._buffer((o) => this.ex().gp_extract_font(this.h, p, l, o))
    );
    if (buf.length === 0) return null;
    const format = buf[0] === 1 ? "truetype" : buf[0] === 2 ? "cff" : "type1";
    return { format, bytes: buf.subarray(1) };
  }
  /**
   * The fonts **embedded** in the document — each `{ baseFont, format }`. Pair
   * with {@link extractFont} to pull a font's bytes out and re-embed them via
   * {@link embedFont}, e.g. to draw new text (with {@link addText}) in a face
   * the document already carries — no external font file needed.
   */
  embeddedFonts(): EmbeddedFont[] {
    return this.g._json((o) => this.ex().gp_embedded_fonts_json(this.h, o));
  }

  // convert PDF → X
  toText(): string {
    return this.g._str((o) => this.ex().gp_to_text(this.h, o));
  }
  toHtml(): string {
    return this.g._str((o) => this.ex().gp_to_html(this.h, o));
  }
  /**
   * Reconstruct this PDF into the **unified editable model** — the
   * format-neutral {@link GigaDocument} tree (sections → pages → blocks → runs)
   * that every format lowers into. Edit it with
   * {@link GigaPdfEngine.applyModelOps}, then export it to any target with
   * {@link GigaPdfEngine.modelToDocx} / {@link GigaPdfEngine.modelToPdf} / … —
   * the foundation for editing any document indifferently of its source format.
   */
  toModel(): GigaDocument {
    return JSON.parse(this.g._str((o) => this.ex().gp_model_from_pdf(this.h, o))) as GigaDocument;
  }
  toDocx(): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_to_docx(this.h, o));
  }
  toPptx(): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_to_pptx(this.h, o));
  }
  /** Convert to an editable OpenDocument Presentation (`.odp`). */
  toOdp(): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_to_odp(this.h, o));
  }
  toOdt(): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_to_odt(this.h, o));
  }
  toXlsx(): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_to_xlsx(this.h, o));
  }
  toOds(): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_to_ods(this.h, o));
  }
  toRtf(): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_to_rtf(this.h, o));
  }
  /**
   * Re-serialize the document as **PDF/A** (archival).
   *
   * `level` selects the conformance flavour (default `"pdfa-2b"`):
   * - `"pdfa-1b"` — ISO 19005-1, based on PDF 1.4.
   * - `"pdfa-1a"` — ISO 19005-1 **Tagged PDF** (level A): 1b + a logical
   *   structure tree, marked content and `/Lang`. Built from the structure the
   *   engine reconstructs (paragraphs, headings, tables, lists).
   * - `"pdfa-2b"` — ISO 19005-2 (default).
   * - `"pdfa-2u"` — like 2b but requires every glyph Unicode-mapped (a
   *   `/ToUnicode` CMap on each font); only validator-clean when the source
   *   fonts already carry one — otherwise prefer `"pdfa-2b"`.
   * - `"pdfa-2a"` — ISO 19005-2 **Tagged PDF** (level A): 2u + a logical
   *   structure tree and marked content (`/StructTreeRoot`, `/MarkInfo`).
   * - `"pdfa-3b"` — ISO 19005-3; permits embedded file attachments.
   *
   * Full conformance also requires every font embedded — this method does not
   * embed missing fonts (see the Rust `to_pdfa_level` docs). For the level-A
   * (`1a`/`2a`) flavours the tagging is rendering-neutral: it only inserts
   * balanced marked-content operators and a structure tree, never changing the
   * page appearance.
   */
  toPdfA(
    level:
      | "pdfa-1b"
      | "pdfa-1a"
      | "pdfa-2b"
      | "pdfa-2u"
      | "pdfa-2a"
      | "pdfa-3b" = "pdfa-2b",
  ): Uint8Array {
    return this.g._withStr(level, (p, l) =>
      this.g._buffer((o) => this.ex().gp_to_pdfa(this.h, p, l, o)),
    );
  }

  /**
   * Author a **tagged (accessible) PDF** — a `/StructTreeRoot` logical-structure
   * tree (`P`/`H1`–`H6`/`Table`/`L`/`Figure` …) with marked content, `/MarkInfo`,
   * `/Lang`, `/RoleMap` and `/Alt` on figures — **without** forcing PDF/A. Pass
   * `{ pdfUa: true }` to also stamp the PDF/UA-1 identifier (ISO 14289). If the
   * document has no reconstructable structure the plain PDF is returned.
   * ISO 32000-1 §14.7/§14.8.
   */
  toTagged(opts: { pdfUa?: boolean } = {}): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_to_tagged(this.h, opts.pdfUa ? 1 : 0, o));
  }

  // security
  /**
   * Serialize the document encrypted with the PDF Standard Security Handler.
   * Defaults to **AES-256 (R6)**. `fileId` is the document `/ID` (any stable
   * hex/string). For AES-256 a **secret 32-byte key** is required — it is taken
   * from `opts.keySeed` or generated with Web Crypto; RC4/AES-128 derive their
   * key from the password and ignore it.
   */
  saveEncrypted(
    password: string,
    fileId: string,
    opts: {
      ownerPassword?: string;
      algorithm?: "rc4" | "aes128" | "aes256";
      /**
       * Named access permissions (ISO 32000-1 Table 22). Omitted flags default
       * to **granted**. Takes precedence over `permissions` when present.
       */
      flags?: Partial<PdfPermissions>;
      /**
       * Raw signed 32-bit `/P` bitmask. Use `flags` (above) for a readable API.
       * Defaults to all permissions granted when neither is given.
       */
      permissions?: number;
      keySeed?: Uint8Array;
    } = {}
  ): Uint8Array {
    const algo = opts.algorithm ?? "aes256";
    const algoCode = algo === "rc4" ? 0 : algo === "aes128" ? 1 : 2;
    // Precedence: explicit `flags` → packed /P; else raw `permissions`; else
    // the unrestricted spec-strict baseline (`/P` = -196, all eight granted).
    const permissions =
      opts.flags !== undefined
        ? this.g.permissionsToP(opts.flags)
        : opts.permissions ?? this.g.permissionsToP();
    let key = opts.keySeed ?? new Uint8Array(0);
    if (algoCode === 2 && key.length < 32) {
      const c = (globalThis as { crypto?: Crypto }).crypto;
      if (!c?.getRandomValues) {
        throw new Error(
          "AES-256 encryption needs Web Crypto (globalThis.crypto.getRandomValues) or an explicit opts.keySeed"
        );
      }
      // `getRandomValues` requires an ArrayBuffer-backed view (not ArrayBufferLike).
      const fresh = new Uint8Array(32);
      c.getRandomValues(fresh);
      key = fresh;
    }
    return this.g._withStr(password, (pwP, pwL) =>
      this.g._withOptStr(opts.ownerPassword, (oP, oL) =>
        this.g._withStr(fileId, (idP, idL) =>
          this.g._withBytes(key, (kP, kL) =>
            this.g._buffer((o) =>
              this.ex().gp_save_encrypted(
                this.h,
                pwP,
                pwL,
                oP,
                oL,
                idP,
                idL,
                kP,
                kL,
                algoCode,
                permissions,
                o
              )
            )
          )
        )
      )
    );
  }

  /**
   * Re-encrypt an already-opened (decrypted) document with **new** passwords,
   * discarding the prior encryption (ISO 32000-1 §7.6). Opening with the old
   * password is what authorises the change. Mirrors {@link saveEncrypted} plus
   * `encryptMetadata` (when `false`, the metadata stream is left in the clear).
   */
  changePasswords(
    newPassword: string,
    fileId: string,
    opts: {
      ownerPassword?: string;
      algorithm?: "rc4" | "aes128" | "aes256";
      flags?: Partial<PdfPermissions>;
      permissions?: number;
      keySeed?: Uint8Array;
      encryptMetadata?: boolean;
    } = {}
  ): Uint8Array {
    const algo = opts.algorithm ?? "aes256";
    const algoCode = algo === "rc4" ? 0 : algo === "aes128" ? 1 : 2;
    const permissions =
      opts.flags !== undefined
        ? this.g.permissionsToP(opts.flags)
        : opts.permissions ?? this.g.permissionsToP();
    let key = opts.keySeed ?? new Uint8Array(0);
    if (algoCode === 2 && key.length < 32) {
      key = cryptoRandom(32);
    }
    const encMeta = opts.encryptMetadata ?? true;
    return this.g._withStr(newPassword, (pwP, pwL) =>
      this.g._withOptStr(opts.ownerPassword, (oP, oL) =>
        this.g._withStr(fileId, (idP, idL) =>
          this.g._withBytes(key, (kP, kL) =>
            this.g._buffer((o) =>
              this.ex().gp_change_passwords(
                this.h, pwP, pwL, oP, oL, idP, idL, kP, kL, algoCode, permissions,
                encMeta ? 1 : 0, o
              )
            )
          )
        )
      )
    );
  }

  /**
   * Strip encryption from an already-opened (decrypted) document, returning a
   * **plaintext** PDF. The document must have been opened with a valid password.
   */
  removeEncryption(): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_remove_encryption(this.h, o));
  }

  /**
   * Encrypt the document to one or more **X.509 recipients** (public-key /
   * certificate security, ISO 32000-1 §7.6.5, `/Filter /Adobe.PubSec`): only a
   * holder of a recipient private key can open it — no shared password.
   * `certificates` are DER X.509 certs. `seed`/`rngSeed` default to Web Crypto
   * randomness. Open the result with {@link GigaPdfEngine.openWithPrivateKey}.
   */
  encryptForRecipients(
    certificates: Uint8Array[],
    opts: {
      flags?: Partial<PdfPermissions>;
      permissions?: number;
      aes256?: boolean;
      encryptMetadata?: boolean;
      seed?: Uint8Array;
      rngSeed?: Uint8Array;
    } = {}
  ): Uint8Array {
    if (certificates.length === 0) {
      throw new Error("encryptForRecipients needs at least one recipient certificate");
    }
    const total = certificates.reduce((n, c) => n + c.length, 0);
    const blob = new Uint8Array(total);
    const lens: number[] = [];
    let off = 0;
    for (const c of certificates) {
      blob.set(c, off);
      off += c.length;
      lens.push(c.length);
    }
    const permissions =
      opts.flags !== undefined
        ? this.g.permissionsToP(opts.flags)
        : opts.permissions ?? this.g.permissionsToP();
    const aes256 = opts.aes256 ?? true;
    const encMeta = opts.encryptMetadata ?? true;
    const seed = opts.seed ?? cryptoRandom(20);
    const rng = opts.rngSeed ?? cryptoRandom(32);
    const out = this.g._withBytes(blob, (cP, cL) =>
      this.g._withU32(lens, (lP, lC) =>
        this.g._withBytes(seed, (sP, sL) =>
          this.g._withBytes(rng, (rP, rL) =>
            this.g._buffer((o) =>
              this.ex().gp_encrypt_for_recipients(
                this.h, cP, cL, lP, lC, permissions, aes256 ? 1 : 0, encMeta ? 1 : 0,
                sP, sL, rP, rL, o
              )
            )
          )
        )
      )
    );
    if (out.length === 0) {
      throw new Error("encryptForRecipients failed (invalid recipient certificate?)");
    }
    return out;
  }

  /** Self-signed digital signature. `random` ≥ 256 bytes from crypto.getRandomValues. */
  sign(fields: string, random: Uint8Array, keyBits = 2048): Uint8Array {
    const rPtr = this.g._toWasm(random);
    const out = this.g._withStr(fields, (fP, fL) =>
      this.g._buffer((o) => this.ex().gp_sign(this.h, fP, fL, rPtr, random.length, keyBits, o))
    );
    this.g._free(rPtr, random.length);
    return out;
  }

  /**
   * **Certify** the document (DocMDP) — like {@link sign} but also declares which
   * later changes are allowed: `docmdpLevel` is `1` (no changes), `2` (form-fill
   * + sign) or `3` (also annotate). `fields` is the same tab-separated string as
   * {@link sign} (`name⇥reason⇥date⇥notBefore⇥notAfter`).
   */
  certify(fields: string, random: Uint8Array, docmdpLevel: 1 | 2 | 3, keyBits = 2048): Uint8Array {
    const rPtr = this.g._toWasm(random);
    const out = this.g._withStr(fields, (fP, fL) =>
      this.g._buffer((o) =>
        this.ex().gp_sign_certify(this.h, fP, fL, rPtr, random.length, keyBits, docmdpLevel, o)
      )
    );
    this.g._free(rPtr, random.length);
    return out;
  }

  /**
   * List every signature on the document (each `/Sig` field's `/V`) with its
   * metadata and `/ByteRange`. For cryptographic validity call
   * {@link verifySignatures}.
   */
  signatures(): SignatureInfo[] {
    return this.g._json((o) => this.ex().gp_signatures_json(this.h, o));
  }

  /**
   * Cryptographically verify every signature against `pdf` — the **original
   * bytes** this document was opened from. Each report carries `byteRangeOk`,
   * `digestOk` (content integrity), `signatureOk` (the RSA signature),
   * `coversWholeDocument`, the signer CN, and the algorithm.
   */
  verifySignatures(pdf: Uint8Array): SignatureReport[] {
    return this.g._withBytes(pdf, (p, l) =>
      this.g._json((o) => this.ex().gp_verify_signatures(this.h, p, l, o))
    );
  }
  /**
   * Sign with a PKCS#12 (`.p12`/`.pfx`) identity — a CA-issued / eIDAS
   * certificate and its RSA key, imported natively (no external crypto). `opts`
   * populates the signature dictionary: `name` (`/Name`), `reason` (`/Reason`),
   * `date` (`/M`, a PDF date string e.g. `D:20260616120000Z`), and the optional
   * `location` (`/Location`) and `contactInfo` (`/ContactInfo`). Throws a single
   * generic error on a wrong password, malformed file, unsupported cipher, or
   * missing certificate.
   */
  signP12(p12: Uint8Array, password: string, opts: SignP12Options = {}): Uint8Array {
    const fields = [
      opts.name ?? "",
      opts.reason ?? "",
      opts.date ?? "",
      opts.location ?? "",
      opts.contactInfo ?? "",
    ].join("\t");
    const p12Ptr = this.g._toWasm(p12);
    const out = this.g._withStr(password, (pwP, pwL) =>
      this.g._withStr(fields, (fP, fL) =>
        this.g._buffer((o) =>
          this.ex().gp_sign_p12(this.h, p12Ptr, p12.length, pwP, pwL, fP, fL, o)
        )
      )
    );
    this.g._free(p12Ptr, p12.length);
    if (out.length === 0) {
      throw new Error(
        "PKCS#12 signing failed: invalid certificate, password, or unsupported file"
      );
    }
    return out;
  }
  /**
   * Sign with an embedded **RFC 3161 trusted timestamp** (PAdES-B-T). Unlike
   * {@link sign}/{@link signP12} this is `async`: the timestamp requires a
   * network round trip to a TSA, so the method runs the engine's two-phase flow
   * — build the signature → POST the `TimeStampReq` to the TSA → embed the
   * returned token — with the HTTP in between.
   *
   * The signing identity is `opts.p12` (+ `opts.password`) when supplied, else a
   * generated self-signed digital ID from `opts.random` (+ `notBefore`/
   * `notAfter`). `opts.tsaUrl` is the TSA endpoint (e.g. FreeTSA
   * `https://freetsa.org/tsr`); pass `opts.tsaFetch` to customise the request
   * (auth, proxy, SSRF allow-list). Throws a single generic error on any failure
   * (bad identity, TSA HTTP error, malformed response, or signature too large).
   */
  async signTimestamped(opts: SignTsaOptions): Promise<Uint8Array> {
    const usingP12 = opts.p12 != null && opts.p12.length > 0;
    if (!usingP12 && (opts.random == null || opts.random.length < 256)) {
      throw new Error(
        "signTimestamped: self-signed path needs `random` ≥ 256 bytes (or pass a `p12`)"
      );
    }
    const fields = [
      opts.name ?? "",
      opts.reason ?? "",
      opts.date ?? "",
      opts.location ?? "",
      opts.contactInfo ?? "",
      opts.notBefore ?? "",
      opts.notAfter ?? "",
    ].join("\t");

    const rand = opts.random ?? new Uint8Array(0);
    const p12 = opts.p12 ?? new Uint8Array(0);
    const nonce = opts.nonce ?? new Uint8Array(0);
    const keyBits = opts.keyBits ?? 2048;

    // Phase 1: build the signature, get the TimeStampReq to POST.
    const rPtr = this.g._toWasm(rand);
    const p12Ptr = this.g._toWasm(p12);
    const noncePtr = this.g._toWasm(nonce);
    let request: Uint8Array;
    try {
      request = this.g._withStr(opts.password ?? "", (pwP, pwL) =>
        this.g._withStr(fields, (fP, fL) =>
          this.g._buffer((o) =>
            this.ex().gp_sign_prepare_tsa(
              this.h,
              fP,
              fL,
              rPtr,
              rand.length,
              keyBits,
              p12Ptr,
              p12.length,
              pwP,
              pwL,
              noncePtr,
              nonce.length,
              o
            )
          )
        )
      );
    } finally {
      this.g._free(rPtr, rand.length);
      this.g._free(p12Ptr, p12.length);
      this.g._free(noncePtr, nonce.length);
    }
    if (request.length === 0) {
      throw new Error(
        "timestamped signing failed: invalid identity or could not build the timestamp request"
      );
    }

    // Host round trip: POST the request to the TSA, read the response.
    const response = opts.tsaFetch
      ? await opts.tsaFetch(request, opts.tsaUrl)
      : await defaultTsaPost(opts.tsaUrl, request);

    // Phase 2: embed the timestamp token, finalize the signed PDF.
    const tokenPtr = this.g._toWasm(response);
    let out: Uint8Array;
    try {
      out = this.g._buffer((o) =>
        this.ex().gp_sign_finish_tsa(this.h, tokenPtr, response.length, o)
      );
    } finally {
      this.g._free(tokenPtr, response.length);
    }
    if (out.length === 0) {
      throw new Error(
        "timestamped signing failed: TSA response rejected or signature too large for the reserved space"
      );
    }
    return out;
  }

  /**
   * Sign with **long-term validation** material embedded (PAdES-B-LT, or B-LTA
   * with `opts.archiveTimestamp`). Builds a B-T signature ({@link signTimestamped}),
   * then fetches the certificate chain's revocation material (OCSP responses /
   * CRLs, by URL **from the certificates** — the engine computes which URLs, the
   * host fetches) and stores it in a `/DSS`, so the signature validates long after
   * the certificates expire or are revoked. With `archiveTimestamp` a document
   * timestamp over the whole file (DSS included) is added for renewable archival.
   *
   * `async`, multi-round-trip: one TSA POST for the B-T timestamp, one OCSP/CRL
   * fetch per chain certificate, and (if `archiveTimestamp`) a second TSA POST.
   * Unreachable responders are skipped — the DSS is built from whatever resolves;
   * a self-signed identity (no AIA/CRL-DP) simply yields a `/DSS/Certs`-only store.
   * Override `tsaFetch`/`revocationFetch`/`crlFetch` to add auth, proxies, or an
   * SSRF allow-list. Throws on a fatal failure (bad identity, B-T signature, or a
   * malformed PDF the DSS can't chain to).
   */
  async signLtv(opts: SignLtvOptions): Promise<Uint8Array> {
    // 1. The B-T signature is the foundation (its /Contents holds the chain).
    const signed = await this.signTimestamped(opts);

    // 2. Ask the engine which validation material to fetch (per certificate).
    const nonce = opts.nonce ?? new Uint8Array(0);
    const targetsJson = this.withSignedPdf(signed, (pdfPtr, pdfLen) => {
      const noncePtr = this.g._toWasm(nonce);
      try {
        return dec.decode(
          this.g._buffer((o) =>
            this.ex().gp_ltv_targets(pdfPtr, pdfLen, noncePtr, nonce.length, o)
          )
        );
      } finally {
        this.g._free(noncePtr, nonce.length);
      }
    });
    const targets: LtvTarget[] = targetsJson ? JSON.parse(targetsJson) : [];

    // 3. Host fetches: chain certs, plus OCSP/CRL per source (best-effort).
    const certs: Uint8Array[] = [];
    const ocsps: Uint8Array[] = [];
    const crls: Uint8Array[] = [];
    const ocspPost = opts.revocationFetch ?? defaultOcspPost;
    const crlGet = opts.crlFetch ?? defaultCrlGet;
    for (const target of targets) {
      certs.push(hexToBytes(target.certHex));
      for (const source of target.sources) {
        try {
          if (source.kind === "ocsp") {
            ocsps.push(await ocspPost(hexToBytes(source.requestHex), source.url));
          } else {
            crls.push(await crlGet(source.url));
          }
        } catch {
          // An unreachable responder is non-fatal: the DSS embeds what succeeds.
        }
      }
    }

    // 4. Embed the material in a /DSS (incremental update → B-LT).
    let lt = this.applyDss(signed, certs, ocsps, crls);

    // 5. Optional B-LTA: a document timestamp over the whole file (DSS included).
    if (opts.archiveTimestamp) {
      lt = await this.appendDocumentTimestamp(lt, opts);
    }
    return lt;
  }

  /** Run `fn` with `pdf` copied into wasm memory, freeing it afterwards. */
  private withSignedPdf<T>(pdf: Uint8Array, fn: (ptr: number, len: number) => T): T {
    const ptr = this.g._toWasm(pdf);
    try {
      return fn(ptr, pdf.length);
    } finally {
      this.g._free(ptr, pdf.length);
    }
  }

  /** Add the `/DSS` to `signed` as an incremental update (B-LT). */
  private applyDss(
    signed: Uint8Array,
    certs: Uint8Array[],
    ocsps: Uint8Array[],
    crls: Uint8Array[]
  ): Uint8Array {
    const certsBuf = frameBlobs(certs);
    const ocspsBuf = frameBlobs(ocsps);
    const crlsBuf = frameBlobs(crls);
    const pdfPtr = this.g._toWasm(signed);
    const cPtr = this.g._toWasm(certsBuf);
    const oPtr = this.g._toWasm(ocspsBuf);
    const rPtr = this.g._toWasm(crlsBuf);
    let out: Uint8Array;
    try {
      out = this.g._buffer((res) =>
        this.ex().gp_apply_dss(
          pdfPtr,
          signed.length,
          cPtr,
          certsBuf.length,
          oPtr,
          ocspsBuf.length,
          rPtr,
          crlsBuf.length,
          res
        )
      );
    } finally {
      this.g._free(pdfPtr, signed.length);
      this.g._free(cPtr, certsBuf.length);
      this.g._free(oPtr, ocspsBuf.length);
      this.g._free(rPtr, crlsBuf.length);
    }
    if (out.length === 0) {
      throw new Error("LTV failed: could not add the /DSS to the signed PDF");
    }
    return out;
  }

  /** Append a document timestamp over the whole `pdf` (B-LTA), TSA round trip
   * included. */
  private async appendDocumentTimestamp(
    pdf: Uint8Array,
    opts: SignLtvOptions
  ): Promise<Uint8Array> {
    const nonce = opts.nonce ?? new Uint8Array(0);
    // Phase 1: build the timestamp shell, get the request.
    const pdfPtr = this.g._toWasm(pdf);
    const noncePtr = this.g._toWasm(nonce);
    let request: Uint8Array;
    try {
      request = this.g._buffer((o) =>
        this.ex().gp_doc_timestamp_prepare(this.h, pdfPtr, pdf.length, noncePtr, nonce.length, o)
      );
    } finally {
      this.g._free(pdfPtr, pdf.length);
      this.g._free(noncePtr, nonce.length);
    }
    if (request.length === 0) {
      throw new Error("LTV archive timestamp failed: could not build the timestamp request");
    }

    // Host round trip: POST the request to the TSA.
    const response = opts.tsaFetch
      ? await opts.tsaFetch(request, opts.tsaUrl)
      : await defaultTsaPost(opts.tsaUrl, request);

    // Phase 2: embed the token, finalize B-LTA.
    const tokenPtr = this.g._toWasm(response);
    let out: Uint8Array;
    try {
      out = this.g._buffer((o) =>
        this.ex().gp_doc_timestamp_finish(this.h, tokenPtr, response.length, o)
      );
    } finally {
      this.g._free(tokenPtr, response.length);
    }
    if (out.length === 0) {
      throw new Error(
        "LTV archive timestamp failed: TSA response rejected or token too large for the reserved space"
      );
    }
    return out;
  }

  // metadata
  getMetadata(key: string): string {
    return this.g._withStr(key, (p, l) =>
      this.g._str((o) => this.ex().gp_get_metadata(this.h, p, l, o))
    );
  }
  /**
   * Set a **single** Info-dictionary entry (e.g. `"Title"`, `"Author"`). This
   * touches only `/Info`; use {@link setInfo} to update the typed fields and keep
   * the XMP `/Metadata` packet in sync.
   */
  setMetadata(key: string, value: string): boolean {
    return (
      this.g._withStr(key, (kP, kL) =>
        this.g._withStr(value, (vP, vL) => this.ex().gp_set_metadata(this.h, kP, kL, vP, vL))
      ) === 0
    );
  }

  /**
   * The document's XMP metadata packet (catalog `/Metadata`, ISO 32000-1 §14.3.2)
   * as raw bytes, or `null` when the document carries no XMP.
   */
  getXmp(): Uint8Array | null {
    const bytes = this.g._buffer((o) => this.ex().gp_get_xmp(this.h, o));
    return bytes.length === 0 ? null : bytes;
  }

  /**
   * Replace (or create) the document's XMP metadata stream (catalog `/Metadata`,
   * stored uncompressed). Accepts a UTF-8 string or raw bytes. Returns `true` on
   * success.
   */
  setXmp(xmp: Uint8Array | string): boolean {
    const bytes = typeof xmp === "string" ? new TextEncoder().encode(xmp) : xmp;
    return this.g._withBytes(bytes, (p, l) => this.ex().gp_set_xmp(this.h, p, l)) === 0;
  }

  /**
   * Set the standard document-information fields, writing **both** the `/Info`
   * dictionary and a synced XMP `/Metadata` packet so the two never drift. This is
   * a **partial** update — only the fields you provide are changed; omit a field
   * to leave it untouched. Returns `true` on success.
   *
   * @example
   * doc.setInfo({ title: "Annual Report", author: "Ada Lovelace", keywords: "finance, 2026" });
   */
  setInfo(fields: InfoFields): boolean {
    return (
      this.g._withStr(JSON.stringify(fields), (p, l) =>
        this.ex().gp_set_info_json(this.h, p, l)
      ) === 0
    );
  }

  // annotations (Acrobat-style markup)
  annotations(page: number): AnnotationInfo[] {
    return this.g._json((o) => this.ex().gp_annotations_json(this.h, page, o));
  }
  removeAnnotation(page: number, index: number): boolean {
    return this.ex().gp_remove_annotation(this.h, page, index) === 0;
  }
  addSquare(
    page: number,
    x0: number,
    y0: number,
    x1: number,
    y1: number,
    stroke: number | null = 0,
    fill: number | null = null,
    lineWidth = 1
  ): boolean {
    return (
      this.ex().gp_add_square(
        this.h,
        page,
        x0,
        y0,
        x1,
        y1,
        RGB(stroke ?? 0),
        stroke === null ? 0 : 1,
        RGB(fill ?? 0),
        fill === null ? 0 : 1,
        lineWidth
      ) === 0
    );
  }
  addHighlight(page: number, x0: number, y0: number, x1: number, y1: number, rgb = 0xffff00): boolean {
    return this.ex().gp_add_highlight(this.h, page, x0, y0, x1, y1, RGB(rgb)) === 0;
  }
  /**
   * Add a `/Line` annotation from `(x1,y1)` to `(x2,y2)`. When `endArrow` is
   * `true`, an open arrowhead (`/LE [/None /OpenArrow]`) is drawn at the
   * `(x2,y2)` end — a real, editable annotation in conforming readers, ideal
   * for callouts that point at content.
   */
  addLineAnnotation(
    page: number,
    x1: number,
    y1: number,
    x2: number,
    y2: number,
    rgb = 0,
    lineWidth = 1,
    endArrow = false
  ): boolean {
    return (
      this.ex().gp_add_line(this.h, page, x1, y1, x2, y2, RGB(rgb), lineWidth, endArrow ? 1 : 0) === 0
    );
  }
  addFreeText(
    page: number,
    x0: number,
    y0: number,
    x1: number,
    y1: number,
    text: string,
    fontSize = 12,
    rgb = 0
  ): boolean {
    return (
      this.g._withStr(text, (p, l) =>
        this.ex().gp_add_free_text(this.h, page, x0, y0, x1, y1, p, l, fontSize, RGB(rgb))
      ) === 0
    );
  }
  addUnderline(page: number, x0: number, y0: number, x1: number, y1: number, rgb = 0): boolean {
    return this.ex().gp_add_underline(this.h, page, x0, y0, x1, y1, RGB(rgb)) === 0;
  }
  addStrikeOut(page: number, x0: number, y0: number, x1: number, y1: number, rgb = 0): boolean {
    return this.ex().gp_add_strike_out(this.h, page, x0, y0, x1, y1, RGB(rgb)) === 0;
  }
  /**
   * Add a text-markup annotation (`highlight` | `underline` | `strikeout` |
   * `squiggly`) spanning one or more `quads` (each `[x0, y0, x1, y1]` in PDF
   * user space, bottom-left origin — multi-quad covers wrapped text), with full
   * reviewer metadata. `date` is a PDF date string (e.g. `"D:20260616T…Z"`) — the
   * engine has no clock, so the host supplies it.
   */
  addMarkupAnnotation(
    page: number,
    subtype: "highlight" | "underline" | "strikeout" | "squiggly",
    quads: Array<[number, number, number, number]>,
    rgb: number,
    opacity: number,
    meta: { contents?: string; author?: string; id?: string; date?: string } = {}
  ): boolean {
    const sub =
      subtype === "highlight"
        ? "Highlight"
        : subtype === "underline"
          ? "Underline"
          : subtype === "strikeout"
            ? "StrikeOut"
            : "Squiggly";
    const packed = [
      sub,
      meta.contents ?? "",
      meta.author ?? "",
      meta.id ?? "",
      meta.date ?? "",
    ].join("");
    const flat = quads.flat();
    return (
      this.g._withStr(packed, (mp, ml) =>
        this.g._withF64(flat, (qp, qc) =>
          this.ex().gp_add_markup_annotation(this.h, page, mp, ml, qp, qc, RGB(rgb), opacity)
        )
      ) === 0
    );
  }
  /**
   * Add a sticky-note (`/Text`) annotation: a badge at `rect` (`[x0,y0,x1,y1]`)
   * that opens a popup with `meta.contents`. `icon` is the named icon (`"Note"`,
   * `"Comment"`, …); `open` sets the initial popup state.
   */
  addTextNote(
    page: number,
    rect: [number, number, number, number],
    rgb: number,
    meta: { contents?: string; author?: string; id?: string; date?: string } = {},
    icon = "Note",
    open = false
  ): boolean {
    const packed = [
      meta.contents ?? "",
      meta.author ?? "",
      meta.id ?? "",
      meta.date ?? "",
    ].join("");
    return (
      this.g._withStr(packed, (mp, ml) =>
        this.g._withStr(icon, (ip, il) =>
          this.ex().gp_add_text_note(
            this.h,
            page,
            rect[0],
            rect[1],
            rect[2],
            rect[3],
            mp,
            ml,
            ip,
            il,
            open ? 1 : 0,
            RGB(rgb)
          )
        )
      ) === 0
    );
  }
  /** Freehand ink annotation from one polyline (`points` = flat [x0,y0,x1,y1,…]). */
  addInk(page: number, points: number[], rgb = 0, lineWidth = 1): boolean {
    return (
      this.g._withF64(points, (p, c) => this.ex().gp_add_ink(this.h, page, p, c, RGB(rgb), lineWidth)) ===
      0
    );
  }
  addStamp(
    page: number,
    x0: number,
    y0: number,
    x1: number,
    y1: number,
    label: string,
    rgb = 0xc00000
  ): boolean {
    return (
      this.g._withStr(label, (p, l) =>
        this.ex().gp_add_stamp(this.h, page, x0, y0, x1, y1, p, l, RGB(rgb))
      ) === 0
    );
  }

  /**
   * Add a `/Circle` (ellipse) annotation inscribed in `[x0,y0,x1,y1]`. `stroke`
   * (border) and `fill` (interior) are packed `0xRRGGBB` numbers, or `null` to
   * omit. Returns `true` on success.
   */
  addCircleAnnotation(
    page: number,
    x0: number,
    y0: number,
    x1: number,
    y1: number,
    stroke: number | null = 0,
    fill: number | null = null,
    lineWidth = 1
  ): boolean {
    return (
      this.ex().gp_add_circle_annot(
        this.h,
        page,
        x0,
        y0,
        x1,
        y1,
        RGB(stroke ?? 0),
        stroke === null ? 0 : 1,
        RGB(fill ?? 0),
        fill === null ? 0 : 1,
        lineWidth
      ) === 0
    );
  }

  /**
   * Add a `/Polygon` annotation — a closed shape through `points` (a flat
   * `[x0, y0, x1, y1, …]` array, PDF user space), with optional `stroke`/`fill`
   * (`0xRRGGBB` or `null`). Returns `true` on success.
   */
  addPolygonAnnotation(
    page: number,
    points: number[],
    stroke: number | null = 0,
    fill: number | null = null,
    lineWidth = 1
  ): boolean {
    return (
      this.g._withF64(points, (p, c) =>
        this.ex().gp_add_polygon_annot(
          this.h,
          page,
          p,
          c,
          RGB(stroke ?? 0),
          stroke === null ? 0 : 1,
          RGB(fill ?? 0),
          fill === null ? 0 : 1,
          lineWidth
        )
      ) === 0
    );
  }

  /**
   * Add a `/PolyLine` annotation — an open path through `points` (a flat
   * `[x0, y0, x1, y1, …]` array). `rgb` is packed `0xRRGGBB`.
   */
  addPolylineAnnotation(page: number, points: number[], rgb = 0, lineWidth = 1): boolean {
    return (
      this.g._withF64(points, (p, c) =>
        this.ex().gp_add_polyline_annot(this.h, page, p, c, RGB(rgb), lineWidth)
      ) === 0
    );
  }

  /**
   * Add a `/Caret` annotation — a small upward wedge in `[x0,y0,x1,y1]` marking
   * an insertion point. `rgb` is packed `0xRRGGBB`.
   */
  addCaretAnnotation(page: number, x0: number, y0: number, x1: number, y1: number, rgb = 0): boolean {
    return this.ex().gp_add_caret_annot(this.h, page, x0, y0, x1, y1, RGB(rgb)) === 0;
  }

  /**
   * Regenerate the appearance stream (`/AP /N`) of the 0-based `index`
   * annotation on `page` from its stored geometry, after editing its colour,
   * border or geometry. Returns `true` on success, `false` for a bad index or a
   * subtype whose appearance can't be reconstructed (FreeText/Stamp/Text/Link).
   */
  regenerateAppearance(page: number, index: number): boolean {
    return this.ex().gp_regenerate_appearance(this.h, page, index) === 0;
  }

  flattenAnnotations(page: number): number {
    return this.ex().gp_flatten_annotations(this.h, page);
  }
  /**
   * Flatten the interactive form: bake every field widget across all pages into
   * the page content and drop `/AcroForm`, so the document is no longer
   * fillable and {@link fields} returns empty afterwards. Returns the number of
   * widgets baked (0 when there is no form).
   */
  flattenForm(): number {
    return this.ex().gp_flatten_form(this.h);
  }
  /**
   * Inline a page's form XObjects (`/Subtype /Form` invoked via `Do`) into its
   * content stream, **de-sharing** each placement so the former form text/graphics
   * become ordinary page content with real, editable text-run indices (no form
   * sentinel) — the enabler for editing invoice/template text in place via
   * {@link replaceText} / {@link moveElement} / {@link removeElement} instead of
   * the redact+add overlay. Image XObjects are left untouched. Returns the number
   * of form XObjects inlined (every `Do` invocation, since each is de-shared).
   *
   * Distinct from {@link flattenForm}, which flattens **AcroForm** interactive
   * fields (and drops `/AcroForm`); this flattens reusable Form XObjects.
   */
  flattenFormXObjects(page: number): number {
    return this.ex().gp_flatten_form_xobjects(this.h, page);
  }

  // hyperlinks
  links(page: number): LinkInfo[] {
    return this.g._json((o) => this.ex().gp_links_json(this.h, page, o));
  }
  addUriLink(page: number, x0: number, y0: number, x1: number, y1: number, uri: string): boolean {
    return (
      this.g._withStr(uri, (p, l) => this.ex().gp_add_uri_link(this.h, page, x0, y0, x1, y1, p, l)) ===
      0
    );
  }
  addGotoLink(
    page: number,
    x0: number,
    y0: number,
    x1: number,
    y1: number,
    targetPage: number
  ): boolean {
    return this.ex().gp_add_goto_link(this.h, page, x0, y0, x1, y1, targetPage) === 0;
  }
  /**
   * Register a named destination `name` → `targetPage` (a whole-page `/Fit`
   * view) in the catalog. Links and bookmarks can then jump by name via
   * {@link addGotoLinkNamed}; because resolution goes through the catalog (not a
   * frozen page number), the anchor survives page extraction/split as long as
   * its page is kept. Re-using a `name` overwrites its target.
   */
  addNamedDest(name: string, targetPage: number): boolean {
    return (
      this.g._withStr(name, (p, l) => this.ex().gp_add_named_dest(this.h, p, l, targetPage)) === 0
    );
  }
  /** The catalog's named destinations as `{name, page}` pairs. */
  namedDests(): NamedDest[] {
    return this.g._json((o) => this.ex().gp_named_dests_json(this.h, o));
  }
  /**
   * Every embedded file attachment in the document's `/Names /EmbeddedFiles`
   * name tree, decoded. Each {@link Attachment} carries the name-tree key, the
   * filespec display name (`/UF`/`/F`), the embedded stream's MIME (`/Subtype`)
   * and `/Params` dates, and the decoded bytes. Entries that don't resolve to a
   * readable embedded stream are skipped, so the result is only extractable
   * files (the native replacement for a reader's `getAttachments()`).
   */
  attachments(): Attachment[] {
    const raw = this.g._json<Array<Omit<Attachment, 'data'> & { dataBase64: string }>>((o) =>
      this.ex().gp_attachments_json(this.h, o)
    );
    return raw.map(({ dataBase64, ...rest }) => ({
      ...rest,
      data: this.g._fromBase64(dataBase64),
    }));
  }

  /**
   * Embed `bytes` as a document-level file attachment named `name`
   * (`/Names /EmbeddedFiles`, ISO 32000-1 §7.11.4). Re-using a `name` **replaces**
   * that attachment; the bytes are stored FlateDecode-compressed. Returns `true`
   * on success (`false` e.g. for an empty name).
   */
  addAttachment(name: string, bytes: Uint8Array, opts: AttachmentOptions = {}): boolean {
    return (
      this.g._withStr(name, (np, nl) =>
        this.g._withBytes(bytes, (bp, bl) =>
          this.g._withOptStr(opts.mime ?? "", (mp, ml) =>
            this.g._withOptStr(opts.description ?? "", (dp, dl) =>
              this.ex().gp_add_attachment(this.h, np, nl, bp, bl, mp, ml, dp, dl)
            )
          )
        )
      ) === 0
    );
  }

  /**
   * Embed `bytes` as an **associated file** (`/AF`, PDF/A-3) named `name` with the
   * given {@link AfRelationship} — the mechanism Factur-X / ZUGFeRD / Order-X use
   * to carry their invoice XML (`"alternative"`). The file is also a normal
   * attachment, is linked from the catalog `/AF` array, and its filespec carries
   * `/AFRelationship`. Returns `true` on success.
   */
  addAssociatedFile(
    name: string,
    bytes: Uint8Array,
    relationship: AfRelationship,
    opts: AttachmentOptions = {}
  ): boolean {
    const rel = AF_RELATIONSHIP_CODE[relationship] ?? AF_RELATIONSHIP_CODE.unspecified;
    return (
      this.g._withStr(name, (np, nl) =>
        this.g._withBytes(bytes, (bp, bl) =>
          this.g._withOptStr(opts.mime ?? "", (mp, ml) =>
            this.g._withOptStr(opts.description ?? "", (dp, dl) =>
              this.ex().gp_add_associated_file(this.h, np, nl, bp, bl, mp, ml, dp, dl, rel)
            )
          )
        )
      ) === 0
    );
  }

  /**
   * Remove the attachment named `name` (from `/Names /EmbeddedFiles` and, if
   * present, the catalog `/AF` array). Returns `true` if one was removed, `false`
   * if no attachment had that name.
   */
  removeAttachment(name: string): boolean {
    return this.g._withStr(name, (p, l) => this.ex().gp_remove_attachment(this.h, p, l)) === 1;
  }

  /**
   * Install a **document-level JavaScript** under the catalog `/Names /JavaScript`
   * name tree (ISO 32000-1 §7.7.3.4 + §12.6.4.16): a named JavaScript action
   * (`<< /S /JavaScript /JS … >>`) keyed by `name`. On open, a conforming viewer
   * runs every document-level script in **name (lexical) order** — this is where
   * form-calculation / validation helper libraries live (e.g. `AFNumber_Format`).
   * Re-using a `name` **replaces** that script; long sources are stored as a
   * FlateDecode stream, and sibling `/Names` subtrees (`/EmbeddedFiles`,
   * `/Dests`, …) are preserved. Returns `true` on success (`false` e.g. for an
   * empty name).
   */
  addDocumentJavascript(name: string, script: string): boolean {
    return (
      this.g._withStr(name, (np, nl) =>
        this.g._withStr(script, (sp, sl) =>
          this.ex().gp_add_document_javascript(this.h, np, nl, sp, sl)
        )
      ) === 0
    );
  }

  /**
   * Every document-level JavaScript as `{ name, script }` pairs, in name
   * (lexical) order — the read side of {@link addDocumentJavascript} (decodes
   * both a literal `/JS` string and a `/JS` stream).
   */
  documentJavascripts(): DocumentJavascript[] {
    return this.g._json((o) => this.ex().gp_document_javascripts_json(this.h, o));
  }

  /**
   * Remove the document-level JavaScript named `name` from `/Names /JavaScript`.
   * Returns `true` if one was removed, `false` if none had that name.
   */
  removeDocumentJavascript(name: string): boolean {
    return (
      this.g._withStr(name, (p, l) => this.ex().gp_remove_document_javascript(this.h, p, l)) === 1
    );
  }

  /**
   * Add a page-anchored **FileAttachment** annotation over `rect` on the 1-based
   * `page`, pointing at the already-embedded attachment `name` (add it first with
   * {@link addAttachment}). `icon` is the visual marker (default `"PushPin"`).
   * Returns `true` on success (`false` if no such attachment exists).
   */
  addFileAttachmentAnnot(
    page: number,
    rect: Box,
    name: string,
    icon: FileAttachmentIcon = "PushPin"
  ): boolean {
    return (
      this.g._withStr(name, (np, nl) =>
        this.g._withOptStr(icon, (ip, il) =>
          this.ex().gp_add_file_attachment_annot(
            this.h,
            page,
            rect.x,
            rect.y,
            rect.x + rect.w,
            rect.y + rect.h,
            np,
            nl,
            ip,
            il
          )
        )
      ) === 0
    );
  }

  /**
   * Add an internal hyperlink over a rectangle that jumps to the named
   * destination `name` (define it with {@link addNamedDest}). Unlike
   * {@link addGotoLink} (an explicit page), this stores `/Dest /name`, keeping
   * cross-references intact through split/extract.
   */
  addGotoLinkNamed(
    page: number,
    x0: number,
    y0: number,
    x1: number,
    y1: number,
    name: string
  ): boolean {
    return (
      this.g._withStr(name, (p, l) =>
        this.ex().gp_add_goto_link_named(this.h, page, x0, y0, x1, y1, p, l)
      ) === 0
    );
  }

  // optional-content layers (calques): list + show/hide + lock/unlock + remove
  layers(): LayerInfo[] {
    return this.g._json((o) => this.ex().gp_layers_json(this.h, o));
  }
  /** Create a new (visible, unlocked) layer; returns its id (0 on error). */
  addLayer(name: string): number {
    return this.g._withStr(name, (p, l) => this.ex().gp_add_layer(this.h, p, l));
  }
  setLayerVisibility(id: number, visible: boolean): boolean {
    return this.ex().gp_set_layer_visibility(this.h, id, visible ? 1 : 0) === 0;
  }
  setLayerLocked(id: number, locked: boolean): boolean {
    return this.ex().gp_set_layer_locked(this.h, id, locked ? 1 : 0) === 0;
  }
  removeLayer(id: number): boolean {
    return this.ex().gp_remove_layer(this.h, id) === 0;
  }
  /**
   * Begin an optional-content marked-content sequence on `page` for the layer
   * `ocg` (its id, from {@link addLayer}). Registers the OCG under the page's
   * `/Resources /Properties` and appends `/OC /OCn BDC` to the content stream;
   * every drawing call after it ({@link addText}, {@link addRectangle},
   * {@link addImage}, …) is gated on that layer's visibility until the matching
   * {@link endOptionalContent} appends `EMC`. Calls nest. Returns the `OCn`
   * property name (empty string on error).
   */
  beginOptionalContent(page: number, ocg: number): string {
    return this.g._str((o) =>
      this.ex().gp_begin_optional_content(this.h, page, ocg, o)
    );
  }
  /** End the innermost optional-content sequence on `page` (`EMC`). */
  endOptionalContent(page: number): boolean {
    return this.ex().gp_end_optional_content(this.h, page) === 0;
  }

  // outline (bookmarks)
  outline(): OutlineEntry[] {
    return this.g._json((o) => this.ex().gp_outline_json(this.h, o));
  }
  /** Replace the outline. Each entry: `{level, page?, title}` (page 0/undefined = no dest). */
  setOutline(entries: OutlineEntry[]): boolean {
    const text = entries.map((e) => `${e.level}\t${e.page ?? 0}\t${e.title}`).join("\n");
    return this.g._withStr(text, (p, l) => this.ex().gp_set_outline(this.h, p, l)) === 0;
  }

  /**
   * Add a `/Link` annotation over `rect` (`{ x, y, w, h }`) carrying any
   * {@link Action} — the full action & destination model (GoTo with every fit
   * mode, GoToR, URI, Named navigation, Launch, JavaScript, SubmitForm,
   * ResetForm). Returns `true` on success (`false` for a malformed action).
   *
   * @example
   * doc.addLink(1, { x: 72, y: 700, w: 120, h: 16 },
   *   { type: "goto", dest: { fit: "xyz", page: 4, top: 720, zoom: 1.5 } });
   */
  addLink(page: number, rect: Box, action: Action): boolean {
    return (
      this.g._withStr(JSON.stringify(action), (p, l) =>
        this.ex().gp_add_link(this.h, page, rect.x, rect.y, rect.x + rect.w, rect.y + rect.h, p, l)
      ) === 0
    );
  }

  /**
   * Set the document `/OpenAction` — the {@link Action} performed when the file
   * is opened. Returns `true` on success.
   */
  setOpenAction(action: Action): boolean {
    return (
      this.g._withStr(JSON.stringify(action), (p, l) =>
        this.ex().gp_set_open_action(this.h, p, l)
      ) === 0
    );
  }

  /**
   * Remove the `linkIndex`-th `/Link` annotation on `page` (links counted in
   * order, ignoring non-link annotations). Returns `true` if one was removed.
   */
  removeLink(page: number, linkIndex: number): boolean {
    return this.ex().gp_remove_link(this.h, page, linkIndex) === 1;
  }

  /**
   * Replace the outline with {@link Bookmark}s that may carry actions (a `goto`
   * action becomes a `/Dest`, any other action an `/A`). An empty array clears
   * the outline. Returns `true` on success.
   */
  setBookmarks(bookmarks: Bookmark[]): boolean {
    const text = bookmarks
      .map((b) => `${b.level}\t${b.title}\t${b.action ? JSON.stringify(b.action) : ""}`)
      .join("\n");
    return this.g._withOptStr(text, (p, l) => this.ex().gp_set_bookmarks(this.h, p, l)) === 0;
  }

  // catalog reading / UX hints (ViewerPreferences, PageLayout, PageMode)
  /**
   * Write the catalog `/ViewerPreferences` (ISO 32000-1 §12.2). Each omitted
   * field is left unchanged; a boolean sets the key. After applying, an empty
   * dictionary is removed. Returns `false` on an invalid `direction`.
   */
  setViewerPreferences(prefs: ViewerPreferences): boolean {
    // -1 = leave unchanged, 0 = false, 1 = true.
    const tri = (v: boolean | undefined): number => (v === undefined ? -1 : v ? 1 : 0);
    return (
      this.g._withStr(prefs.direction ?? "", (dP, dL) =>
        this.ex().gp_set_viewer_preferences(
          this.h,
          tri(prefs.hideToolbar),
          tri(prefs.hideMenubar),
          tri(prefs.hideWindowUI),
          tri(prefs.fitWindow),
          tri(prefs.centerWindow),
          tri(prefs.displayDocTitle),
          dP,
          dL
        )
      ) === 0
    );
  }
  /** Set the catalog `/PageLayout`. `null` removes the key. Returns `false` on an unknown name. */
  setPageLayout(layout: PageLayout | null): boolean {
    return this.g._withStr(layout ?? "", (p, l) => this.ex().gp_set_page_layout(this.h, p, l)) === 0;
  }
  /** Set the catalog `/PageMode`. `null` removes the key. Returns `false` on an unknown name. */
  setPageMode(mode: PageMode | null): boolean {
    return this.g._withStr(mode ?? "", (p, l) => this.ex().gp_set_page_mode(this.h, p, l)) === 0;
  }

  // interactive forms (AcroForm)
  fields(): FieldInfo[] {
    return this.g._json((o) => this.ex().gp_fields_json(this.h, o));
  }
  setTextField(name: string, value: string): boolean {
    return (
      this.g._withStr(name, (nP, nL) =>
        this.g._withStr(value, (vP, vL) => this.ex().gp_set_text_field(this.h, nP, nL, vP, vL))
      ) === 0
    );
  }
  setCheckbox(name: string, checked: boolean): boolean {
    return (
      this.g._withStr(name, (p, l) => this.ex().gp_set_checkbox(this.h, p, l, checked ? 1 : 0)) === 0
    );
  }
  setRadio(name: string, value: string): boolean {
    return (
      this.g._withStr(name, (nP, nL) =>
        this.g._withStr(value, (vP, vL) => this.ex().gp_set_radio(this.h, nP, nL, vP, vL))
      ) === 0
    );
  }
  /** Set a choice field's selection (multi-select list boxes accept several values). */
  setChoice(name: string, values: string[]): boolean {
    return (
      this.g._withStr(name, (nP, nL) =>
        this.g._withStr(values.join("\n"), (vP, vL) => this.ex().gp_set_choice(this.h, nP, nL, vP, vL))
      ) === 0
    );
  }

  // ── form field creation ──────────────────────────────────────────────────

  /**
   * Create a text field on `page` covering `rect` = `[x0, y0, x1, y1]` (PDF
   * user space). Options: `maxLen` character cap, `multiline`, `password`,
   * and visual `style`.
   */
  addTextField(
    page: number,
    name: string,
    rect: [number, number, number, number],
    value = "",
    opts: { maxLen?: number; multiline?: boolean; password?: boolean; style?: FieldStyle } = {}
  ): boolean {
    const st = styleArgs(opts.style);
    return (
      this.g._withStr(name, (nP, nL) =>
        this.g._withStr(value, (vP, vL) =>
          this.ex().gp_add_text_field(
            this.h, page, nP, nL, rect[0], rect[1], rect[2], rect[3], vP, vL,
            opts.maxLen ?? -1, opts.multiline ? 1 : 0, opts.password ? 1 : 0, ...st
          )
        )
      ) === 0
    );
  }

  /** Create a checkbox. `export` is the on-state name (default `On`). */
  addCheckbox(
    page: number,
    name: string,
    rect: [number, number, number, number],
    checked = false,
    opts: { export?: string; style?: FieldStyle } = {}
  ): boolean {
    const st = styleArgs(opts.style);
    return (
      this.g._withStr(name, (nP, nL) =>
        this.g._withStr(opts.export ?? "On", (eP, eL) =>
          this.ex().gp_add_checkbox(
            this.h, page, nP, nL, rect[0], rect[1], rect[2], rect[3], checked ? 1 : 0, eP, eL, ...st
          )
        )
      ) === 0
    );
  }

  /**
   * Create a radio-button group: one logical field whose `options` are the
   * individual buttons. `selected` is the initially-chosen export value.
   */
  addRadioGroup(
    page: number,
    name: string,
    options: RadioOption[],
    opts: { selected?: string; style?: FieldStyle } = {}
  ): boolean {
    const st = styleArgs(opts.style);
    const exports = options.map((o) => o.export).join("\n");
    const rects = options.flatMap((o) => o.rect).join(",");
    return (
      this.g._withStr(name, (nP, nL) =>
        this.g._withStr(exports, (eP, eL) =>
          this.g._withStr(rects, (rP, rL) =>
            this.g._withStr(opts.selected ?? "", (sP, sL) =>
              this.ex().gp_add_radio_group(this.h, page, nP, nL, eP, eL, rP, rL, sP, sL, ...st)
            )
          )
        )
      ) === 0
    );
  }

  /** Create a drop-down combo box. `editable` permits values outside `options`. */
  addComboBox(
    page: number,
    name: string,
    rect: [number, number, number, number],
    options: string[],
    opts: { selected?: string; editable?: boolean; style?: FieldStyle } = {}
  ): boolean {
    const st = styleArgs(opts.style);
    return (
      this.g._withStr(name, (nP, nL) =>
        this.g._withStr(options.join("\n"), (oP, oL) =>
          this.g._withStr(opts.selected ?? "", (sP, sL) =>
            this.ex().gp_add_combo_box(
              this.h, page, nP, nL, rect[0], rect[1], rect[2], rect[3], oP, oL, sP, sL,
              opts.editable ? 1 : 0, ...st
            )
          )
        )
      ) === 0
    );
  }

  /** Create a scrolling list box. `multi` allows selecting several options. */
  addListBox(
    page: number,
    name: string,
    rect: [number, number, number, number],
    options: string[],
    opts: { selected?: string; multi?: boolean; style?: FieldStyle } = {}
  ): boolean {
    const st = styleArgs(opts.style);
    return (
      this.g._withStr(name, (nP, nL) =>
        this.g._withStr(options.join("\n"), (oP, oL) =>
          this.g._withStr(opts.selected ?? "", (sP, sL) =>
            this.ex().gp_add_list_box(
              this.h, page, nP, nL, rect[0], rect[1], rect[2], rect[3], oP, oL, sP, sL,
              opts.multi ? 1 : 0, ...st
            )
          )
        )
      ) === 0
    );
  }

  /**
   * Create a **visible signature field** (`/FT /Sig`) over `rect` — an empty
   * widget the signing pipeline can later target — and flag the AcroForm
   * `/SigFlags`. Returns `true` on success.
   */
  addSignatureField(
    page: number,
    name: string,
    rect: [number, number, number, number],
    opts: { style?: FieldStyle } = {}
  ): boolean {
    const st = styleArgs(opts.style);
    return (
      this.g._withStr(name, (nP, nL) =>
        this.ex().gp_add_signature_field(
          this.h, page, nP, nL, rect[0], rect[1], rect[2], rect[3], ...st
        )
      ) === 0
    );
  }

  /**
   * Attach field-level JavaScript to a field's `/AA` for the given `trigger`
   * (`"keystroke"` filters input, `"format"` formats display, `"validate"`
   * checks on change, `"calculate"` recomputes — ordered by
   * {@link setCalculationOrder}). Returns `true` if set, `false` if no field has
   * that name.
   */
  setFieldScript(
    name: string,
    trigger: "keystroke" | "format" | "validate" | "calculate",
    js: string
  ): boolean {
    return (
      this.g._withStr(name, (nP, nL) =>
        this.g._withStr(trigger, (tP, tL) =>
          this.g._withStr(js, (jP, jL) =>
            this.ex().gp_set_field_script(this.h, nP, nL, tP, tL, jP, jL)
          )
        )
      ) === 1
    );
  }

  /**
   * Set the AcroForm calculation order (`/CO`) — the sequence in which fields
   * with a `calculate` script recompute. Unknown names are skipped.
   */
  setCalculationOrder(names: string[]): boolean {
    return (
      this.g._withOptStr(names.join("\n"), (p, l) =>
        this.ex().gp_set_calculation_order(this.h, p, l)
      ) === 0
    );
  }

  /**
   * Delete a form field by name (removed from `/Fields`, `/CO` and every page's
   * annotations). Returns `true` if a field was removed.
   */
  removeField(name: string): boolean {
    return this.g._withStr(name, (p, l) => this.ex().gp_remove_field(this.h, p, l)) === 1;
  }

  /**
   * Rebuild a field's appearance from its current value/style — call after
   * changing a field's value programmatically. Returns `true` if regenerated
   * (`false` for an unknown name or a kind that can't be regenerated alone, e.g.
   * a radio parent with kids).
   */
  regenerateFieldAppearance(name: string): boolean {
    return (
      this.g._withStr(name, (p, l) => this.ex().gp_regenerate_field_appearance(this.h, p, l)) === 1
    );
  }
}
