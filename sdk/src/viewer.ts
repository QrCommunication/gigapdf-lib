/**
 * `@qrcommunication/gigapdf-lib/viewer` — a zero-dependency document **viewer**
 * built entirely on the WASM engine. It opens PDF, Office (docx/xlsx/pptx +
 * legacy/ODF) and HTML — converting non-PDF inputs to PDF in-engine — renders
 * pages with {@link GigaPdfDoc.renderPage}, detects each page's orientation and
 * adapts, and offers navigation, zoom, a thumbnail rail and a **fullscreen
 * presentation mode**. No third-party libraries (no pdf.js); browser-only (DOM).
 *
 * ```ts
 * const giga = await GigaPdfEngine.load(wasmUrl);
 * const viewer = new GigaPdfViewer(giga, document.getElementById("app")!);
 * await viewer.open({ kind: "auto", bytes });   // pdf / office / html auto-detected
 * viewer.present();                              // fullscreen slideshow
 * ```
 */
import { GigaPdfEngine, GigaPdfDoc, type HtmlFont } from "./index";

/** A document to open. `auto` sniffs the format from magic bytes. */
export type ViewerSource =
  | { kind: "pdf"; bytes: Uint8Array }
  | { kind: "office"; bytes: Uint8Array }
  | { kind: "html"; html: string; fonts?: HtmlFont[] }
  | { kind: "auto"; bytes: Uint8Array };

export interface ViewerOptions {
  /** CSS zoom multiplier applied to the page boxes (default 1). */
  scale?: number;
  /** Raster scale for crisp rendering (default 2 ≈ retina). */
  renderScale?: number;
  /** Show the thumbnail rail (default true). */
  thumbnails?: boolean;
  /** Show the toolbar (default true). */
  toolbar?: boolean;
  /** Gutter background colour (default `#525659`). */
  background?: string;
}

/** Page orientation derived from the rendered raster. */
export type Orientation = "portrait" | "landscape";

const CSS = `
.gpv{position:relative;display:flex;flex-direction:column;height:100%;width:100%;background:var(--gpv-bg,#525659);color:#eee;font:13px/1.4 system-ui,sans-serif;overflow:hidden}
.gpv-bar{display:flex;align-items:center;gap:6px;padding:6px 10px;background:#323639;border-bottom:1px solid #000;flex:0 0 auto;user-select:none}
.gpv-bar button{background:#4a4f52;color:#eee;border:0;border-radius:4px;padding:4px 9px;cursor:pointer;font:inherit}
.gpv-bar button:hover{background:#5a6063}
.gpv-bar .gpv-sp{flex:1}
.gpv-bar input{width:46px;background:#222;color:#eee;border:1px solid #000;border-radius:4px;padding:3px;text-align:center;font:inherit}
.gpv-bar select{background:#222;color:#eee;border:1px solid #000;border-radius:4px;padding:3px;font:inherit;cursor:pointer}
.gpv-bar .gpv-zoom{min-width:44px;text-align:center;color:#ddd;font-variant-numeric:tabular-nums}
.gpv-body{flex:1;display:flex;min-height:0}
.gpv-thumbs{flex:0 0 132px;overflow-y:auto;background:#2a2d2f;padding:8px;display:flex;flex-direction:column;gap:8px}
.gpv-thumbs img{width:100%;display:block;border:2px solid transparent;border-radius:2px;cursor:pointer;background:#fff}
.gpv-thumbs .gpv-on img{border-color:#3b82f6}
.gpv-thumbs span{display:block;text-align:center;color:#aaa;font-size:11px}
.gpv-pages{flex:1;overflow:auto;padding:18px;display:flex;flex-direction:column;align-items:center;gap:18px;scroll-behavior:smooth}
.gpv-page{position:relative;box-shadow:0 1px 6px rgba(0,0,0,.6);background:#fff}
.gpv-page img{display:block;width:100%;height:auto}
.gpv:fullscreen{--gpv-bg:#000}
.gpv.gpv-present .gpv-thumbs,.gpv.gpv-present .gpv-bar{display:none}
.gpv.gpv-present .gpv-pages{padding:0;justify-content:center;overflow:hidden}
.gpv.gpv-present .gpv-page{box-shadow:none;max-height:100vh}
.gpv.gpv-present .gpv-page img{width:auto;max-width:100vw;max-height:100vh}
`;

let styleInjected = false;
function injectStyle(doc: Document) {
  if (styleInjected) return;
  const el = doc.createElement("style");
  el.textContent = CSS;
  doc.head.appendChild(el);
  styleInjected = true;
}

