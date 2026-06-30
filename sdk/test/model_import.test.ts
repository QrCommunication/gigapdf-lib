import { describe, it, expect, beforeAll } from "vitest";
import { GigaPdfEngine } from "../src/index";
import type { GigaBlock } from "../src/index";

// Exercises the model-producing importers that complete the bidirectional
// conversion matrix: `rtfToModel` / `txtToModel` / `imageToModel` are the
// inverses of `modelToRtf` / `to_text` / image embedding, mirroring the
// already-shipped `officeToModel` / `htmlToModel` / `mdToModel` / `csvToModel`.
// Every assertion runs through the production WASM binary.

describe("model importers (production WASM)", () => {
  let giga: GigaPdfEngine;
  beforeAll(async () => {
    giga = await GigaPdfEngine.loadDefault();
  });

  // Collect every paragraph run's text across the whole model tree.
  const allText = (blocks: GigaBlock[]): string => {
    let out = "";
    for (const b of blocks) {
      switch (b.kind.t) {
        case "paragraph":
          for (const r of b.kind.v.runs) if (r.t === "run") out += r.v.text;
          break;
        case "heading":
          for (const r of b.kind.v.para.runs) if (r.t === "run") out += r.v.text;
          break;
        case "list":
          for (const item of b.kind.v.items) out += allText(item.blocks);
          break;
        case "table":
          for (const row of b.kind.v.rows)
            for (const cell of row.cells) out += allText(cell.blocks);
          break;
        default:
          break;
      }
    }
    return out;
  };

  const firstPageBlocks = (model: ReturnType<typeof giga.txtToModel>): GigaBlock[] =>
    model.sections[0]?.pages[0]?.blocks ?? [];

  it("txtToModel: one paragraph per line, text preserved", () => {
    const model = giga.txtToModel("First line\nSecond line\n\nFourth");
    expect(model.sections.length).toBeGreaterThanOrEqual(1);
    const text = allText(firstPageBlocks(model));
    expect(text).toContain("First line");
    expect(text).toContain("Second line");
    expect(text).toContain("Fourth");
  });

  it("txtToModel → modelToPdf round-trips into a valid PDF", () => {
    const model = giga.txtToModel("Round trip me");
    const pdf = giga.modelToPdf(model);
    expect(new TextDecoder().decode(pdf.slice(0, 5))).toBe("%PDF-");
    const doc = giga.open(pdf);
    expect(doc.pageCount()).toBeGreaterThanOrEqual(1);
    doc.close();
  });

  it("rtfToModel: recovers plain text from a minimal RTF document", () => {
    const rtf = "{\\rtf1\\ansi Hello \\b bold\\b0 world.\\par}";
    const model = giga.rtfToModel(rtf);
    const text = allText(firstPageBlocks(model));
    expect(text).toContain("Hello");
    expect(text).toContain("bold");
    expect(text).toContain("world");
  });

  it("rtfToModel → modelToRtf is a stable round-trip (text survives)", () => {
    const rtf = "{\\rtf1\\ansi Stable text here.\\par}";
    const model = giga.rtfToModel(rtf);
    const back = giga.modelToRtf(model);
    expect(back).toContain("Stable text here");
  });

  it("imageToModel: wraps a PNG into a single full-page picture block", () => {
    const rgba = new Uint8Array([
      255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
    ]);
    const png = giga.rgbaToPng(rgba, 2, 2);
    const model = giga.imageToModel(png);
    expect(model).not.toBeNull();
    const blocks = firstPageBlocks(model!);
    const imageBlocks = blocks.filter((b) => b.kind.t === "image");
    expect(imageBlocks.length).toBe(1);
    // The bytes are interned in the model resource table.
    const res = model!.resources as { images?: Record<string, unknown> } | undefined;
    expect(Object.keys(res?.images ?? {}).length).toBeGreaterThanOrEqual(1);
  });

  it("imageToModel → modelToPdf re-embeds the picture into a valid PDF", () => {
    const rgba = new Uint8Array([
      255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
    ]);
    const model = giga.imageToModel(giga.rgbaToPng(rgba, 2, 2));
    expect(model).not.toBeNull();
    const pdf = giga.modelToPdf(model!);
    expect(new TextDecoder().decode(pdf.slice(0, 5))).toBe("%PDF-");
  });

  it("imageToModel rejects non-image bytes (null)", () => {
    expect(giga.imageToModel(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]))).toBeNull();
  });
});
