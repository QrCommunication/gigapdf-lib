import { readFileSync } from "node:fs";
import { describe, it, expect, beforeAll } from "vitest";
import { GigaPdfEngine } from "../src/index";
import type { GigaBlock, GigaInline } from "../src/index";

// Real OpenSSL-3 PKCS#12 (PBES2/AES + HMAC-SHA256), password "gigapdf".
const MODERN_P12 = new Uint8Array(
  readFileSync(new URL("../../crates/core/src/sign/fixtures/modern.p12", import.meta.url))
);

// A PDF carrying an embedded DejaVu TrueType program.
const EMBEDDED_FONTS_PDF = new Uint8Array(
  readFileSync(new URL("../../fixtures/embedded-fonts.pdf", import.meta.url))
);

// ── tiny DER helpers for the timestamp test (build a mock TimeStampResp) ──────
function derLen(len: number): number[] {
  if (len < 0x80) return [len];
  const bytes: number[] = [];
  let n = len;
  while (n > 0) {
    bytes.unshift(n & 0xff);
    n >>= 8;
  }
  return [0x80 | bytes.length, ...bytes];
}
function derTlv(tag: number, content: number[] | Uint8Array): Uint8Array {
  const body = Array.from(content);
  return new Uint8Array([tag, ...derLen(body.length), ...body]);
}
function derInt(value: number): Uint8Array {
  return derTlv(0x02, [value]); // small non-negative integers only
}
function derSeq(members: Uint8Array[]): Uint8Array {
  const body: number[] = [];
  for (const m of members) body.push(...m);
  return derTlv(0x30, body);
}

/** Recover the exact CMS `ContentInfo` DER from a signed PDF's `/Contents <hex>`
 *  window, trimming the `00` right-padding by reading the outer SEQUENCE length. */
