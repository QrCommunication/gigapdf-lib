/**
 * `@qrcommunication/gigapdf-lib/editor` — an interactive **editing canvas** that
 * extends {@link GigaPdfViewer}. It overlays each rendered page with an SVG
 * surface for drawing/placing annotations and shapes (rectangle, ellipse, line,
 * freehand ink, text, image, highlight, redaction), supports select / move /
 * delete, and **bakes the edits into the real PDF** through the engine
 * (`addRectangle`/`addEllipse`/`addPolygon`/`addText`/`addImage`/`redact`) — then
 * re-renders. Zero external libraries; browser-only.
 *
 * ```ts
 * const ed = new GigaPdfEditor(giga, host, { defaultFont: { family: "Roboto", ttf } });
 * await ed.open({ kind: "auto", bytes });
 * ed.setTool("rect"); ed.setStyle({ color: 0xcc0000 });
 * // …user draws…
 * ed.applyEdits();                 // bake into the PDF
 * const pdf = ed.save();           // download the result
 * ```
 */
import { GigaPdfEngine } from "./index";
import { GigaPdfViewer, type ViewerOptions } from "./viewer";

const SVG_NS = "http://www.w3.org/2000/svg" as const;
const PT_PER_MM = 72 / 25.4; // 1 mm in PDF points

/** A page edge that carries an adjustable margin. */
export type Side = "top" | "right" | "bottom" | "left";

export type EditTool =
  | "select"
  | "text"
  | "rect"
  | "ellipse"
  | "line"
  | "ink"
  | "image"
  | "highlight"
  | "redact";

export interface EditStyle {
  /** Stroke (shapes / ink / line) and text colour, `0xRRGGBB`. */
  color: number;
  /** Shape fill colour, or `null` for no fill. */
  fill: number | null;
  lineWidth: number;
  fontSize: number;
  /** 0–1 fill/stroke alpha. */
  opacity: number;
}

export interface EditorOptions extends ViewerOptions {
  /** A TrueType font for the text tool (required to add text). */
  defaultFont?: { family: string; ttf: Uint8Array };
}

// Element geometry is in **PDF points, top-left origin** (Y-down) so it maps 1:1
// onto the overlay SVG (whose viewBox is the page in points) and flips to PDF's
// bottom-left Y-up only when applied.
type El = {
  id: number;
  page: number;
  kind: EditTool;
  // bbox / endpoints / polyline, all in page points (top-left):
  x: number;
  y: number;
  w: number;
  h: number;
  pts?: number[];
  text?: string;
  imgUrl?: string;
  data?: Uint8Array;
  s: EditStyle;
};

const hex = (n: number) => "#" + (n & 0xffffff).toString(16).padStart(6, "0");

export class GigaPdfEditor extends GigaPdfViewer {
  private tool: EditTool = "select";
  private style: EditStyle = { color: 0x1a1a1a, fill: null, lineWidth: 1.5, fontSize: 16, opacity: 1 };
  private els: El[] = [];
  private overlays: (SVGSVGElement | null)[] = [];
  private idSeq = 1;
  private selected: El | null = null;
  private fontObj: number | null = null;
  private editorOpts: EditorOptions;
  private palette: HTMLElement | null = null;

  // Rulers & margins. Margins are stored in **PDF points**, shared across pages.
  private margins: Record<Side, number> = { top: 56.7, right: 56.7, bottom: 56.7, left: 56.7 }; // ≈20 mm
  private guides: (SVGSVGElement | null)[] = [];
  private showGuides = true;
  private marginInputs: Record<Side, HTMLInputElement | null> = {
    top: null,
    right: null,
    bottom: null,
    left: null,
  };

  constructor(engine: GigaPdfEngine, container: HTMLElement, opts: EditorOptions = {}) {
    super(engine, container, opts);
    this.editorOpts = opts;
    this.buildPalette();
  }

  // ── public API ─────────────────────────────────────────────────────────────
  setTool(tool: EditTool) {
    this.tool = tool;
    for (const b of this.palette?.querySelectorAll("button[data-tool]") ?? []) {
      b.classList.toggle("gpe-on", b.getAttribute("data-tool") === tool);
    }
    if (this.selected && tool !== "select") {
      this.selected = null;
      this.redrawAll();
    }
  }
  setStyle(patch: Partial<EditStyle>) {
    this.style = { ...this.style, ...patch };
    if (this.selected) {
      this.selected.s = { ...this.selected.s, ...patch };
      this.redraw(this.selected.page);
    }
  }
  /** Edits drawn but not yet baked into the PDF. */
  get pendingEdits(): number {
    return this.els.length;
  }

