#!/usr/bin/env node
// gen-fixtures.mjs — emit conformance fixtures from the gigapdf-lib SDK.
//
// Drives the *real* engine (no third-party PDF/Office library) so the fixtures
// exercise the exact code paths shipped to users. The conformance gate then
// validates each artifact with reference validators (veraPDF / qpdf / structural
// OPC+ODF). One embedded HTML document is rendered to PDF and used as the source
// for every derived format.
//
// Output (default ./fixtures, override with $OUT_DIR or argv[2]):
//   sample.pdf            native HTML+CSS→PDF (engine.htmlToPdf)
//   sample.pdfa-1b.pdf    ISO 19005-1  (doc.toPdfA "pdfa-1b")
//   sample.pdfa-1a.pdf    ISO 19005-1a (Tagged PDF, level A)
//   sample.pdfa-2b.pdf    ISO 19005-2  (default)
//   sample.pdfa-2u.pdf    ISO 19005-2u (Unicode-mapped glyphs)
//   sample.pdfa-2a.pdf    ISO 19005-2a (Tagged PDF, level A)
//   sample.pdfa-3b.pdf    ISO 19005-3  (allows attachments)
//   sample.docx/.xlsx/.pptx   ECMA-376 / ISO 29500 (OPC)
//   sample.odt/.ods/.odp      ISO 26300 (ODF)
//
// Usage: node scripts/conformance/gen-fixtures.mjs [outDir]
import { mkdir, writeFile, rm } from "node:fs/promises";
import { dirname, resolve, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const OUT = resolve(process.argv[2] || process.env.OUT_DIR || join(HERE, "fixtures"));

// Self-contained HTML test page: prose, a heading, a table, a list and an inline
// image (data URI). Embeds a Unicode-mappable Latin font implicitly via the
// engine's built-in base-14 so 2u can map every glyph; its block/heading/table
// structure also drives the StructTreeRoot the level-A (1a/2a) profiles require.
// Kept intentionally simple and printable to keep the PDF/A conformance bar
// reachable.
const SAMPLE_HTML = `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>gigapdf-lib conformance fixture</title>
  <style>
    @page { size: A4; margin: 18mm; }
    body { font-family: Helvetica, Arial, sans-serif; color: #1a1a1a; font-size: 11pt; }
    h1 { font-size: 18pt; margin: 0 0 8pt; }
    h2 { font-size: 13pt; margin: 14pt 0 4pt; }
    p { line-height: 1.4; margin: 0 0 6pt; }
    table { border-collapse: collapse; width: 100%; margin: 8pt 0; }
    th, td { border: 1px solid #888; padding: 4pt 6pt; text-align: left; }
    th { background: #eee; }
    ul { margin: 4pt 0 8pt 16pt; }
  </style>
</head>
<body>
  <h1>Document Conformance Fixture</h1>
  <p>This page is rendered by the native gigapdf-lib HTML+CSS engine and exported
     to PDF, PDF/A (1b, 1a, 2b, 2u, 2a, 3b) and the Office / OpenDocument formats. It is
     validated by veraPDF, qpdf and structural OPC/ODF checks in CI so that every
     conformance level the engine claims is proven on every push.</p>

  <h2>Quarterly figures</h2>
  <table>
    <thead><tr><th>Quarter</th><th>Region</th><th>Revenue</th></tr></thead>
    <tbody>
      <tr><td>Q1</td><td>EMEA</td><td>1,240</td></tr>
      <tr><td>Q2</td><td>AMER</td><td>1,815</td></tr>
      <tr><td>Q3</td><td>APAC</td><td>2,030</td></tr>
    </tbody>
  </table>

  <h2>Notes</h2>
  <ul>
    <li>ISO 32000 — base PDF (qpdf structural check).</li>
    <li>ISO 19005 — PDF/A archival profiles (veraPDF).</li>
    <li>ISO 29500 / ISO 26300 — Office &amp; OpenDocument (OPC / ODF invariants).</li>
  </ul>

  <p>Sphinx of black quartz, judge my vow. 0123456789 &mdash; &eacute;&agrave;&ccedil;&uuml;.</p>
</body>
</html>`;

// The Office/ODF emitters reconstruct an editable document model from the PDF's
// own content (the SDK's PDF→Office path). PDF/A levels are gated by veraPDF;
// Office/ODF by structural validators. Each tuple: [filename, () => Uint8Array].
function deriveFixtures(engine, doc) {
  return [
    ["sample.pdfa-1b.pdf", () => doc.toPdfA("pdfa-1b")],
    ["sample.pdfa-1a.pdf", () => doc.toPdfA("pdfa-1a")],
    ["sample.pdfa-2b.pdf", () => doc.toPdfA("pdfa-2b")],
    ["sample.pdfa-2u.pdf", () => doc.toPdfA("pdfa-2u")],
    ["sample.pdfa-2a.pdf", () => doc.toPdfA("pdfa-2a")],
    ["sample.pdfa-3b.pdf", () => doc.toPdfA("pdfa-3b")],
    ["sample.docx", () => doc.toDocx()],
    ["sample.xlsx", () => doc.toXlsx()],
    ["sample.pptx", () => doc.toPptx()],
    ["sample.odt", () => doc.toOdt()],
    ["sample.ods", () => doc.toOds()],
    ["sample.odp", () => doc.toOdp()],
  ];
}

async function loadSdk() {
  // Prefer the built dist (what release ships); fall back to a sibling checkout.
  const candidates = [
    join(REPO, "sdk", "dist", "index.js"),
    join(REPO, "sdk", "dist", "index.cjs"),
  ];
  for (const c of candidates) {
    try {
      return await import(pathToFileURL(c).href);
    } catch {
      /* try next */
    }
  }
  throw new Error(
    `gigapdf-lib SDK build not found. Run \`bash sdk/scripts/build-wasm.sh && (cd sdk && pnpm build)\` first. Looked in:\n  ${candidates.join("\n  ")}`,
  );
}

async function main() {
  const { GigaPdfEngine } = await loadSdk();
  if (typeof GigaPdfEngine?.loadDefault !== "function") {
    throw new Error("SDK does not export GigaPdfEngine.loadDefault — incompatible build");
  }

  await rm(OUT, { recursive: true, force: true });
  await mkdir(OUT, { recursive: true });

  const engine = await GigaPdfEngine.loadDefault();

  // 1. Base PDF from the native HTML engine.
  const pdfBytes = engine.htmlToPdf(SAMPLE_HTML);
  if (!(pdfBytes instanceof Uint8Array) || pdfBytes.length < 200) {
    throw new Error(`htmlToPdf produced an implausible PDF (${pdfBytes?.length} bytes)`);
  }
  await writeFile(join(OUT, "sample.pdf"), pdfBytes);
  console.log(`✓ sample.pdf (${pdfBytes.length} bytes)`);

  // 2. Everything derived from that PDF.
  const doc = engine.open(pdfBytes);
  const written = ["sample.pdf"];
  for (const [name, make] of deriveFixtures(engine, doc)) {
    let bytes;
    try {
      bytes = make();
    } catch (e) {
      throw new Error(`generating ${name} threw: ${e?.message || e}`);
    }
    if (!(bytes instanceof Uint8Array) || bytes.length < 64) {
      throw new Error(`${name} is implausibly small (${bytes?.length} bytes)`);
    }
    await writeFile(join(OUT, name), bytes);
    written.push(name);
    console.log(`✓ ${name} (${bytes.length} bytes)`);
  }

  console.log(`\nWrote ${written.length} fixture(s) to ${OUT}`);
}

main().catch((e) => {
  console.error(`✗ gen-fixtures failed: ${e?.stack || e}`);
  process.exit(1);
});
