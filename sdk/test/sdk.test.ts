import { readFileSync } from "node:fs";
import { describe, it, expect, beforeAll } from "vitest";
import { GigaPdfEngine } from "../src/index";

// Real OpenSSL-3 PKCS#12 (PBES2/AES + HMAC-SHA256), password "gigapdf".
const MODERN_P12 = new Uint8Array(
  readFileSync(new URL("../../crates/core/src/sign/fixtures/modern.p12", import.meta.url))
);

// Exercises the typed wrappers against the real bundled .wasm (loadDefault reads
// gigapdf.wasm produced by `pnpm build:wasm`). Catches wrapper-level bugs the
// engine smoke test can't (e.g. argument-arity / flag mistakes).
describe("@qrcommunication/gigapdf-lib", () => {
  let giga: GigaPdfEngine;
  beforeAll(async () => {
    giga = await GigaPdfEngine.loadDefault();
  });

  it("loads the bundled wasm", () => {
    expect(giga).toBeInstanceOf(GigaPdfEngine);
  });

  it("creates a PDF from text and reads it back", () => {
    const pdf = giga.txtToPdf("Hello gigapdf");
    expect(pdf.length).toBeGreaterThan(100);
    const doc = giga.open(pdf);
    expect(doc.pageCount()).toBe(1);
    const lines = doc.structuredText(1);
    expect(lines.some((l) => l.text.includes("Hello"))).toBe(true);
    doc.close();
  });

  it("edits (addRectangle with stroke flag) and round-trips a save", () => {
    const doc = giga.open(giga.txtToPdf("Edit me"));
    // Red stroke, no fill, 2pt — exercises the has_stroke/has_fill flags.
    expect(doc.addRectangle(1, 50, 50, 100, 40, 0xff0000, null, 2)).toBe(true);
    const out = doc.save();
    expect(out.length).toBeGreaterThan(100);
    const reopened = giga.open(out);
    expect(reopened.pageCount()).toBe(1);
    reopened.close();
    doc.close();
  });

  it("annotates and lists annotations", () => {
    const doc = giga.open(giga.txtToPdf("Annotate"));
    expect(doc.addHighlight(1, 50, 50, 150, 64, 0xffff00)).toBe(true);
    expect(doc.annotations(1).length).toBeGreaterThanOrEqual(1);
    doc.close();
  });

  it("exposes the font catalog", () => {
    const cat = giga.fontCatalog();
    expect(cat.length).toBeGreaterThan(100);
    expect(cat.some((f) => f.family === "Roboto")).toBe(true);
  });

  it("converts to DOCX (zip magic)", () => {
    const doc = giga.open(giga.txtToPdf("To Word"));
    const docx = doc.toDocx();
    expect(docx[0]).toBe(0x50); // 'P'
    expect(docx[1]).toBe(0x4b); // 'K'
    doc.close();
  });

  it("manages optional-content layers (calques)", () => {
    const doc = giga.open(giga.txtToPdf("Layers"));
    expect(doc.layers()).toHaveLength(0);
    const id = doc.addLayer("Watermark");
    expect(id).toBeGreaterThan(0);
    expect(doc.layers()[0]).toMatchObject({ name: "Watermark", visible: true, locked: false });
    expect(doc.setLayerVisibility(id, false)).toBe(true);
    expect(doc.setLayerLocked(id, true)).toBe(true);
    expect(doc.layers()[0]).toMatchObject({ visible: false, locked: true });
    expect(doc.removeLayer(id)).toBe(true);
    expect(doc.layers()).toHaveLength(0);
    doc.close();
  });

  it("page ops: resize, add blank, copy", () => {
    const doc = giga.open(giga.txtToPdf("Page ops"));
    expect(doc.pageCount()).toBe(1);
    expect(doc.resizePage(1, 200, 300)).toBe(true);
    expect(doc.addPage(400, 500, 1)).toBeGreaterThan(0);
    expect(doc.pageCount()).toBe(2);
    expect(doc.copyPage(1)).toBeGreaterThan(0);
    expect(doc.pageCount()).toBe(3);
    const reopened = giga.open(doc.save());
    expect(reopened.pageCount()).toBe(3);
    reopened.close();
    doc.close();
  });

  it("draws shapes (line, ellipse, polygon, svg path) and embeds an image", () => {
    const doc = giga.open(giga.txtToPdf("Shapes"));
    expect(doc.drawLine(1, 10, 10, 100, 100, 0x0000ff, 2)).toBe(true);
    // Translucent ellipse exercises the /ExtGState opacity path.
    expect(doc.addEllipse(1, 150, 150, 40, 25, 0x00ff00, 0xffeeaa, 1, 0.5)).toBe(true);
    expect(doc.addPolygon(1, [200, 200, 260, 200, 230, 260], true, 0x000000, 0xff0000)).toBe(true);
    expect(doc.addPath(1, "M 0 0 L 50 0 L 25 40 Z", 300, 400, 0x123456, null, 1.5)).toBe(true);
    // Embed a real PNG: render the page to PNG, then place it back as an image.
    const png = doc.renderPage(1, 1);
    expect(png[0]).toBe(0x89); // PNG magic
    expect(doc.addImage(1, png, 50, 500, 120, 80, 0.8)).toBe(true);
    const reopened = giga.open(doc.save());
    expect(reopened.pageCount()).toBe(1);
    reopened.close();
    doc.close();
  });

  it("converts PDF ↔ ODP (presentation) both ways", () => {
    const doc = giga.open(giga.txtToPdf("Slide one"));
    const odp = doc.toOdp();
    // ODP is a zip (PK magic) carrying the OpenDocument presentation mimetype.
    expect(odp[0]).toBe(0x50);
    expect(odp[1]).toBe(0x4b);
    // Reverse: ODP → PDF, format auto-detected by officeToPdf.
    const pdf = giga.officeToPdf(odp);
    expect(new TextDecoder().decode(pdf.slice(0, 5))).toBe("%PDF-");
    doc.close();
  });

  it("renders HTML→PDF with the native engine and lists needed Google fonts", () => {
    const html = `<style>body{font-family:Roboto;color:#333}</style>
      <body><h1>Invoice</h1><p>Hello <b>world</b> — rendered natively, no browser.</p>
      <table><tr><td>A</td><td>B</td></tr></table></body>`;
    // Phase 1: which Google fonts to fetch.
    const fonts = giga.htmlNeededFonts(html);
    expect(fonts.some((f) => /roboto/i.test(f.family) && f.url.length > 0)).toBe(true);
    // Phase 2: render (no font bytes supplied → layout/backgrounds still produce a valid PDF).
    const pdf = giga.htmlRender(html, [], 612, 792, 36);
    expect(new TextDecoder().decode(pdf.slice(0, 5))).toBe("%PDF-");
    expect(pdf.length).toBeGreaterThan(200);
  });

  it("signs with a PKCS#12 identity (native import, no node-forge)", () => {
    const doc = giga.open(giga.txtToPdf("Sign me with a real cert"));
    const signed = doc.signP12(MODERN_P12, "gigapdf", {
      name: "Tester",
      reason: "Approval",
      date: "D:20260616120000Z",
      location: "Paris",
    });
    expect(new TextDecoder().decode(signed.slice(0, 5))).toBe("%PDF-");
    const text = new TextDecoder().decode(signed);
    expect(text.includes("adbe.pkcs7.detached")).toBe(true);
    expect(text.includes("/Location")).toBe(true);
    // The signed PDF re-opens as a structurally valid document.
    const reopened = giga.open(signed);
    expect(reopened.pageCount()).toBe(1);
    reopened.close();
    doc.close();
  });

  it("rejects a wrong PKCS#12 password with a generic error", () => {
    const doc = giga.open(giga.txtToPdf("x"));
    expect(() => doc.signP12(MODERN_P12, "wrong", { reason: "R" })).toThrow(
      /PKCS#12 signing failed/
    );
    doc.close();
  });
});
