# Changelog

All notable changes to `@qrcommunication/gigapdf-lib` are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [0.9.0] - 2026-06-16

### Added

- **`engine.encryptionInfo(pdf)`** — inspect a PDF's encryption **without
  decrypting it** (no password needed): returns `{ encrypted, permissions,
  version, revision }`, read straight from the `/Encrypt` dictionary's `/P` /
  `/V` / `/R`. Works on password-protected files (where `open()` fails). ABI
  `gp_encryption_info`.

## [0.8.0] - 2026-06-16

### Added

- **AES PDF encryption** (`doc.saveEncrypted`). The Standard Security Handler can
  now *write* **AES-128 (V4/R4)** and **AES-256 (V5/R6)** in addition to RC4, with
  **separate user and owner passwords**:
  - `saveEncrypted(password, fileId, { algorithm: "aes256" | "aes128" | "rc4",
    ownerPassword, permissions, keySeed })` — defaults to **AES-256**.
  - AES-256 needs a **secret 32-byte file key** (the engine has no RNG): the SDK
    generates it with Web Crypto, or you pass `keySeed`. The decryption side
    already read AESV2/AESV3; `openEncrypted` now also accepts the **owner**
    password for R6 (Algorithm 2.A).
  - ABI `gp_save_encrypted` gains `owner`, `key` and an `algorithm` selector.

### Changed

- **Breaking (SDK):** `saveEncrypted(password, fileId, permissions?)` →
  `saveEncrypted(password, fileId, opts?)`. Pass `{ permissions }` (and
  `{ algorithm: "rc4" }` to keep the previous RC4 behaviour).

## [0.7.0] - 2026-06-15

### Added

- **Complete viewer zoom controls** (`@qrcommunication/gigapdf-lib/viewer`):
  `fitWidth()`, `fitPage()`, `actualSize()`, `setZoom()` / `setZoomPercent()` and a
  `zoom` getter; a toolbar **preset drop-down** (Fit width · Fit page · 50–400 %)
  with a live `%` readout; `Ctrl`/`⌘` + mouse-wheel zoom; and a `0` keyboard
  shortcut. A chosen **fit mode is sticky** — it re-applies when the viewport
  resizes (via `ResizeObserver`) and when paging to a page of a different
  orientation.
- **Editor rulers & margins** (`@qrcommunication/gigapdf-lib/editor`): every page
  shows graduated **millimetre rulers** (top + left) and four **margin guides**
  dragged **live** from handles in the ruler bands — or set via the palette's
  `T R B L` mm fields and the `setMargins()` / `getMargins()` / `showRulers()`
  API. Guides are drawn in page-point coordinates (on a second SVG layer) and kept
  a constant on-screen size at any zoom.

## [0.6.0] - 2026-06-15

### Added

- **Full page control for HTML→PDF** via `htmlRenderWith(html, fonts, options)`:
  - **named paper sizes** — `pageSize: "A4" | "a3-landscape" | "letter" | …`
    (ISO A0–A6, ISO B4/B5, US Letter/Legal/Tabloid/Executive; `-landscape`
    suffix swaps the axes). `giga.pageSize(name)` resolves one to `{ w, h }`
    points.
  - **per-side margins** — `margin: number | { top, right, bottom, left }`.
  - **running header & footer** — `header` / `footer` are full HTML+CSS
    snippets painted in the page margins on every page, with `{{page}}` /
    `{{pages}}` substitution and configurable `startPageNumber`,
    `headerOffset` / `footerOffset`.
- **`htmlNeededFontsWith(html, header, footer)`** — phase-1 font discovery that
  also scans the header/footer HTML so their Google fonts are fetched.
- New ABI exports: `gp_html_render_opts`, `gp_html_needed_fonts_ex`,
  `gp_page_size`.

### Images & SVG

- **SVG → native PDF vector** via `doc.addSvg(page, src, x, y, w, h)` (ABI
  `gp_add_svg`): shapes (`rect`/`circle`/`ellipse`/`line`/`polyline`/`polygon`),
  `<path>` (full `d` grammar with **exact `A` arc→Bézier conversion**), `<g>`
  groups, `transform`, `viewBox`, `fill`/`stroke`/`stroke-width`/`opacity`, and
  **gradients** (`<linearGradient>`/`<radialGradient>` → native PDF axial/radial
  shadings, with stops, `gradientUnits`, `gradientTransform` and `href`
  inheritance) — crisp at any zoom, not rasterized. In the HTML renderer, inline
  `<svg>` and `data:image/svg+xml` `<img>` sources render as native vector.
- **PNG transparency in the rasterizer**: `renderPage`/thumbnails now honour an
  image's `/SMask` (soft mask) as per-pixel alpha instead of treating it as
  opaque, so transparent PNGs composite correctly in every conversion (not just
  HTML→PDF).
