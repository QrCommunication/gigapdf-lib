/**
 * @qrcommunication/gigapdf-lib — TypeScript SDK for the zero-dependency Rust→WASM
 * PDF engine (gigapdf-lib). Wraps the flat `extern "C"` `gp_*` ABI behind a typed,
 * ergonomic class. No third-party runtime deps; the `.wasm` is self-contained.
 *
 * Usage:
 *   const giga = await GigaPdfEngine.load(wasmBytesOrUrl);
 *   const doc = giga.open(pdfBytes);
 *   const docx = doc.toDocx();
 *   const png = doc.renderPage(1, 2);
 *   doc.close();
 */

// FFI boundary: the wasm exports are an untyped table of `gp_*` functions
// (numbers in, numbers out) plus `memory`. `any` here is the documented FFI
// exception — every public method below re-imposes precise types.
type Exports = {
  memory: WebAssembly.Memory;
  gp_alloc(len: number): number;
  gp_free(ptr: number, len: number): void;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  [k: string]: any;
};

const enc = new TextEncoder();
const dec = new TextDecoder();

/** Loaded engine module. Create documents with {@link open} / {@link openEncrypted}. */
export class GigaPdfEngine {
  private constructor(private readonly ex: Exports) {}

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
    const { instance } = await WebAssembly.instantiate(
      bytes instanceof Uint8Array ? bytes.slice().buffer : bytes,
      {}
    );
    return new GigaPdfEngine(instance.exports as Exports);
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
    const { readFile } = await import("node:fs/promises");
    const { fileURLToPath } = await import("node:url");
    const wasmPath = fileURLToPath(new URL("../gigapdf.wasm", import.meta.url));
    return GigaPdfEngine.load(await readFile(wasmPath));
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
    const ptr = this.ex.gp_alloc(bytes.length);
    this.u8().set(bytes, ptr);
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

    return this._withStr(html, (hp, hl) =>
      this._withBytes(blob, (fp, fl) =>
        this._withOptStr(options.header, (hdp, hdl) =>
          this._withOptStr(options.footer, (ftp, ftl) =>
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
                o
              )
            )
          )
        )
      )
    );
  }
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
export interface TextLine extends Box {
  text: string;
}
export interface SearchHit extends Box {
  page: number;
  text: string;
}
export interface OcrWord extends Box {
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
/** A markup annotation (rect corners in PDF user space). */
export interface AnnotationInfo {
  index: number;
  subtype: string;
  x0: number;
  y0: number;
  x1: number;
  y1: number;
  contents: string;
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
export interface FieldInfo {
  name: string;
  type: string;
  kind: FieldKind;
  flags: number;
  readOnly: boolean;
  required: boolean;
  multiline: boolean;
  fillable: boolean;
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
/** One outline (bookmark) entry; `level` is the nesting depth (0 = top). */
export interface OutlineEntry {
  level: number;
  title: string;
  page?: number;
}
/** A named destination from {@link GigaPdfDoc.namedDests}: a name → page anchor. */
export interface NamedDest {
  name: string;
  page: number;
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
}

const RGB = (rgb: number) => rgb & 0xffffff;

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

  // text intelligence
  textRuns(page: number): TextRunInfo[] {
    return this.g._json((o) => this.ex().gp_text_runs_json(this.h, page, o));
  }
  elements(page: number): Element[] {
    return this.g._json((o) => this.ex().gp_elements_json(this.h, page, o));
  }
  structuredText(page: number): TextLine[] {
    return this.g._json((o) => this.ex().gp_structured_text_json(this.h, page, o));
  }
  search(query: string, caseInsensitive = true): SearchHit[] {
    return this.g._withStr(query, (p, l) =>
      this.g._json((o) => this.ex().gp_search_json(this.h, p, l, caseInsensitive ? 1 : 0, o))
    );
  }
  /** OCR a (scanned) page → words with PDF-space boxes. `scale` ≥ 2 for small text. */
  ocr(page: number, scale = 2): OcrWord[] {
    return this.g._json((o) => this.ex().gp_ocr_json(this.h, page, scale, o));
  }
  ocrText(page: number, scale = 2): string {
    return this.g._str((o) => this.ex().gp_ocr_text(this.h, page, scale, o));
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
  appendPages(otherPdf: Uint8Array): boolean {
    return this.g._withBytes(otherPdf, (p, l) => this.ex().gp_append_pages(this.h, p, l)) === 0;
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
  /** Resize a page's MediaBox to `width`×`height` points. */
  resizePage(page: number, width: number, height: number): boolean {
    return this.ex().gp_resize_page(this.h, page, width, height) === 0;
  }
  /** Insert a blank page after the 1-based `after` page (0 = front); returns its id. */
  addPage(width: number, height: number, after = 0): number {
    return this.ex().gp_add_page(this.h, width, height, after);
  }
  /** Duplicate a page, inserting the copy right after it; returns the new page's id. */
  copyPage(page: number): number {
    return this.ex().gp_copy_page(this.h, page);
  }
  /** A page's size (points) and `/Rotate` (0/90/180/270). */
  pageInfo(page: number): PageInfo {
    return this.g._json((o) => this.ex().gp_page_info_json(this.h, page, o));
  }

  // render
  renderPage(page: number, scale = 1): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_render_page(this.h, page, scale, o));
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
    rotationDeg = 0
  ): boolean {
    return (
      this.g._withStr(text, (p, l) =>
        this.ex().gp_add_text(
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
          rotationDeg
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
    rotationDeg = 0
  ): boolean {
    return (
      this.g._withStr(text, (tp, tl) =>
        this.g._withStr(fontName, (fp, fl) =>
          this.ex().gp_add_text_standard(
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
            rotationDeg
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
  toPdfA(): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_to_pdfa(this.h, o));
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
      permissions?: number;
      keySeed?: Uint8Array;
    } = {}
  ): Uint8Array {
    const algo = opts.algorithm ?? "aes256";
    const algoCode = algo === "rc4" ? 0 : algo === "aes128" ? 1 : 2;
    const permissions = opts.permissions ?? -44;
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

  // metadata
  getMetadata(key: string): string {
    return this.g._withStr(key, (p, l) =>
      this.g._str((o) => this.ex().gp_get_metadata(this.h, p, l, o))
    );
  }
  /** Set an Info-dictionary entry (e.g. "Title", "Author"). */
  setMetadata(key: string, value: string): boolean {
    return (
      this.g._withStr(key, (kP, kL) =>
        this.g._withStr(value, (vP, vL) => this.ex().gp_set_metadata(this.h, kP, kL, vP, vL))
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
  addLineAnnotation(
    page: number,
    x1: number,
    y1: number,
    x2: number,
    y2: number,
    rgb = 0,
    lineWidth = 1
  ): boolean {
    return this.ex().gp_add_line(this.h, page, x1, y1, x2, y2, RGB(rgb), lineWidth) === 0;
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

  // outline (bookmarks)
  outline(): OutlineEntry[] {
    return this.g._json((o) => this.ex().gp_outline_json(this.h, o));
  }
  /** Replace the outline. Each entry: `{level, page?, title}` (page 0/undefined = no dest). */
  setOutline(entries: OutlineEntry[]): boolean {
    const text = entries.map((e) => `${e.level}\t${e.page ?? 0}\t${e.title}`).join("\n");
    return this.g._withStr(text, (p, l) => this.ex().gp_set_outline(this.h, p, l)) === 0;
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
}