  /** Delete the currently selected element. */
  removeSelected() {
    if (!this.selected) return;
    const page = this.selected.page;
    this.els = this.els.filter((e) => e !== this.selected);
    this.selected = null;
    this.redraw(page);
  }
  /** Discard all un-applied edits. */
  clearEdits() {
    this.els = [];
    this.selected = null;
    this.redrawAll();
  }

  /** Bake every pending edit into the PDF, then re-render the affected pages. */
  applyEdits(): number {
    if (!this.doc || this.els.length === 0) return 0;
    const pages = new Set<number>();
    let applied = 0;
    for (const e of this.els) {
      if (this.bake(e)) {
        pages.add(e.page);
        applied++;
      }
    }
    this.els = [];
    this.selected = null;
    for (const p of pages) this.reRenderPage(p);
    this.redrawAll();
    return applied;
  }

  // ── apply one element to the PDF (flip Y: top-left points → PDF bottom-left) ──
  private bake(e: El): boolean {
    const doc = this.doc!;
    const H = this.pageHeightPt(e.page);
    const nx = Math.min(e.x, e.x + e.w);
    const ny = Math.min(e.y, e.y + e.h);
    const nw = Math.abs(e.w);
    const nh = Math.abs(e.h);
    const { color, fill, lineWidth, opacity } = e.s;
    switch (e.kind) {
      case "rect":
        return doc.addRectangle(e.page, nx, H - (ny + nh), nw, nh, color, fill, lineWidth, opacity);
      case "ellipse":
        return doc.addEllipse(e.page, nx + nw / 2, H - (ny + nh / 2), nw / 2, nh / 2, color, fill, lineWidth, opacity);
      case "line":
        return doc.addPolygon(e.page, [e.x, H - e.y, e.x + e.w, H - (e.y + e.h)], false, color, null, lineWidth, opacity);
      case "ink": {
        const flat: number[] = [];
        const pts = e.pts ?? [];
        for (let i = 0; i + 1 < pts.length; i += 2) {
          flat.push(pts[i]!, H - pts[i + 1]!);
        }
        return flat.length >= 4 ? doc.addPolygon(e.page, flat, false, color, null, lineWidth, opacity) : false;
      }
      case "text": {
        const id = this.ensureFont();
        if (id === null || !e.text) return false;
        return doc.addText(e.page, e.x, H - e.y, e.s.fontSize, e.text, id, color);
      }
      case "image":
        return e.data ? doc.addImage(e.page, e.data, nx, H - (ny + nh), nw, nh, opacity) : false;
      case "highlight":
        return doc.addRectangle(e.page, nx, H - (ny + nh), nw, nh, null, color, 0, 0.4);
      case "redact":
        return doc.redact(e.page, nx, H - (ny + nh), nw, nh, 0, false) >= 0;
      default:
        return false;
    }
  }

  private ensureFont(): number | null {
    if (this.fontObj !== null) return this.fontObj;
    const f = this.editorOpts.defaultFont;
    if (!f || !this.doc) return null;
    this.fontObj = this.doc.embedFont(f.family, f.ttf);
    return this.fontObj;
  }

  // ── overlays (rebuilt after each (re)render) ─────────────────────────────────
  protected override afterRender() {
    this.overlays = new Array(this.count + 1).fill(null);
    this.guides = new Array(this.count + 1).fill(null);
    const d = this.root.ownerDocument;
    for (let p = 1; p <= this.count; p++) {
      const box = this.pageEls[p];
      if (!box) continue;
      box.style.position = "relative";
      // 1) drawing surface — captures the editing tools.
      const svg = d.createElementNS(SVG_NS, "svg") as SVGSVGElement;
      svg.setAttribute("viewBox", `0 0 ${this.pageWidthPt(p)} ${this.pageHeightPt(p)}`);
      svg.setAttribute("preserveAspectRatio", "none");
      svg.style.cssText = "position:absolute;inset:0;width:100%;height:100%;touch-action:none";
      svg.dataset.page = String(p);
      box.appendChild(svg);
      this.overlays[p] = svg;
      this.bindOverlay(svg, p);
      // 2) rulers + margin guides — on top, click-through except the drag handles.
      const g = d.createElementNS(SVG_NS, "svg") as SVGSVGElement;
      g.setAttribute("viewBox", `0 0 ${this.pageWidthPt(p)} ${this.pageHeightPt(p)}`);
      g.setAttribute("preserveAspectRatio", "none");
      g.style.cssText = "position:absolute;inset:0;width:100%;height:100%;pointer-events:none;overflow:visible";
      g.dataset.page = String(p);
      box.appendChild(g);
      this.guides[p] = g;
    }
    this.redrawAll();
    this.redrawGuidesAll();
  }

