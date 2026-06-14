/**
 * @giga-pdf/wasm-engine — TypeScript SDK for the zero-dependency Rust→WASM PDF
 * engine (gigapdf-engine). Wraps the flat `extern "C"` `gp_*` ABI behind a typed,
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
}

export interface FontInfo {
  family: string;
  category: string;
  google: boolean;
  weights: number[];
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
  value: string;
  options: string[];
}
/** One outline (bookmark) entry; `level` is the nesting depth (0 = top). */
export interface OutlineEntry {
  level: number;
  title: string;
  page?: number;
}

const RGB = (rgb: number) => rgb & 0xffffff;

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
    lineWidth = 1
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
        lineWidth
      ) === 0
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
  /** Extract the given 1-based page numbers into a NEW standalone PDF. */
  extractPages(pages: number[]): Uint8Array {
    return this.g._withU32(pages, (p, c) =>
      this.g._buffer((o) => this.ex().gp_extract_pages(this.h, p, c, o))
    );
  }

  // render
  renderPage(page: number, scale = 1): Uint8Array {
    return this.g._buffer((o) => this.ex().gp_render_page(this.h, page, scale, o));
  }

  // fonts — embed a downloaded TTF, then add real selectable text
  embedFont(family: string, ttf: Uint8Array): number {
    const b = enc.encode(family);
    const famPtr = this.g._toWasm(b);
    const obj = this.g._withBytes(ttf, (p, l) =>
      this.ex().gp_embed_font(this.h, famPtr, b.length, p, l)
    );
    this.g._free(famPtr, b.length);
    return obj;
  }
  addText(
    page: number,
    x: number,
    y: number,
    size: number,
    text: string,
    fontObj: number,
    rgb = 0
  ): boolean {
    return (
      this.g._withStr(text, (p, l) =>
        this.ex().gp_add_text(this.h, page, x, y, size, p, l, fontObj, RGB(rgb))
      ) === 0
    );
  }
  neededFonts(): string[] {
    return this.g._json((o) => this.ex().gp_needed_fonts(this.h, o));
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
  saveEncrypted(password: string, fileId: string, permissions = -44): Uint8Array {
    return this.g._withStr(password, (pwP, pwL) =>
      this.g._withStr(fileId, (idP, idL) =>
        this.g._buffer((o) =>
          this.ex().gp_save_encrypted(this.h, pwP, pwL, idP, idL, permissions, o)
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
}