/** Read a PNG's pixel dimensions from its IHDR (bytes 16–23, big-endian). */
function pngSize(b: Uint8Array): { w: number; h: number } {
  if (b.length < 24) return { w: 1, h: 1 };
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  return { w: dv.getUint32(16) || 1, h: dv.getUint32(20) || 1 };
}

export class GigaPdfViewer {
  // `protected` members are the surface the editor subclass builds on.
  protected doc: GigaPdfDoc | null = null;
  protected count = 0;
  protected current = 1;
  protected cssScale: number;
  protected renderScale: number;
  /** Active auto-fit mode; re-applied on resize and page change. */
  protected fitMode: "none" | "width" | "page" = "none";
  private presenting = false;
  private resizeObs: ResizeObserver | null = null;

  // Per-page caches (1-indexed; slot 0 unused).
  protected urls: (string | null)[] = [];
  protected sizes: ({ w: number; h: number } | null)[] = [];

  // DOM nodes.
  protected root: HTMLElement;
  private bar: HTMLElement | null = null;
  private body!: HTMLElement;
  private thumbs!: HTMLElement;
  protected pages!: HTMLElement;
  private pageInput: HTMLInputElement | null = null;
  private zoomReadout: HTMLElement | null = null;
  private zoomSelect: HTMLSelectElement | null = null;
  protected pageEls: (HTMLElement | null)[] = [];
  private onKey: (e: KeyboardEvent) => void;
  private onFsChange: () => void;

  constructor(
    protected engine: GigaPdfEngine,
    container: HTMLElement,
    private options: ViewerOptions = {}
  ) {
    this.cssScale = options.scale ?? 1;
    this.renderScale = options.renderScale ?? 2;
    this.root = container;
    injectStyle(container.ownerDocument);
    this.buildChrome();
    this.onKey = (e) => this.handleKey(e);
    this.onFsChange = () => this.syncPresentClass();
    container.ownerDocument.addEventListener("keydown", this.onKey);
    container.ownerDocument.addEventListener("fullscreenchange", this.onFsChange);
  }

  /** Number of pages currently open. */
  get pageCount(): number {
    return this.count;
  }
  /** The page currently in view (1-based). */
  get currentPage(): number {
    return this.current;
  }
  /** Orientation of `page` (after open). */
  orientation(page: number): Orientation {
    const s = this.sizes[page];
    return s && s.w > s.h ? "landscape" : "portrait";
  }

  /** The current document as PDF bytes (including any applied edits). */
  save(): Uint8Array {
    if (!this.doc) return new Uint8Array(0);
    return this.doc.save();
  }

  /** Open a document; Office/HTML are converted to PDF in-engine. Returns the page count. */
  async open(src: ViewerSource): Promise<number> {
    const pdf = this.toPdf(src);
    this.close();
    this.doc = this.engine.open(pdf);
    this.count = this.doc.pageCount();
    this.urls = new Array(this.count + 1).fill(null);
    this.sizes = new Array(this.count + 1).fill(null);
    this.pageEls = new Array(this.count + 1).fill(null);
    this.current = 1;
    this.renderAllPages();
    if (this.options.thumbnails !== false) this.buildThumbs();
    this.goTo(1);
    return this.count;
  }

  private toPdf(src: ViewerSource): Uint8Array {
    switch (src.kind) {
      case "pdf":
        return src.bytes;
      case "office":
        return this.engine.officeToPdf(src.bytes);
      case "html":
        return src.fonts?.length
          ? this.engine.htmlRender(src.html, src.fonts)
          : this.engine.htmlToPdf(src.html);
      case "auto":
        return this.detectAndConvert(src.bytes);
    }
  }

  private detectAndConvert(b: Uint8Array): Uint8Array {
    const is = (...sig: number[]) => sig.every((v, i) => b[i] === v);
    if (is(0x25, 0x50, 0x44, 0x46)) return b; // %PDF
    if (is(0x50, 0x4b)) return this.engine.officeToPdf(b); // PK zip (docx/xlsx/pptx/odf)
    if (is(0xd0, 0xcf, 0x11, 0xe0)) return this.engine.officeToPdf(b); // OLE2 (legacy doc/xls/ppt)
    return this.engine.htmlToPdf(new TextDecoder().decode(b)); // else: HTML/text
  }

