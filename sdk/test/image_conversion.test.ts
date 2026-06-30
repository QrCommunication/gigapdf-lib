import { readFileSync } from "node:fs";
import { describe, it, expect, beforeAll } from "vitest";
import { GigaPdfEngine } from "../src/index";

const fx = (p: string) =>
  new Uint8Array(readFileSync(new URL(`../../crates/core/src/raster/fixtures/${p}`, import.meta.url)));

const isPdf = (b: Uint8Array) =>
  b.length > 100 && new TextDecoder().decode(b.slice(0, 5)) === "%PDF-";

// A 2×1 uncompressed little-endian RGB TIFF (red, green), mirroring the Rust
// make_rgb_tiff_le fixture — exercises the TIFF → PDF transcode path end-to-end
// through the production WASM binary.
function tinyRgbTiff(): Uint8Array {
  const strip = [255, 0, 0, 0, 255, 0];
  const entries: [number, number, number, number][] = [
    [256, 3, 1, 2],
    [257, 3, 1, 1],
    [258, 3, 1, 8],
    [259, 3, 1, 1],
    [262, 3, 1, 2],
    [273, 4, 1, 0], // StripOffsets — patched
    [277, 3, 1, 3],
    [278, 3, 1, 1],
    [279, 4, 1, strip.length],
  ];
  const stripOff = 8 + 2 + entries.length * 12 + 4;
  const out: number[] = [0x49, 0x49, 0x2a, 0x00, 8, 0, 0, 0];
  const u16 = (v: number) => [v & 0xff, (v >> 8) & 0xff];
  const u32 = (v: number) => [v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff, (v >> 24) & 0xff];
  out.push(...u16(entries.length));
  for (const [tag, ty, cnt, val] of entries) {
    out.push(...u16(tag), ...u16(ty), ...u32(cnt), ...u32(tag === 273 ? stripOff : val));
  }
  out.push(...u32(0), ...strip);
  return new Uint8Array(out);
}

describe("image → PDF conversion (production WASM)", () => {
  let giga: GigaPdfEngine;
  beforeAll(async () => {
    giga = await GigaPdfEngine.loadDefault();
  });

  it("PNG → PDF", () => {
    const rgba = new Uint8Array([255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255]);
    expect(isPdf(giga.imageToPdf(giga.rgbaToPng(rgba, 2, 2)))).toBe(true);
  });

  it("WebP → PDF", () => {
    expect(isPdf(giga.imageToPdf(fx("vp8test.webp")))).toBe(true);
  });

  it("AVIF → PDF", () => {
    expect(isPdf(giga.imageToPdf(fx("av1test.avif")))).toBe(true);
  });

  it("TIFF → PDF", () => {
    expect(isPdf(giga.imageToPdf(tinyRgbTiff()))).toBe(true);
  });

  it("rejects non-image bytes", () => {
    const out = giga.imageToPdf(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]));
    expect(out.length).toBe(0);
  });

  it("addImage accepts a transcoded PNG (round-trips into a page image)", () => {
    const rgba = new Uint8Array([255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255]);
    const doc = giga.open(giga.txtToPdf("with image"));
    expect(doc.addImage(1, giga.rgbaToPng(rgba, 2, 2), 40, 500, 60, 60, 1)).toBe(true);
    expect(doc.imageElements(1).length).toBeGreaterThan(0);
    doc.close();
  });
});