- **Colour emoji** (COLR v0 + CPAL): when a text run uses a colour font (e.g.
  `font-family: "Noto Color Emoji"`), emoji are drawn as native vector colour
  layers in the HTML renderer — crisp, and rasterized for free. **Apple `sbix`
  bitmap emoji** are placed as PNG glyph bitmaps. Non-colour characters in the
  run still render as text. (COLRv1 gradient glyphs and `CBDT/CBLC` strikes are
  not yet drawn.)

### Viewer

- **`@qrcommunication/gigapdf-lib/viewer`** — a new zero-dependency browser
  document viewer (`GigaPdfViewer`) built on the engine (no pdf.js): opens PDF,
  Office (docx/xlsx/pptx, legacy, ODF) and HTML (auto-detected, converted
  in-engine), renders pages with `renderPage`, **detects per-page orientation**
  and adapts, with navigation, zoom, a thumbnail rail, keyboard control and a
  **fullscreen presentation mode**.
- **`@qrcommunication/gigapdf-lib/editor`** — an interactive **editing canvas**
  (`GigaPdfEditor`) extending the viewer: an SVG overlay per page with tools
  (text, rectangle, ellipse, line, freehand ink, image, highlight, redaction),
  select·move·delete, and `applyEdits()` that **bakes edits into the real PDF**
  through the engine (then re-renders); `save()` returns the result.

### CSS

- HTML→PDF renderer gained `min-width` / `max-width`, `height` / `min-height`,
  `box-sizing`, `text-indent` (first-line indent), `visibility: hidden`,
  `opacity` (backgrounds/borders/text rules), and `text-decoration: line-through`
  / `overline`. See [`docs/HTML-CSS.md`](../docs/HTML-CSS.md).

## [0.5.0] - 2026-06-15

### Changed

- **Suspendable JavaScript VM** for `<script>` execution (`htmlRender` /
  `runInlineScripts`). `function*` and `async` bodies now compile to a
  resumable bytecode machine, so:
  - **generators are truly lazy** — an infinite `while (true) { yield … }` is
    fine, `.next(v)` feeds a value back into the suspended `yield`, and `yield*`
    delegates lazily;
  - **`await` yields to the event loop** with spec microtask ordering (the
    synchronous tail runs before a deferred continuation), instead of draining
    the queue synchronously;
  - **full control flow** can span a `yield`/`await` — `try`/`catch`/`finally`
    (the handler survives suspension; a rejected `await` in a `try` is caught),
    `for…of`/`for…in`, `switch`, labelled `break`/`continue`, destructuring,
    compound assignment, and `...spread`.

  No API change — existing `htmlRender`/`runInlineScripts` calls simply behave
  correctly for script-driven, generator/async-heavy templates. A body the VM
  can't compile (e.g. `try`/`catch` around a `yield`/`await`) transparently
  falls back to the previous eager/synchronous model.

## [0.4.0] - 2026-06-15

### Added

- **AcroForm field creation.** Build interactive forms from scratch — no
  `pdf-lib`. New `GigaPdfDoc` methods, each taking a `[x0,y0,x1,y1]` rectangle
  and an optional [`FieldStyle`](src/index.ts) (font size, text/border/background
  colour, border width):
  - `addTextField(page, name, rect, value?, { maxLen, multiline, password, style })`
  - `addCheckbox(page, name, rect, checked?, { export, style })`
  - `addRadioGroup(page, name, options: RadioOption[], { selected, style })`
  - `addComboBox(page, name, rect, options, { selected, editable, style })`
  - `addListBox(page, name, rect, options, { selected, multi, style })`

  Every widget is given a real `/AP` appearance stream (text baseline, a vector
  tick for checkboxes, a filled dot for radios) **and** the form is flagged
  `NeedAppearances`, so fields display immediately and stay faithful when edited.
- **Advanced flexbox + real grid** in the HTML renderer:
  - `flex-direction: column`, `justify-content` (start/center/end/space-between/
    space-around) and per-item `flex-grow` (proportional column widths);
  - `display: grid` with `grid-template-columns` (fixed column count; children
    wrap into rows). `float` still maps to inline-block.
- **ES module syntax** is now parsed transparently by the JS engine (`import` is
  elided, `export` declarations run as normal statements).

## [0.3.0] - 2026-06-15

### Added

- **JavaScript engine** (zero-dependency, pure Rust → WASM). A document's inline
  `<script>`s now execute **before layout** inside `htmlRender` /
  `htmlNeededFonts` — no Chromium/Playwright — so script-driven content renders.
  The engine covers:
  - Language: classes + `super`, closures, destructuring, spread, optional
    chaining, template literals, `for…of`, generators (`function*`/`yield`,
    eager), `async`/`await` + `Promise` (deterministic synchronous microtask
    model), `try/catch/finally`, labelled loops, `arguments`.
  - Built-ins: `Object`/`Array`/`String`/`Number`/`Boolean`/`Math`/`JSON`/
    `console`/`Map`/`Set`/`RegExp` (a from-scratch backtracking regex engine)/
    `Error`, plus `parseInt`/`parseFloat`/`setTimeout`/`queueMicrotask`.
  - DOM bindings: `document.getElementById`/`getElementsByTagName`/
    `querySelector(All)` (combinators `>`/`+`/`~`, attribute selectors), and on
    elements `textContent`/`innerHTML`/`getAttribute`/`setAttribute`/
    `appendChild`/`removeChild`/`classList`/`style`/`children`.