  // ── rendering ────────────────────────────────────────────────────────────────
  protected renderPageRaster(page: number): { url: string; w: number; h: number } {
    const cachedUrl = this.urls[page];
    const cachedSize = this.sizes[page];
    if (cachedUrl && cachedSize) return { url: cachedUrl, ...cachedSize };
    const png = this.doc!.renderPage(page, this.renderScale);
    const size = pngSize(png);
    // Copy into a fresh ArrayBuffer (the wasm-backed view may sit on a
    // SharedArrayBuffer, which `Blob` rejects).
    const buf = new ArrayBuffer(png.byteLength);
    new Uint8Array(buf).set(png);
    const url = URL.createObjectURL(new Blob([buf], { type: "image/png" }));
    this.urls[page] = url;
    this.sizes[page] = size;
    return { url, ...size };
  }

  /** The page's width / height in PDF points (raster pixels ÷ render scale). */
  protected pageWidthPt(page: number): number {
    const s = this.sizes[page];
    return s ? s.w / this.renderScale : 0;
  }
  protected pageHeightPt(page: number): number {
    const s = this.sizes[page];
    return s ? s.h / this.renderScale : 0;
  }

  /** Re-raster a page after its content changed (drops the cached image). */
  protected reRenderPage(page: number) {
    const old = this.urls[page];
    if (old) URL.revokeObjectURL(old);
    this.urls[page] = null;
    this.sizes[page] = null;
    const { url, w } = this.renderPageRaster(page);
    const box = this.pageEls[page];
    if (!box) return;
    box.style.width = `${(w / this.renderScale) * this.cssScale}px`;
    const img = box.querySelector("img");
    if (img) img.src = url;
  }

  /** Hook for subclasses (the editor) to attach per-page overlays. */
  protected afterRender() {}

  private renderAllPages() {
    this.pages.replaceChildren();
    for (let p = 1; p <= this.count; p++) {
      const { url, w, h } = this.renderPageRaster(p);
      const box = this.root.ownerDocument.createElement("div");
      box.className = "gpv-page";
      box.dataset.page = String(p);
      // Size the box to the page's true aspect ratio (so landscape pages are
      // wide and portrait pages tall) at the current CSS zoom.
      box.style.width = `${(w / this.renderScale) * this.cssScale}px`;
      const img = this.root.ownerDocument.createElement("img");
      img.src = url;
      img.alt = `Page ${p}`;
      img.loading = "lazy";
      box.appendChild(img);
      this.pages.appendChild(box);
      this.pageEls[p] = box;
    }
    this.afterRender();
  }

  private buildThumbs() {
    this.thumbs.replaceChildren();
    for (let p = 1; p <= this.count; p++) {
      const url = this.urls[p];
      const item = this.root.ownerDocument.createElement("div");
      item.dataset.page = String(p);
      const img = this.root.ownerDocument.createElement("img");
      if (url) img.src = url;
      img.addEventListener("click", () => this.goTo(p));
      const label = this.root.ownerDocument.createElement("span");
      label.textContent = String(p);
      item.append(img, label);
      this.thumbs.appendChild(item);
    }
    this.highlightThumb();
  }

  private highlightThumb() {
    for (const child of Array.from(this.thumbs.children)) {
      child.classList.toggle("gpv-on", child.getAttribute("data-page") === String(this.current));
    }
  }

  // ── navigation ───────────────────────────────────────────────────────────────
  /** Scroll/jump to `page` (clamped to range). */
  goTo(page: number) {
    if (this.count === 0) return;
    this.current = Math.min(Math.max(1, Math.round(page)), this.count);
    if (this.presenting) {
      // Single-page: show only the current page.
      for (let p = 1; p <= this.count; p++) {
        const el = this.pageEls[p];
        if (el) el.style.display = p === this.current ? "" : "none";
      }
    } else {
      this.pageEls[this.current]?.scrollIntoView({ block: "start" });
    }
    if (this.pageInput) this.pageInput.value = String(this.current);
    if (this.fitMode !== "none") this.applyFitMode();
    this.highlightThumb();
  }
  next() {
    this.goTo(this.current + 1);
  }
  prev() {
    this.goTo(this.current - 1);
  }

  // ── zoom ─────────────────────────────────────────────────────────────────────
  /** Current CSS zoom multiplier (1 = 100%). */
  get zoom(): number {
    return this.cssScale;
  }

  /** Resize the page boxes to the current `cssScale` (no re-raster). */
  private applyScale(scale: number) {
    this.cssScale = Math.min(Math.max(0.08, scale), 8);
    for (let p = 1; p <= this.count; p++) {
      const box = this.pageEls[p];
      const size = this.sizes[p];
      if (box && size) box.style.width = `${(size.w / this.renderScale) * this.cssScale}px`;
    }
    this.updateZoomReadout();
    this.onZoomChange();
  }