  /** Keep ruler ticks / handles a constant on-screen size as the zoom changes. */
  protected override onZoomChange() {
    this.redrawGuidesAll();
  }

  // ── rulers & margins (public API) ────────────────────────────────────────────
  /** Set page margins (default unit: millimetres). */
  setMargins(m: Partial<Record<Side, number>>, unit: "mm" | "pt" = "mm") {
    const k = unit === "mm" ? PT_PER_MM : 1;
    for (const side of ["top", "right", "bottom", "left"] as Side[]) {
      const v = m[side];
      if (typeof v === "number" && isFinite(v)) this.margins[side] = Math.max(0, v * k);
    }
    this.syncMarginInputs();
    this.redrawGuidesAll();
  }
  /** Current margins in the requested unit (millimetres by default). */
  getMargins(unit: "mm" | "pt" = "mm"): Record<Side, number> {
    const k = unit === "mm" ? PT_PER_MM : 1;
    return {
      top: this.margins.top / k,
      right: this.margins.right / k,
      bottom: this.margins.bottom / k,
      left: this.margins.left / k,
    };
  }
  /** Show or hide the rulers and margin guides. */
  showRulers(on: boolean) {
    this.showGuides = on;
    this.redrawGuidesAll();
  }

  private syncMarginInputs() {
    for (const side of ["top", "right", "bottom", "left"] as Side[]) {
      const inp = this.marginInputs[side];
      if (inp) inp.value = String(Math.round(this.margins[side] / PT_PER_MM));
    }
  }

  // ── ruler / margin drawing ───────────────────────────────────────────────────
  private redrawGuidesAll() {
    for (let p = 1; p <= this.count; p++) this.redrawGuides(p);
  }

  private redrawGuides(page: number) {
    const g = this.guides[page];
    if (!g) return;
    g.replaceChildren();
    if (!this.showGuides) return;
    const W = this.pageWidthPt(page);
    const H = this.pageHeightPt(page);
    if (!W || !H) return;
    const d = this.root.ownerDocument;
    const z = this.zoom || 1;
    const band = 14 / z; // ruler thickness ≈14 px on screen
    const fs = 8 / z; // tick-label size ≈8 px

    const line = (x1: number, y1: number, x2: number, y2: number, stroke: string, dash?: string) => {
      const l = d.createElementNS(SVG_NS, "line");
      l.setAttribute("x1", String(x1));
      l.setAttribute("y1", String(y1));
      l.setAttribute("x2", String(x2));
      l.setAttribute("y2", String(y2));
      l.setAttribute("stroke", stroke);
      l.setAttribute("stroke-width", "1");
      l.setAttribute("vector-effect", "non-scaling-stroke");
      if (dash) l.setAttribute("stroke-dasharray", dash);
      g.appendChild(l);
    };
    const label = (x: number, y: number, text: string) => {
      const t = d.createElementNS(SVG_NS, "text");
      t.setAttribute("x", String(x));
      t.setAttribute("y", String(y));
      t.setAttribute("font-size", String(fs));
      t.setAttribute("fill", "#c9ced2");
      t.setAttribute("font-family", "system-ui,sans-serif");
      t.textContent = text;
      g.appendChild(t);
    };
    const bandRect = (w: number, h: number) => {
      const r = d.createElementNS(SVG_NS, "rect");
      r.setAttribute("x", "0");
      r.setAttribute("y", "0");
      r.setAttribute("width", String(w));
      r.setAttribute("height", String(h));
      r.setAttribute("fill", "rgba(38,42,45,0.86)");
      g.appendChild(r);
    };

    // ruler bands (top + left)
    bandRect(W, band);
    bandRect(band, H);

    // ticks every 5 mm (minor) / 10 mm (major + mm label)
    const step = PT_PER_MM * 5;
    for (let x = 0, m = 0; x <= W + 0.5; x += step, m += 5) {
      const major = m % 10 === 0;
      line(x, 0, x, major ? band : band * 0.55, "#7a8085");
      if (major && m > 0) label(x + 1 / z, band - 1.5 / z, String(m));
    }
    for (let y = 0, m = 0; y <= H + 0.5; y += step, m += 5) {
      const major = m % 10 === 0;
      line(0, y, major ? band : band * 0.55, y, "#7a8085");
      if (major && m > 0) label(1 / z, y - 1 / z, String(m));
    }

    // margin guides across the page (dashed blue)
    const ml = this.margins.left;
    const mr = W - this.margins.right;
    const mt = this.margins.top;
    const mb = H - this.margins.bottom;
    line(ml, 0, ml, H, "#3b82f6", "5 3");
    line(mr, 0, mr, H, "#3b82f6", "5 3");
    line(0, mt, W, mt, "#3b82f6", "5 3");
    line(0, mb, W, mb, "#3b82f6", "5 3");

    // draggable handles, sitting in the ruler bands
    const hs = 11 / z;
    this.marginHandle(g, page, "left", ml, band / 2, hs, "ew-resize", W, H);
    this.marginHandle(g, page, "right", mr, band / 2, hs, "ew-resize", W, H);
    this.marginHandle(g, page, "top", band / 2, mt, hs, "ns-resize", W, H);
    this.marginHandle(g, page, "bottom", band / 2, mb, hs, "ns-resize", W, H);
  }

