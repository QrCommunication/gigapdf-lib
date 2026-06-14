import { describe, it, expect, beforeAll } from "vitest";
import { GigaPdfEngine } from "../src/index";

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
});