  /** Set an explicit zoom multiplier (cancels any auto-fit mode). */
  setZoom(scale: number) {
    this.fitMode = "none";
    this.applyScale(scale);
  }
  /** Set zoom as a percentage (e.g. `125`). */
  setZoomPercent(pct: number) {
    this.setZoom(pct / 100);
  }
  /** Reset to 100 % (actual size). */
  actualSize() {
    this.setZoom(1);
  }
  zoomIn() {
    this.setZoom(this.cssScale * 1.2);
  }
  zoomOut() {
    this.setZoom(this.cssScale / 1.2);
  }
  /** Fit the current page's **width** to the viewport (sticks across resizes). */
  fitWidth() {
    this.fitMode = "width";
    this.applyFitMode();
  }
  /** Fit the **whole** current page (width *and* height) to the viewport. */
  fitPage() {
    this.fitMode = "page";
    this.applyFitMode();
  }
  /** Recompute the zoom for the active fit mode against the current page. */
  protected applyFitMode() {
    if (this.fitMode === "none" || this.presenting || this.count === 0) return;
    const size = this.sizes[this.current];
    if (!size) return;
    const wPt = size.w / this.renderScale;
    const hPt = size.h / this.renderScale;
    if (this.fitMode === "width") {
      this.applyScale((this.pages.clientWidth - 36) / wPt || 1);
    } else {
      const availW = this.pages.clientWidth - 36;
      const availH = this.pages.clientHeight - 36;
      this.applyScale(Math.min(availW / wPt, availH / hPt) || 1);
    }
  }

  /** Hook for subclasses (the editor) to react to zoom changes. */
  protected onZoomChange() {}

  private updateZoomReadout() {
    if (this.zoomReadout) this.zoomReadout.textContent = `${Math.round(this.cssScale * 100)}%`;
    if (this.zoomSelect) {
      const v =
        this.fitMode === "width" ? "width" : this.fitMode === "page" ? "page" : String(this.cssScale);
      const has = Array.from(this.zoomSelect.options).some((o) => o.value === v);
      this.zoomSelect.value = has ? v : "";
    }
  }

  // ── presentation (fullscreen slideshow) ───────────────────────────────────────
  /** Enter fullscreen single-page presentation mode. */
  present() {
    this.presenting = true;
    this.root.classList.add("gpv-present");
    this.goTo(this.current);
    void this.root.requestFullscreen?.();
  }
  /** Leave presentation mode. */
  exitPresent() {
    this.presenting = false;
    this.root.classList.remove("gpv-present");
    if (this.root.ownerDocument.fullscreenElement) void this.root.ownerDocument.exitFullscreen?.();
    for (let p = 1; p <= this.count; p++) {
      const el = this.pageEls[p];
      if (el) el.style.display = "";
    }
    this.goTo(this.current);
  }
  private syncPresentClass() {
    // The user pressed Esc / left fullscreen via the browser chrome.
    if (this.presenting && !this.root.ownerDocument.fullscreenElement) this.exitPresent();
  }

  // ── keyboard ─────────────────────────────────────────────────────────────────
  private handleKey(e: KeyboardEvent) {
    if (!this.isActive()) return;
    switch (e.key) {
      case "ArrowRight":
      case "PageDown":
      case " ":
        this.next();
        break;
      case "ArrowLeft":
      case "PageUp":
        this.prev();
        break;
      case "Home":
        this.goTo(1);
        break;
      case "End":
        this.goTo(this.count);
        break;
      case "+":
      case "=":
        this.zoomIn();
        break;
      case "-":
        this.zoomOut();
        break;
      case "0":
        this.actualSize();
        break;
      case "f":
      case "F":
        this.presenting ? this.exitPresent() : this.present();
        break;
      case "Escape":
        if (this.presenting) this.exitPresent();
        break;
      default:
        return;
    }
    e.preventDefault();
  }
  private isActive(): boolean {
    return this.presenting || this.root.contains(this.root.ownerDocument.activeElement) || this.root.matches(":hover");
  }