  private marginHandle(
    g: SVGSVGElement,
    page: number,
    side: Side,
    cx: number,
    cy: number,
    size: number,
    cursor: string,
    W: number,
    H: number
  ) {
    const d = this.root.ownerDocument;
    const r = d.createElementNS(SVG_NS, "rect");
    r.setAttribute("x", String(cx - size / 2));
    r.setAttribute("y", String(cy - size / 2));
    r.setAttribute("width", String(size));
    r.setAttribute("height", String(size));
    r.setAttribute("rx", String(size * 0.25));
    r.setAttribute("fill", "#3b82f6");
    r.setAttribute("stroke", "#fff");
    r.setAttribute("stroke-width", "1");
    r.setAttribute("vector-effect", "non-scaling-stroke");
    r.style.cssText = `pointer-events:all;cursor:${cursor}`;
    r.addEventListener("pointerdown", (e: PointerEvent) => {
      // Capture on the *guide layer* (stable across redraws), not the handle —
      // each drag redraws and replaces this rect, which would drop its capture.
      e.stopPropagation();
      e.preventDefault();
      try {
        g.setPointerCapture(e.pointerId);
      } catch {
        /* capture is best-effort */
      }
      const move = (ev: PointerEvent) => {
        const pp = this.toPagePt(g, ev.clientX, ev.clientY);
        const val =
          side === "left" ? pp.x : side === "right" ? W - pp.x : side === "top" ? pp.y : H - pp.y;
        this.dragMargin(page, side, val);
      };
      const up = (ev: PointerEvent) => {
        try {
          g.releasePointerCapture(ev.pointerId);
        } catch {
          /* already released */
        }
        g.removeEventListener("pointermove", move);
        g.removeEventListener("pointerup", up);
        g.removeEventListener("pointercancel", up);
      };
      g.addEventListener("pointermove", move);
      g.addEventListener("pointerup", up);
      g.addEventListener("pointercancel", up);
    });
    g.appendChild(r);
  }

  private dragMargin(page: number, side: Side, val: number) {
    const W = this.pageWidthPt(page);
    const H = this.pageHeightPt(page);
    const lim = side === "left" || side === "right" ? W : H;
    const opp =
      side === "left"
        ? this.margins.right
        : side === "right"
          ? this.margins.left
          : side === "top"
            ? this.margins.bottom
            : this.margins.top;
    this.margins[side] = Math.max(0, Math.min(val, lim - opp - 8));
    this.syncMarginInputs();
    this.redrawGuidesAll();
  }