- **Page breaks** in the HTML renderer: CSS `page-break-before|after: always`,
  `break-before|after: page`, a `<pagebreak>` element, or `class="page-break"`
  start the following content on a new page.
- **CSS flexbox** (`display: flex` / `inline-flex`) — a basic equal-column row;
  `grid` falls back to block flow and `float` to inline-block.

### Notes

- `htmlRender` / `htmlNeededFonts` are unchanged in signature — they simply run
  the document's scripts first. No new SDK call is required.

## [0.2.0] - 2026-06-15

### Added

- Vector drawing primitives on `GigaPdfDoc`: `drawLine`, `addEllipse`,
  `addPolygon`, and `addPath` — the latter accepts arbitrary SVG path data
  (`M`/`L`/`H`/`V`/`C`/`S`/`Q`/`T`/`A`/`Z`, absolute & relative), converting
  quadratic Béziers to cubics and flipping the Y axis like `pdf-lib`'s
  `drawSvgPath`. Covers freeform/polygon/triangle shapes.
- `addImage`: embed PNG or JPEG rasters as image XObjects. JPEG is stored
  losslessly via `/DCTDecode`; PNG is decoded in-engine (zero-dependency) with
  its alpha channel honoured through a `/SMask` soft mask.
- `opacity` (fill + stroke alpha through a transient `/ExtGState`) on every
  shape and image (`addRectangle`, `drawLine`, `addEllipse`, `addPolygon`,
  `addPath`, `addImage`).
- `toOdp`: convert a PDF to an editable OpenDocument Presentation (`.odp`) —
  one slide per page with positioned text boxes, pictures and shapes. This
  completes **bidirectional ODF** (`.odt` / `.ods` / `.odp` both ways, the
  reverse via `officeToPdf`), round-trip validated through LibreOffice Impress.
- **HTML → PDF rendering engine** (`htmlNeededFonts` + `htmlRender`): a
  zero-dependency in-engine pipeline — HTML parser → CSS cascade (selectors,
  specificity, inheritance, UA defaults) → block / inline / table layout with
  pagination → paint — that renders HTML + CSS to PDF **without a headless
  browser**. Text is set in **embedded Google fonts** resolved against the full
  catalogue (real glyphs + metrics → identical or nearest match). Validated
  end-to-end: Roboto downloaded, embedded (`emb=yes`, Identity-H) and the output
  opens in LibreOffice. JavaScript execution is not included (a separate engine).

### Changed

- `addRectangle` gains a trailing optional `opacity` argument — backward
  compatible (defaults to `1`).

[0.2.0]: https://github.com/qrcommunication/gigapdf-lib/releases/tag/v0.2.0

## [0.1.0] - 2026-06-14

### Added

- Initial public release of the TypeScript SDK for **gigapdf-lib**, a
  zero-dependency Rust→WASM PDF engine.
- `GigaPdfEngine`: `load()`, `loadDefault()` (Node), `open()`, `openEncrypted()`,
  stateless conversions (`txtToPdf`, `htmlToPdf`, `rtfToPdf`, `officeToPdf`), and
  font helpers (`fontCatalog`, `fontRequestUrl`, `parseCssFontUrl`).
- `GigaPdfDoc`: full document API — text intelligence (`textRuns`,
  `structuredText`, `search`, `ocr`, `ocrText`, `elements`, `elementAt`),
  editing (`replaceText`, `removeElement`, `moveElement`, `duplicateElement`,
  `addRectangle`, `redact`), pages (`rotatePage`, `deletePage`, `movePage`,
  `appendPages`, `extractPages`), rendering (`renderPage`), embedded fonts
  (`embedFont`, `addText`, `neededFonts`), conversions to
  text/HTML/DOCX/PPTX/ODT/XLSX/ODS/RTF/PDF-A, security (`saveEncrypted`, `sign`),
  metadata (`getMetadata`, `setMetadata`), annotations (square, highlight, line,
  free-text, underline, strike-out, ink, stamp, plus `annotations`,
  `removeAnnotation`, `flattenAnnotations`), hyperlinks (`links`, `addUriLink`,
  `addGotoLink`), outline (`outline`, `setOutline`), and AcroForm fields
  (`fields`, `setTextField`, `setCheckbox`, `setRadio`, `setChoice`).
- The engine `.wasm` is self-contained — no third-party runtime dependencies.

[0.1.0]: https://github.com/qrcommunication/gigapdf-lib/releases/tag/v0.1.0