  // ── chrome ───────────────────────────────────────────────────────────────────
  private buildChrome() {
    const d = this.root.ownerDocument;
    this.root.classList.add("gpv");
    if (this.options.background) this.root.style.setProperty("--gpv-bg", this.options.background);
    this.root.replaceChildren();

    if (this.options.toolbar !== false) {
      const bar = d.createElement("div");
      bar.className = "gpv-bar";
      const btn = (label: string, fn: () => void, title?: string) => {
        const b = d.createElement("button");
        b.textContent = label;
        if (title) b.title = title;
        b.addEventListener("click", fn);
        bar.appendChild(b);
        return b;
      };
      btn("‹", () => this.prev(), "Previous page");
      btn("›", () => this.next(), "Next page");
      const input = d.createElement("input");
      input.value = "1";
      input.addEventListener("change", () => this.goTo(Number(input.value) || 1));
      bar.appendChild(input);
      this.pageInput = input;
      btn("−", () => this.zoomOut(), "Zoom out");
      const zr = d.createElement("span");
      zr.className = "gpv-zoom";
      zr.textContent = "100%";
      bar.appendChild(zr);
      this.zoomReadout = zr;
      btn("+", () => this.zoomIn(), "Zoom in");
      const zsel = d.createElement("select");
      zsel.title = "Zoom level";
      const zopts: [string, string][] = [
        ["width", "Fit width"],
        ["page", "Fit page"],
        ["0.5", "50%"],
        ["0.75", "75%"],
        ["1", "100%"],
        ["1.25", "125%"],
        ["1.5", "150%"],
        ["2", "200%"],
        ["4", "400%"],
      ];
      for (const [val, label] of zopts) {
        const o = d.createElement("option");
        o.value = val;
        o.textContent = label;
        zsel.appendChild(o);
      }
      zsel.value = "1";
      zsel.addEventListener("change", () => {
        const v = zsel.value;
        if (v === "width") this.fitWidth();
        else if (v === "page") this.fitPage();
        else if (v) this.setZoom(Number(v));
      });
      bar.appendChild(zsel);
      this.zoomSelect = zsel;
      const sp = d.createElement("div");
      sp.className = "gpv-sp";
      bar.appendChild(sp);
      btn("⛶ Present", () => this.present(), "Fullscreen presentation (F)");
      this.root.appendChild(bar);
      this.bar = bar;
    }

    this.body = d.createElement("div");
    this.body.className = "gpv-body";
    this.thumbs = d.createElement("div");
    this.thumbs.className = "gpv-thumbs";
    if (this.options.thumbnails === false) this.thumbs.style.display = "none";
    this.pages = d.createElement("div");
    this.pages.className = "gpv-pages";
    // Track which page is centred while scrolling (continuous mode).
    this.pages.addEventListener("scroll", () => this.trackScroll());
    this.body.append(this.thumbs, this.pages);
    this.root.appendChild(this.body);

    // Re-apply the active fit mode when the viewport resizes.
    if (typeof ResizeObserver !== "undefined") {
      this.resizeObs = new ResizeObserver(() => this.applyFitMode());
      this.resizeObs.observe(this.pages);
    }
    // Ctrl/⌘ + wheel zooms (like every PDF viewer).
    this.pages.addEventListener(
      "wheel",
      (e) => {
        if (!e.ctrlKey && !e.metaKey) return;
        e.preventDefault();
        this.setZoom(this.cssScale * (e.deltaY < 0 ? 1.1 : 1 / 1.1));
      },
      { passive: false }
    );
  }

  private trackScroll() {
    if (this.presenting || this.count === 0) return;
    const mid = this.pages.scrollTop + this.pages.clientHeight / 2;
    let best = this.current;
    for (let p = 1; p <= this.count; p++) {
      const el = this.pageEls[p];
      if (el && el.offsetTop <= mid) best = p;
    }
    if (best !== this.current) {
      this.current = best;
      if (this.pageInput) this.pageInput.value = String(best);
      if (this.fitMode !== "none") this.applyFitMode();
      this.highlightThumb();
    }
  }

  // ── lifecycle ────────────────────────────────────────────────────────────────
  /** Close the current document and free its rendered pages. */
  close() {
    for (const u of this.urls) if (u) URL.revokeObjectURL(u);
    this.urls = [];
    this.sizes = [];
    this.pageEls = [];
    this.doc?.close();
    this.doc = null;
    this.count = 0;
    this.pages?.replaceChildren();
    this.thumbs?.replaceChildren();
  }

  /** Destroy the viewer: close the document and detach listeners. */
  destroy() {
    this.close();
    this.resizeObs?.disconnect();
    this.resizeObs = null;
    this.root.ownerDocument.removeEventListener("keydown", this.onKey);
    this.root.ownerDocument.removeEventListener("fullscreenchange", this.onFsChange);
    this.root.classList.remove("gpv", "gpv-present");
    this.root.replaceChildren();
  }
}