  private toPagePt(svg: SVGSVGElement, clientX: number, clientY: number): { x: number; y: number } {
    const m = svg.getScreenCTM();
    if (!m) return { x: 0, y: 0 };
    const pt = new DOMPoint(clientX, clientY).matrixTransform(m.inverse());
    return { x: pt.x, y: pt.y };
  }

  private bindOverlay(svg: SVGSVGElement, page: number) {
    let draft: El | null = null;
    let origin: { x: number; y: number } | null = null;
    let moving: { el: El; dx: number; dy: number } | null = null;

    svg.addEventListener("pointerdown", (e: PointerEvent) => {
      svg.setPointerCapture(e.pointerId);
      const p = this.toPagePt(svg, e.clientX, e.clientY);
      if (this.tool === "select") {
        const hit = this.hitTest(page, p);
        this.selected = hit;
        if (hit) moving = { el: hit, dx: p.x - hit.x, dy: p.y - hit.y };
        this.redraw(page);
        return;
      }
      if (this.tool === "text") {
        this.placeText(page, p);
        return;
      }
      if (this.tool === "image") {
        void this.placeImage(page, p);
        return;
      }
      origin = p;
      draft = {
        id: this.idSeq++,
        page,
        kind: this.tool,
        x: p.x,
        y: p.y,
        w: 0,
        h: 0,
        s: { ...this.style },
        ...(this.tool === "ink" ? { pts: [p.x, p.y] } : {}),
      };
      this.els.push(draft);
    });

    svg.addEventListener("pointermove", (e: PointerEvent) => {
      const p = this.toPagePt(svg, e.clientX, e.clientY);
      if (moving) {
        moving.el.x = p.x - moving.dx;
        moving.el.y = p.y - moving.dy;
        this.redraw(page);
        return;
      }
      if (!draft || !origin) return;
      if (draft.kind === "ink") {
        draft.pts!.push(p.x, p.y);
      } else {
        draft.x = origin.x;
        draft.y = origin.y;
        draft.w = p.x - origin.x;
        draft.h = p.y - origin.y;
      }
      this.redraw(page);
    });

    const end = () => {
      if (draft && !this.isMeaningful(draft)) {
        this.els = this.els.filter((x) => x !== draft);
        this.redraw(page);
      } else if (draft) {
        this.selected = draft;
        this.redraw(page);
      }
      draft = null;
      origin = null;
      moving = null;
    };
    svg.addEventListener("pointerup", end);
    svg.addEventListener("pointercancel", end);
  }

  private isMeaningful(e: El): boolean {
    if (e.kind === "ink") return (e.pts?.length ?? 0) >= 4;
    return Math.abs(e.w) > 1 || Math.abs(e.h) > 1;
  }

  private hitTest(page: number, p: { x: number; y: number }): El | null {
    // Topmost element whose bbox contains the point.
    for (let i = this.els.length - 1; i >= 0; i--) {
      const e = this.els[i]!;
      if (e.page !== page) continue;
      const nx = Math.min(e.x, e.x + e.w);
      const ny = Math.min(e.y, e.y + e.h);
      const nw = Math.abs(e.w) || 12;
      const nh = Math.abs(e.h) || 12;
      if (p.x >= nx - 4 && p.x <= nx + nw + 4 && p.y >= ny - 4 && p.y <= ny + nh + 4) return e;
    }
    return null;
  }

  private placeText(page: number, p: { x: number; y: number }) {
    const text = this.root.ownerDocument.defaultView?.prompt("Text:") ?? "";
    if (!text.trim()) return;
    this.els.push({ id: this.idSeq++, page, kind: "text", x: p.x, y: p.y + this.style.fontSize, w: 0, h: 0, text, s: { ...this.style } });
    this.redraw(page);
  }

  private async placeImage(page: number, p: { x: number; y: number }) {
    const d = this.root.ownerDocument;
    const input = d.createElement("input");
    input.type = "file";
    input.accept = "image/png,image/jpeg";
    input.addEventListener("change", async () => {
      const file = input.files?.[0];
      if (!file) return;
      const data = new Uint8Array(await file.arrayBuffer());
      const url = URL.createObjectURL(file);
      this.els.push({ id: this.idSeq++, page, kind: "image", x: p.x, y: p.y, w: 120, h: 120, data, imgUrl: url, s: { ...this.style } });
      this.redraw(page);
    });
    input.click();
  }