function extractContentsCms(pdf: Uint8Array): Uint8Array {
  const text = new TextDecoder("latin1").decode(pdf);
  const lt = text.indexOf("/Contents <");
  const start = lt + "/Contents <".length;
  const gt = text.indexOf(">", start);
  const hex = text.slice(start, gt);
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  // Outer DER: tag (0x30) + length octets → total length, dropping padding.
  if (bytes[0] !== 0x30) throw new Error("not a SEQUENCE");
  let lenLen = 1;
  let len = bytes[1];
  if (len & 0x80) {
    const n = len & 0x7f;
    len = 0;
    for (let i = 0; i < n; i++) len = (len << 8) | bytes[2 + i];
    lenLen = 1 + n;
  }
  return bytes.slice(0, 1 + lenLen + len);
}

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

  it("extracts rich text elements (font, size, colour, bounds)", () => {
    const doc = giga.open(giga.txtToPdf("Rich text"));
    const els = doc.textElements(1);
    expect(els.length).toBeGreaterThan(0);
    const e = els.find((el) => el.text.includes("Rich")) ?? els[0]!;
    expect(typeof e.fontFamily).toBe("string");
    expect(e.fontFamily.length).toBeGreaterThan(0);
    expect(e.fontSize).toBeGreaterThan(0);
    expect(e.color).toHaveLength(3);
    expect(Number.isFinite(e.x) && Number.isFinite(e.y)).toBe(true);
    // index is the text-run index that replaceText accepts.
    expect(Number.isInteger(e.index)).toBe(true);
    doc.close();
  });

  it("extracts image elements (placement + bytes + format)", () => {
    const doc = giga.open(giga.txtToPdf("Has an image"));
    // 2x2 opaque RGB → PNG, placed on the page.
    const rgba = new Uint8Array([255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255]);
    doc.addImage(1, giga.rgbaToPng(rgba, 2, 2), 40, 500, 60, 60, 1);
    const imgs = doc.imageElements(1);
    expect(imgs.length).toBeGreaterThan(0);
    const img = imgs[0]!;
    expect(img.pixelWidth).toBe(2);
    expect(img.pixelHeight).toBe(2);
    expect(["png", "jpeg", "jp2"]).toContain(img.format);
    expect(img.data.length).toBeGreaterThan(0);
    expect(img.width).toBeGreaterThan(0);
    doc.close();
  });

  it("imageElements/vectorPaths report the unified index (round-trips through edits)", () => {
    // Mixed page in stream order: text → image → path → image.
    const doc = giga.open(giga.txtToPdf("mixed"));
    const rgba = new Uint8Array([255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255]);
    const png = giga.rgbaToPng(rgba, 2, 2);
    expect(doc.addImage(1, png, 40, 600, 60, 60, 1)).toBe(true); // image #1
    expect(doc.addRectangle(1, 100, 400, 80, 60, null, 0x0000ff, 1)).toBe(true); // filled path
    expect(doc.addImage(1, png, 300, 200, 40, 40, 1)).toBe(true); // image #2

    const imgs = doc.imageElements(1);
    expect(imgs.length).toBe(2);
    // Image-local would be 0 and 1; the two images' unified indices must differ
    // and the 2nd must be strictly greater than the 1st (the path sits between).
    expect(imgs[0].index).toBeLessThan(imgs[1].index);
    expect(imgs[1].index - imgs[0].index).toBeGreaterThanOrEqual(2);

    const paths = doc.vectorPaths(1);
    expect(paths.length).toBe(1);
    const pathIdx = paths[0]!.index;
    // The path's unified index sits between the two image indices (text,img,path,img).
    expect(pathIdx).toBeGreaterThan(imgs[0].index);
    expect(pathIdx).toBeLessThan(imgs[1].index);

    // Restyle THAT path by its reported index → fill turns green.
    expect(doc.setPathStyle(1, pathIdx, { fill: [0, 1, 0] })).toBe(true);
    expect(doc.vectorPaths(1)[0]!.fill).toEqual([0, 1, 0]);

    // Remove the 2nd image by its reported (unified) index → only it goes.
    expect(doc.removeElement(1, imgs[1].index)).toBe(true);
    const after = doc.imageElements(1);
    expect(after.length).toBe(1);
    expect(after[0].index).toBe(imgs[0].index); // the FIRST image survived
    // The path is still present too.
    expect(doc.vectorPaths(1).length).toBe(1);
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

  it("adds line and arrow (/Line + /LE OpenArrow) annotations", () => {
    const doc = giga.open(giga.txtToPdf("Point at me"));
    expect(doc.addLineAnnotation(1, 50, 50, 250, 50, 0x000000, 1.5)).toBe(true);
    expect(doc.addLineAnnotation(1, 50, 120, 250, 200, 0xff0000, 2, true)).toBe(true);
    const out = doc.save();
    const lines = giga.open(out).annotations(1).filter((a) => a.subtype === "Line");
    expect(lines.length).toBe(2);
    // The arrow ending is recorded as /LE [/None /OpenArrow] for conforming readers.
    expect(new TextDecoder("latin1").decode(out)).toContain("OpenArrow");
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

  it("signs with a PAdES-B-T timestamp via the two-phase TSA flow", async () => {
    // Extract a genuine CMS ContentInfo from a p12-signed PDF — it stands in as
    // the TSA's TimeStampToken so the round trip needs no network.
    const probe = giga.open(giga.txtToPdf("probe"));
    const probeSigned = probe.signP12(MODERN_P12, "gigapdf", { reason: "R" });
    probe.close();
    const cms = extractContentsCms(probeSigned);

    // A granted TimeStampResp ::= SEQUENCE { PKIStatusInfo{INTEGER 0}, token }.
    const statusInfo = derSeq([derInt(0)]);
    const tsaResponse = derSeq([statusInfo, cms]);

    let capturedReq: Uint8Array | undefined;
    const doc = giga.open(giga.txtToPdf("Timestamp me"));
    const signed = await doc.signTimestamped({
      random: crypto.getRandomValues(new Uint8Array(256)),
      notBefore: "260101000000Z",
      notAfter: "360101000000Z",
      name: "Tester",
      reason: "Approval",
      date: "D:20260616120000Z",
      tsaUrl: "https://tsa.example/never-hit",
      nonce: new Uint8Array([0x01, 0x02, 0x03, 0x04]),
      tsaFetch: async (req) => {
        capturedReq = req;
        return tsaResponse;
      },
    });

    // Phase 1 handed our mock fetch a well-formed TimeStampReq SEQUENCE.
    expect(capturedReq).toBeDefined();
    expect(capturedReq![0]).toBe(0x30);
    // Phase 2 produced a PAdES PDF carrying the ETSI subfilter, not the legacy one.
    expect(new TextDecoder().decode(signed.slice(0, 5))).toBe("%PDF-");
    const text = new TextDecoder().decode(signed);
    expect(text.includes("ETSI.CAdES.detached")).toBe(true);
    expect(text.includes("adbe.pkcs7.detached")).toBe(false);
    const reopened = giga.open(signed);
    expect(reopened.pageCount()).toBe(1);
    reopened.close();
    doc.close();
  });

  it("rejects a malformed TSA token in the finish phase", async () => {
    const doc = giga.open(giga.txtToPdf("bad token"));
    await expect(
      doc.signTimestamped({
        random: crypto.getRandomValues(new Uint8Array(256)),
        notBefore: "260101000000Z",
        notAfter: "360101000000Z",
        reason: "R",
        tsaUrl: "https://tsa.example/never-hit",
        tsaFetch: async () => new Uint8Array([0x00, 0x01, 0x02]), // not a TimeStampResp
      })
    ).rejects.toThrow(/timestamped signing failed/);
    doc.close();
  });

  it("draws text in built-in base-14 standard fonts (no embedding)", () => {
    const doc = giga.open(giga.txtToPdf("base14"));
    expect(doc.addStandardText(1, 72, 700, 18, "Times Bold heading", "Times-Bold", 0x000000)).toBe(
      true
    );
    expect(doc.addStandardText(1, 72, 680, 12, "courier code", "Courier", 0x333333)).toBe(true);
    // An unknown font name is rejected.
    expect(doc.addStandardText(1, 72, 660, 12, "x", "NotARealFont")).toBe(false);
    const out = doc.save();
    expect(new TextDecoder().decode(out).includes("Times-Bold")).toBe(true);
    doc.close();
  });

  it("lists embedded fonts, extracts one, and re-embeds it to draw new text", () => {
    const doc = giga.open(EMBEDDED_FONTS_PDF);
    const fonts = doc.embeddedFonts();
    expect(fonts.length).toBeGreaterThan(0);
    const ttf = fonts.find((f) => f.format === "truetype");
    expect(ttf).toBeDefined();
    expect(/DejaVu/i.test(ttf!.baseFont)).toBe(true);

    // Pull the program out and re-embed it — drawing text in the doc's own face.
    const program = doc.extractFont(ttf!.baseFont);
    expect(program).not.toBeNull();
    expect(program!.format).toBe("truetype");
    const handle = doc.embedFont("ReusedFace", program!.bytes);
    expect(handle).toBeGreaterThan(0);
    expect(doc.addText(1, 72, 500, 14, "reused glyphs", handle)).toBe(true);
    doc.close();
  });

  it("writes a host-built grid to xlsx/ods natively (with sheet names)", () => {
    const grids = [
      [
        ["Name", "Age"],
        ["Alice", "30"],
      ],
      [["Page two"]],
    ];
    const xlsx = giga.gridsToXlsx(grids, ["People", "Notes"]);
    expect(xlsx[0]).toBe(0x50); // 'P' — XLSX is a ZIP
    expect(xlsx[1]).toBe(0x4b); // 'K'
    expect(xlsx.length).toBeGreaterThan(200);
    // Default names when none supplied still produce a valid workbook.
    expect(giga.gridsToXlsx(grids).length).toBeGreaterThan(200);
    const ods = giga.gridsToOds(grids, ["People"]);
    expect(ods[0]).toBe(0x50);
    expect(ods[1]).toBe(0x4b);
    // Malformed/empty grid yields a valid single-sheet workbook (no throw).
    expect(giga.gridsToXlsx([]).length).toBeGreaterThan(100);

    // Round-trip: read the workbook back natively (no third-party reader).
    const sheets = giga.xlsxToGrids(xlsx);
    expect(sheets.map((s) => s.name)).toEqual(["People", "Notes"]);
    expect(sheets[0]!.rows[0]).toEqual(["Name", "Age"]);
    expect(sheets[0]!.rows[1]).toEqual(["Alice", "30"]);
    expect(sheets[1]!.rows[0]![0]).toBe("Page two");
    expect(giga.xlsxToGrids(new Uint8Array([1, 2, 3]))).toEqual([]); // non-xlsx
  });

  it("encodes raw RGBA to PNG natively", () => {
    const w = 2;
    const h = 2;
    const rgba = new Uint8Array(w * h * 4).fill(255); // 2×2 opaque white
    const png = giga.rgbaToPng(rgba, w, h);
    expect(Array.from(png.slice(0, 4))).toEqual([0x89, 0x50, 0x4e, 0x47]); // ‰PNG
    expect(png.length).toBeGreaterThan(20);
    // A buffer that doesn't match width*height*4 → empty (no throw).
    expect(giga.rgbaToPng(new Uint8Array(3), w, h).length).toBe(0);
  });

  it("resizes raw RGBA natively (downscale averages, alpha-correct)", () => {
    // 2×2 (red, green, blue, white) → 1×1 averages to ~ (128,128,128).
    const src = new Uint8Array([
      255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
    ]);
    const out = giga.resizeRgba(src, 2, 2, 1, 1);
    expect(out.length).toBe(4);
    for (let c = 0; c < 3; c++) expect(Math.abs(out[c]! - 128)).toBeLessThanOrEqual(2);
    expect(out[3]).toBe(255);
    // Upscale keeps a flat colour flat; bad input → empty.
    expect(giga.resizeRgba(new Uint8Array([10, 20, 30, 255]), 1, 1, 3, 2).length).toBe(3 * 2 * 4);
    expect(giga.resizeRgba(new Uint8Array(3), 1, 1, 2, 2).length).toBe(0);
  });

  it("encodes + decodes JPEG natively (round-trip) and decodes PNG", () => {
    const w = 16;
    const h = 16;
    const rgba = new Uint8Array(w * h * 4);
    for (let i = 0; i < w * h; i++) {
      rgba[i * 4] = (i % w) * 16; // R ramp
      rgba[i * 4 + 1] = 100;
      rgba[i * 4 + 2] = 50;
      rgba[i * 4 + 3] = 255;
    }
    const jpg = giga.encodeJpeg(rgba, w, h, 92);
    expect(jpg[0]).toBe(0xff);
    expect(jpg[1]).toBe(0xd8); // JPEG SOI
    const dec = giga.decodeJpeg(jpg);
    expect(dec).not.toBeNull();
    expect(dec!.width).toBe(w);
    expect(dec!.height).toBe(h);
    expect(dec!.rgba.length).toBe(w * h * 4);
    // Lossy but close at q92.
    expect(Math.abs(dec!.rgba[4 * 100 + 1]! - 100)).toBeLessThanOrEqual(8);

    // PNG decode round-trips exactly (lossless).
    const png = giga.rgbaToPng(rgba, w, h);
    const pdec = giga.decodePng(png);
    expect(pdec).not.toBeNull();
    expect(pdec!.width).toBe(w);
    expect(Array.from(pdec!.rgba.slice(0, 4))).toEqual([0, 100, 50, 255]);
    expect(giga.decodeJpeg(new Uint8Array([1, 2, 3]))).toBeNull();

    // Lossless WebP (VP8L) round-trips exactly.
    const webp = giga.encodeWebp(rgba, w, h);
    expect(new TextDecoder().decode(webp.slice(0, 4))).toBe("RIFF");
    expect(new TextDecoder().decode(webp.slice(8, 12))).toBe("WEBP");
    const wdec = giga.decodeWebp(webp);
    expect(wdec).not.toBeNull();
    expect(wdec!.width).toBe(w);
    expect(Array.from(wdec!.rgba.slice(0, 4))).toEqual([0, 100, 50, 255]);
  });

  it("registers named destinations and resolves links that jump to them by name", () => {
    const doc = giga.open(giga.txtToPdf("Cover"));
    expect(doc.addPage(612, 792, 1)).toBeGreaterThan(0); // page 2
    expect(doc.pageCount()).toBe(2);

    expect(doc.addNamedDest("intro", 2)).toBe(true);
    expect(doc.namedDests()).toEqual([{ name: "intro", page: 2 }]);

    // A link by name resolves to its destination page…
    expect(doc.addGotoLinkNamed(1, 10, 10, 60, 30, "intro")).toBe(true);
    expect(doc.links(1).some((l) => l.kind === "page" && l.page === 2)).toBe(true);

    // …and both survive a save round-trip.
    const reopened = giga.open(doc.save());
    expect(reopened.namedDests()).toEqual([{ name: "intro", page: 2 }]);
    expect(reopened.links(1).some((l) => l.kind === "page" && l.page === 2)).toBe(true);
    reopened.close();
    doc.close();
  });

  it("converts an image (PNG) to a one-page PDF carrying the image", () => {
    // 2×2 opaque RGBA → PNG, then PNG → one-page PDF.
    const rgba = new Uint8Array([255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255]);
    const png = giga.rgbaToPng(rgba, 2, 2);
    const pdf = giga.imageToPdf(png);
    expect(new TextDecoder().decode(pdf.slice(0, 5))).toBe("%PDF-");

    const doc = giga.open(pdf);
    expect(doc.pageCount()).toBe(1);
    // The image is embedded as a real /Image XObject on the page.
    expect(doc.imageElements(1).length).toBeGreaterThan(0);
    doc.close();

    // Unrecognized bytes → empty array (no PDF produced).
    expect(giga.imageToPdf(new Uint8Array([1, 2, 3, 4])).length).toBe(0);
  });

  it("converts a transparent RGBA (type 6) PNG to a PDF (alpha → soft mask)", () => {
    // 4×4 RGBA where half the pixels are semi-transparent. Before the fix this
    // produced an EMPTY buffer; it must now yield a real one-page PDF with the
    // image embedded (the alpha channel becomes a /SMask, not flattened).
    const rgba = new Uint8Array(4 * 4 * 4);
    for (let i = 0; i < 16; i++) {
      rgba[i * 4] = (i * 9) & 0xff;
      rgba[i * 4 + 1] = (i * 5) & 0xff;
      rgba[i * 4 + 2] = (i * 3) & 0xff;
      rgba[i * 4 + 3] = i % 2 === 0 ? 96 : 255; // semi-transparent / opaque
    }
    const png = giga.rgbaToPng(rgba, 4, 4);
    const pdf = giga.imageToPdf(png);
    expect(pdf.length).toBeGreaterThan(0);
    expect(new TextDecoder().decode(pdf.slice(0, 5))).toBe("%PDF-");

    const doc = giga.open(pdf);
    expect(doc.pageCount()).toBe(1);
    expect(doc.imageElements(1).length).toBeGreaterThan(0);
    doc.close();
  });

  it("merges several PDFs into one (page count is the sum)", () => {
    const onePage = giga.txtToPdf("Single page");

    // Build a two-page PDF.
    const twoPageDoc = giga.open(giga.txtToPdf("First"));
    expect(twoPageDoc.addPage(612, 792, 1)).toBeGreaterThan(0);
    expect(twoPageDoc.pageCount()).toBe(2);
    const twoPage = twoPageDoc.save();
    twoPageDoc.close();

    const merged = giga.mergePdfs([onePage, twoPage, onePage]);
    expect(new TextDecoder().decode(merged.slice(0, 5))).toBe("%PDF-");

    const doc = giga.open(merged);
    expect(doc.pageCount()).toBe(1 + 2 + 1); // 4 pages total
    doc.close();

    // Edge cases: empty list → empty bytes; single PDF → returned unchanged.
    expect(giga.mergePdfs([]).length).toBe(0);
    expect(giga.mergePdfs([onePage])).toBe(onePage);
  });

  it("setPathStyle opacity sets a path's fill alpha (ExtGState)", () => {
    const doc = giga.open(giga.txtToPdf("Opacity"));
    expect(doc.addRectangle(1, 100, 100, 80, 60, null, 0xff0000, 1)).toBe(true);
    const idx = doc.vectorPaths(1)[0]!.index;
    // fillAlpha is now honoured end-to-end (registers an /ExtGState + injects gs).
    expect(doc.setPathStyle(1, idx, { fill: [1, 0, 0], fillAlpha: 0.5 })).toBe(true);
    const path = doc.vectorPaths(1)[0]!;
    expect(path.fill).toEqual([1, 0, 0]);
    expect(Math.abs(path.fillAlpha - 0.5)).toBeLessThan(1e-6);
    // Survives a save/reopen.
    const reopened = giga.open(doc.save());
    expect(Math.abs(reopened.vectorPaths(1)[0]!.fillAlpha - 0.5)).toBeLessThan(1e-6);
    doc.close();
    reopened.close();
  });

  it("setElementOpacity sets an image's alpha in place", () => {
    const doc = giga.open(giga.txtToPdf("Image opacity"));
    const rgba = new Uint8Array([255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255]);
    const png = giga.rgbaToPng(rgba, 2, 2);
    expect(doc.addImage(1, png, 40, 600, 60, 60, 1)).toBe(true);
    const imgIdx = doc.imageElements(1)[0]!.index;
    expect(doc.setElementOpacity(1, imgIdx, 0.25)).toBe(true);
    const img = doc.imageElements(1)[0]!;
    expect(Math.abs(img.opacity - 0.25)).toBeLessThan(1e-6);
    doc.close();
  });

  it("reorderElement changes z-order (front/back)", () => {
    const doc = giga.open(giga.txtToPdf("Z order"));
    expect(doc.addRectangle(1, 10, 10, 20, 20, null, 0xff0000, 1)).toBe(true); // shape A
    expect(doc.addRectangle(1, 50, 50, 20, 20, null, 0x00ff00, 1)).toBe(true); // shape B
    const before = doc.vectorPaths(1);
    expect(before.length).toBe(2);
    const firstIdx = before[0]!.index;
    // Bring the first shape to front; both shapes still present afterwards.
    expect(doc.reorderElement(1, firstIdx, true)).toBe(true);
    expect(doc.vectorPaths(1).length).toBe(2);
    // Send a shape to the back too.
    expect(doc.reorderElement(1, doc.vectorPaths(1)[0]!.index, false)).toBe(true);
    expect(doc.vectorPaths(1).length).toBe(2);
    doc.close();
  });

  it("renderPageExcluding omits the chosen element from the raster", () => {
    const doc = giga.open(giga.txtToPdf("Exclude me"));
    expect(doc.addRectangle(1, 100, 100, 200, 200, null, 0xff0000, 1)).toBe(true);
    const boxIdx = doc.vectorPaths(1)[0]!.index;
    const full = doc.renderPage(1, 1);
    const excluding = doc.renderPageExcluding(1, [boxIdx], 1);
    const none = doc.renderPageExcluding(1, [], 1);
    expect(full.length).toBeGreaterThan(0);
    expect(excluding.length).toBeGreaterThan(0);
    // Excluding the box changes the raster; excluding nothing equals the full render.
    expect(Buffer.from(excluding).equals(Buffer.from(full))).toBe(false);
    expect(Buffer.from(none).equals(Buffer.from(full))).toBe(true);
    doc.close();
  });

  it("pageBlocks exposes the recognised structure as a typed, narrowable tree", () => {
    // A document with a heading, bold body run, and a small table — rendered to a
    // real PDF, then reconstructed. The point of this test is that the SDK types
    // narrow on `kind.t` and expose runs/styles/levels/cells *without any cast*.
    const html =
      "<html><body>" +
      "<h1>Quarterly Report</h1>" +
      "<p>Plain intro then a <b>bold phrase</b> closing the line.</p>" +
      "<table><tr><td>Name</td><td>Total</td></tr><tr><td>Alice</td><td>42</td></tr></table>" +
      "</body></html>";
    const doc = giga.open(giga.htmlToPdf(html));
    const blocks: GigaBlock[] = doc.pageBlocks(1);
    expect(blocks.length).toBeGreaterThan(0);

    // Walk the typed tree, narrowing purely on the `t` discriminant. Collect proof
    // that runs/levels/cells are reachable with full typing (no `as`, no `any`).
    let sawTypedRun = false;
    let sawHeadingLevel = false;
    let sawTableCell = false;

    const visitInlines = (runs: GigaInline[]): void => {
      for (const inline of runs) {
        if (inline.t === "run") {
          // `inline.v` is GigaInlineRun: these field accesses are type-checked.
          expect(typeof inline.v.text).toBe("string");
          expect(typeof inline.v.style.bold).toBe("boolean");
          expect(typeof inline.v.style.italic).toBe("boolean");
          expect(typeof inline.v.style.underline).toBe("boolean");
          expect(typeof inline.v.style.size_pt).toBe("number");
          // color is a tuple or null — both are valid typed shapes.
          expect(inline.v.style.color === null || inline.v.style.color.length === 3).toBe(true);
          sawTypedRun = true;
        } else if (inline.t === "link") {
          visitInlines(inline.children);
        }
      }
    };

    const visitBlocks = (bs: GigaBlock[]): void => {
      for (const b of bs) {
        switch (b.kind.t) {
          case "paragraph":
            visitInlines(b.kind.v.runs);
            break;
          case "heading":
            expect(typeof b.kind.v.level).toBe("number");
            expect(b.kind.v.level).toBeGreaterThanOrEqual(1);
            sawHeadingLevel = true;
            visitInlines(b.kind.v.para.runs);
            break;
          case "table":
            for (const row of b.kind.v.rows) {
              for (const cell of row.cells) {
                expect(typeof cell.col_span).toBe("number");
                expect(typeof cell.row_span).toBe("number");
                visitBlocks(cell.blocks); // cell content is itself a block tree
                sawTableCell = true;
              }
            }
            break;
          case "list":
            for (const item of b.kind.v.items) visitBlocks(item.blocks);
            break;
          default:
            break;
        }
      }
    };

    visitBlocks(blocks);
    // The reconstruction recovers at least styled text runs; structural elements
    // (heading level, table cells) are recovered when the layout exposes them.
    expect(sawTypedRun).toBe(true);
    expect(sawHeadingLevel || sawTableCell).toBe(true);
    doc.close();
  });
});
