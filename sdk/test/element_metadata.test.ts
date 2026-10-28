import { readFileSync } from "node:fs";
import { describe, it, expect, beforeAll } from "vitest";
import { GigaPdfEngine } from "../src/index";
import type { ImageElementInfo, TextElementInfo, VectorPathInfo } from "../src/index";

// One page exercising every element-metadata field of the JSON bridge, inside
// `q /GS1 gs … Q` (GS1 = /ca 0.5 /CA 0.5 /BM /Multiply):
// - text `2 Tr` (fill+stroke), red stroke, `Tm [1.5 0 0.3 1]`, TJ
//   `[(He) -10 (llo) -3000 (World)]` (a sub-split kern, then a position jump);
// - a 2×1 image drawn by `100 0 0 50 40 480 cm` under a `50 500 100 100 re W n` clip;
// then, after `Q`: an `f*` square and a `1 J 2 j 4 M` stroked line.
const FIXTURE = new Uint8Array(
  readFileSync(new URL("../../fixtures/element-metadata.pdf", import.meta.url))
);

const close = (actual: number, expected: number) => expect(actual).toBeCloseTo(expected, 3);

describe("element metadata (text / images / vector paths)", () => {
  let text: TextElementInfo[];
  let images: ImageElementInfo[];
  let paths: VectorPathInfo[];

  beforeAll(async () => {
    const giga = await GigaPdfEngine.loadDefault();
    const doc = giga.open(FIXTURE);
    text = doc.textElements(1);
    images = doc.imageElements(1);
    paths = doc.vectorPaths(1);
    doc.close();
  });

  it("text runs carry paint state and a Fabric-compatible transform", () => {
    expect(text).toHaveLength(1);
    const run = text[0]!;
    expect(run.text).toBe("HelloWorld");
    expect(run.fontFamily).toBe("Helvetica");
    expect(run.fontResolved).toBe(true);
    close(run.fillAlpha, 0.5);
    expect(run.blendMode).toBe("Multiply");
    expect(run.strokeColor).toEqual([1, 0, 0]);
    expect(run.renderingMode).toBe(2);
    // [1.5 0 0.3 1] = R(0) · Scale(1.5, 1) · SkewX(atan(0.3 / 1.5)).
    expect(run.rotation).toBe(0);
    close(run.scaleX, 1.5);
    close(run.scaleY, 1);
    close(run.skewX, (Math.atan(0.2) * 180) / Math.PI);
    expect(run.skewY).toBe(0);
  });

  it("text segments carry spacing and intra-segment kerning", () => {
    const segs = text[0]!.segments;
    expect(segs.map((s) => s.text)).toEqual(["Hello", "World"]);
    expect(segs[0]!.totalKerning).toBe(-10);
    expect(segs[1]!.totalKerning).toBe(0);
    for (const s of segs) {
      expect(s.wordSpacing).toBe(0);
      expect(s.charSpacing).toBe(0);
    }
  });

  it("images carry blend mode, transform and the active clip box", () => {
    expect(images).toHaveLength(1);
    const img = images[0]!;
    close(img.opacity, 0.5);
    expect(img.blendMode).toBe("Multiply");
    close(img.scaleX, 100);
    close(img.scaleY, 50);
    expect(img.skewX).toBe(0);
    expect(img.skewY).toBe(0);
    expect(img.clipBounds).toEqual([50, 500, 150, 600]);
    // The placement box itself is not clipped.
    expect([img.x, img.y, img.width, img.height]).toEqual([40, 480, 100, 50]);
  });

  it("vector paths carry fill rule, line style and blend mode", () => {
    expect(paths).toHaveLength(2);
    const [square, line] = paths as [VectorPathInfo, VectorPathInfo];
    expect(square.fillRule).toBe("evenodd");
    expect(square.blendMode).toBeNull();
    expect(square.lineCap).toBe("butt");
    expect(square.lineJoin).toBe("miter");
    expect(square.miterLimit).toBe(10);
    expect(line.fillRule).toBe("nonzero");
    expect(line.lineCap).toBe("round");
    expect(line.lineJoin).toBe("bevel");
    expect(line.miterLimit).toBe(4);
  });
});