  // ── overlay drawing ──────────────────────────────────────────────────────────
  private redrawAll() {
    for (let p = 1; p <= this.count; p++) this.redraw(p);
  }

  private redraw(page: number) {
    const svg = this.overlays[page];
    if (!svg) return;
    svg.replaceChildren();
    const d = this.root.ownerDocument;
    for (const e of this.els) {
      if (e.page !== page) continue;
      const node = this.elNode(d, e);
      if (node) svg.appendChild(node);
    }
    if (this.selected && this.selected.page === page) {
      const nx = Math.min(this.selected.x, this.selected.x + this.selected.w);
      const ny = Math.min(this.selected.y, this.selected.y + this.selected.h);
      const sel = d.createElementNS(SVG_NS, "rect");
      sel.setAttribute("x", String(nx - 3));
      sel.setAttribute("y", String(ny - 3));
      sel.setAttribute("width", String((Math.abs(this.selected.w) || 12) + 6));
      sel.setAttribute("height", String((Math.abs(this.selected.h) || 12) + 6));
      sel.setAttribute("fill", "none");
      sel.setAttribute("stroke", "#3b82f6");
      sel.setAttribute("stroke-dasharray", "4 3");
      sel.setAttribute("stroke-width", "1");
      svg.appendChild(sel);
    }
  }

  private elNode(d: Document, e: El): SVGElement | null {
    const stroke = hex(e.s.color);
    const fill = e.s.fill === null ? "none" : hex(e.s.fill);
    const nx = Math.min(e.x, e.x + e.w);
    const ny = Math.min(e.y, e.y + e.h);
    const nw = Math.abs(e.w);
    const nh = Math.abs(e.h);
    switch (e.kind) {
      case "rect":
      case "highlight":
      case "redact": {
        const r = d.createElementNS(SVG_NS, "rect");
        r.setAttribute("x", String(nx));
        r.setAttribute("y", String(ny));
        r.setAttribute("width", String(nw));
        r.setAttribute("height", String(nh));
        if (e.kind === "rect") {
          r.setAttribute("fill", fill);
          r.setAttribute("stroke", stroke);
          r.setAttribute("stroke-width", String(e.s.lineWidth));
          r.setAttribute("opacity", String(e.s.opacity));
        } else if (e.kind === "highlight") {
          r.setAttribute("fill", stroke);
          r.setAttribute("opacity", "0.4");
        } else {
          r.setAttribute("fill", "#000");
          r.setAttribute("stroke", "#f00");
          r.setAttribute("stroke-dasharray", "3 2");
          r.setAttribute("stroke-width", "0.5");
        }
        return r;
      }
      case "ellipse": {
        const el = d.createElementNS(SVG_NS, "ellipse");
        el.setAttribute("cx", String(nx + nw / 2));
        el.setAttribute("cy", String(ny + nh / 2));
        el.setAttribute("rx", String(nw / 2));
        el.setAttribute("ry", String(nh / 2));
        el.setAttribute("fill", fill);
        el.setAttribute("stroke", stroke);
        el.setAttribute("stroke-width", String(e.s.lineWidth));
        el.setAttribute("opacity", String(e.s.opacity));
        return el;
      }
      case "line": {
        const ln = d.createElementNS(SVG_NS, "line");
        ln.setAttribute("x1", String(e.x));
        ln.setAttribute("y1", String(e.y));
        ln.setAttribute("x2", String(e.x + e.w));
        ln.setAttribute("y2", String(e.y + e.h));
        ln.setAttribute("stroke", stroke);
        ln.setAttribute("stroke-width", String(e.s.lineWidth));
        return ln;
      }
      case "ink": {
        const pl = d.createElementNS(SVG_NS, "polyline");
        const pts = e.pts ?? [];
        let s = "";
        for (let i = 0; i + 1 < pts.length; i += 2) s += `${pts[i]},${pts[i + 1]} `;
        pl.setAttribute("points", s.trim());
        pl.setAttribute("fill", "none");
        pl.setAttribute("stroke", stroke);
        pl.setAttribute("stroke-width", String(e.s.lineWidth));
        pl.setAttribute("stroke-linejoin", "round");
        pl.setAttribute("stroke-linecap", "round");
        return pl;
      }
      case "text": {
        const t = d.createElementNS(SVG_NS, "text");
        t.setAttribute("x", String(e.x));
        t.setAttribute("y", String(e.y));
        t.setAttribute("font-size", String(e.s.fontSize));
        t.setAttribute("fill", stroke);
        t.setAttribute("font-family", "sans-serif");
        t.textContent = e.text ?? "";
        return t;
      }
      case "image": {
        const im = d.createElementNS(SVG_NS, "image");
        im.setAttribute("x", String(nx));
        im.setAttribute("y", String(ny));
        im.setAttribute("width", String(nw || 120));
        im.setAttribute("height", String(nh || 120));
        if (e.imgUrl) im.setAttribute("href", e.imgUrl);
        return im;
      }
      default:
        return null;
    }
  }

  // ── tool palette ─────────────────────────────────────────────────────────────
  private buildPalette() {
    const d = this.root.ownerDocument;
    const bar = d.createElement("div");
    bar.className = "gpe-bar";
    bar.style.cssText =
      "display:flex;flex-wrap:wrap;gap:4px;align-items:center;padding:6px 10px;background:#3a3f42;border-bottom:1px solid #000;color:#eee;font:13px system-ui";
    const tools: [EditTool, string][] = [
      ["select", "▸"],
      ["text", "T"],
      ["rect", "▭"],
      ["ellipse", "◯"],
      ["line", "╱"],
      ["ink", "✎"],
      ["image", "🖼"],
      ["highlight", "▥"],
      ["redact", "█"],
    ];
    for (const [t, label] of tools) {
      const b = d.createElement("button");
      b.textContent = label;
      b.title = t;
      b.dataset.tool = t;
      b.style.cssText = "background:#4a4f52;color:#eee;border:0;border-radius:4px;padding:4px 8px;cursor:pointer";
      b.addEventListener("click", () => this.setTool(t));
      bar.appendChild(b);
    }
    const color = d.createElement("input");
    color.type = "color";
    color.value = hex(this.style.color);
    color.title = "Colour";
    color.addEventListener("input", () => this.setStyle({ color: parseInt(color.value.slice(1), 16) }));
    bar.appendChild(color);

    // Margins group: ruler toggle + four live mm inputs (T / R / B / L).
    const mg = d.createElement("span");
    mg.style.cssText = "display:flex;align-items:center;gap:3px;margin-left:8px";
    const mtog = d.createElement("input");
    mtog.type = "checkbox";
    mtog.checked = this.showGuides;
    mtog.title = "Show rulers & margins";
    mtog.addEventListener("change", () => this.showRulers(mtog.checked));
    const mlbl = d.createElement("span");
    mlbl.textContent = "Marges mm";
    mlbl.style.color = "#bbb";
    mg.append(mtog, mlbl);
    for (const side of ["top", "right", "bottom", "left"] as Side[]) {
      const inp = d.createElement("input");
      inp.type = "number";
      inp.min = "0";
      inp.value = String(Math.round(this.margins[side] / PT_PER_MM));
      inp.title = side;
      inp.style.cssText =
        "width:44px;background:#222;color:#eee;border:1px solid #000;border-radius:4px;padding:3px;text-align:center";
      inp.addEventListener("change", () => this.setMargins({ [side]: Number(inp.value) || 0 }, "mm"));
      this.marginInputs[side] = inp;
      mg.appendChild(inp);
    }
    bar.appendChild(mg);

    const sp = d.createElement("span");
    sp.style.flex = "1";
    bar.appendChild(sp);

    const apply = d.createElement("button");
    apply.textContent = "Apply";
    apply.style.cssText = "background:#2563eb;color:#fff;border:0;border-radius:4px;padding:4px 12px;cursor:pointer";
    apply.addEventListener("click", () => this.applyEdits());
    bar.appendChild(apply);

    const del = d.createElement("button");
    del.textContent = "Delete";
    del.style.cssText = "background:#4a4f52;color:#eee;border:0;border-radius:4px;padding:4px 10px;cursor:pointer";
    del.addEventListener("click", () => this.removeSelected());
    bar.appendChild(del);

    // Insert below the viewer toolbar (first child) if present, else at the top.
    const style = d.createElement("style");
    style.textContent = ".gpe-bar button.gpe-on{outline:2px solid #3b82f6}";
    d.head.appendChild(style);
    this.root.insertBefore(bar, this.root.children[1] ?? null);
    this.palette = bar;
    this.setTool("select");
  }
}
