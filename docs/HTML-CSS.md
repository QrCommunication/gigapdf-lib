# HTML + CSS → PDF — Supported features reference

gigapdf-lib renders **HTML + CSS + JavaScript to PDF with no headless browser**
(no Chromium, no Playwright). The renderer is a real — if pragmatic —
implementation of the CSS visual formatting model written in Rust: an HTML
parser, a cascading stylesheet engine, a box-tree layout (block / inline / table
/ flex / grid), pagination, and a painter that sets text in **embedded Google
fonts** (real glyphs and metrics).

This page is the **exhaustive** list of what the renderer understands, so you
know exactly what you can author. Anything not listed is ignored gracefully
(unknown properties/elements don't break the render).

> **How to call it** — two phases (host does the network I/O):
> ```ts
> const fonts = await fetchFonts(giga.htmlNeededFonts(html)); // phase 1: which fonts
> const pdf = giga.htmlRender(html, fonts, pageW?, pageH?, margin?); // phase 2: render
> ```
> Inline `<script>` runs **before layout** through the built-in JS engine — see
> [the JavaScript section](#javascript) and [USAGE.md](USAGE.md) §7b.

---

## 1. Page setup

Two render entry points, both with sizes in **PDF points** (1pt = 1/72"):

```ts
// Simple: explicit size + one uniform margin (defaults: US Letter, 36pt).
giga.htmlRender(html, fonts, pageW = 612, pageH = 792, margin = 36);

// Full control: named size, per-side margins, running header/footer, numbering.
giga.htmlRenderWith(html, fonts, {
  pageSize: "A4",                 // or pageWidth/pageHeight in points
  margin: { top: 72, bottom: 72, left: 54, right: 54 }, // or a single number
  header: `<div style="text-align:center">My Report</div>`,
  footer: `<div style="text-align:right">Page {{page}} / {{pages}}</div>`,
  startPageNumber: 1,             // number assigned to the first page
});
```

Content flows top-to-bottom and **paginates automatically**; backgrounds and
borders are split correctly across page breaks. Force a break with
[`page-break-*`](#page-breaks) or a `<pagebreak>` tag.

### Named paper sizes (`pageSize`)

Case-insensitive; append `-landscape` (or `-l`) to swap the axes. `giga.pageSize(name)`
resolves one to `{ w, h }` points.

| Family | Names | Portrait size (pt) |
|--------|-------|--------------------|
| ISO A | `a0` `a1` `a2` `a3` `a4` `a5` `a6` | A4 = 595.28 × 841.89, A3 = 841.89 × 1190.55, A5 = 419.53 × 595.28 |
| ISO B | `b4` `b5` | B5 = 498.90 × 708.66 |
| US | `letter` `legal` `tabloid`/`ledger` `executive` | Letter = 612 × 792, Legal = 612 × 1008, Tabloid = 792 × 1224 |

### Margins

`htmlRenderWith` accepts either a single number (uniform) or a per-side object
`{ top, right, bottom, left }` (omitted sides default to 36pt). The content band
is the page minus these margins; running headers/footers are painted **inside**
the top/bottom margins, so leave enough room for them there.

### Running header & footer + page numbering

`header` / `footer` are **full HTML+CSS snippets** (same engine as the body —
fonts, alignment, backgrounds, everything below applies). They repeat on every
page, painted in the top / bottom margin. Two tokens are substituted per page:

| Token | Value |
|-------|-------|
| `{{page}}` | the current page number (from `startPageNumber`) |
| `{{pages}}` | the total page count |

`headerOffset` / `footerOffset` (default `18`) set the distance in points from
the top / bottom edge to the header / footer block. When the header/footer
reference Google fonts, resolve them with `htmlNeededFontsWith(html, header, footer)`
so those faces are downloaded too.

---

## 2. Supported HTML elements

Every element below has a sensible **user-agent default style** (you can override
it with CSS). Unknown elements render as `display: block` and their children are
laid out normally.

### Structure & grouping (block)
`html`, `body`, `div`, `p`, `section`, `article`, `header`, `footer`, `nav`,
`main`, `blockquote`, `figure`, `figcaption`, `form`, `fieldset`, `hr`.

- `p` — 8pt top/bottom margin.
- `blockquote` — 30pt left margin.
- `hr` — a full-width 1pt grey rule.

### Headings (block, bold)
`h1` (24pt) · `h2` (20pt) · `h3` (16pt) · `h4` (13pt) · `h5` (12pt) · `h6` (11pt),
each with bold weight and proportional top/bottom margins.

### Inline text & phrasing
`span`, `a` (blue + underlined), `b` / `strong` (bold), `i` / `em` (italic),
`u` (underline), `small` (10pt), `code` / `kbd` / `samp` (monospace),
`br` (line break). Any other inline element flows as plain inline text.

### Pre-formatted
`pre` — block, **`white-space: pre`** (whitespace and newlines preserved),
monospace.

### Lists
`ul` / `ol` — block, 30pt left padding, 8pt top/bottom margin.
`li` — `display: list-item`; gets a **marker**:

- inside `<ul>` → a bullet `•`,
- inside `<ol>` → an incrementing number `1.`, `2.`, …

### Tables
`table` (`display: table`), `tr` (`table-row`), `td` / `th` (`table-cell`,
2pt padding, 1pt grey border). `th` is bold. `thead` / `tbody` / `tfoot` are
traversed transparently. Cells are laid **side-by-side**, equal width; each row's
height is its tallest cell.

### Images & SVG
`<img src width height>` — an **inline box** sized by the `width` / `height`
attributes (default 64×64). Because the renderer is a **zero-network sandbox**,
use a `data:` URI (or a host-resolved source) for `src`. A PNG/JPEG renders as a
bitmap (**PNG transparency is honoured** via a soft mask); a
`data:image/svg+xml` source renders as **native vector**.

Inline **`<svg>`** is supported too and drawn as native vector paths — never
rasterized. Supported: the shapes `rect` (incl. `rx`/`ry` rounded corners),
`circle`, `ellipse`, `line`, `polyline`, `polygon`; `<path>` (the full `d`
grammar — `M/L/H/V/C/S/Q/T/A/Z`, with **`A` arcs converted exactly** to Béziers);
`<g>` grouping; `transform` (`translate`/`scale`/`rotate`/`matrix`/`skewX`/`skewY`);
`viewBox`; `fill`, `stroke`, `stroke-width`, `opacity`, `fill-opacity`,
`stroke-opacity` (`none` honoured); and **gradients** —
`<linearGradient>` / `<radialGradient>` (with `<stop>` colours, `gradientUnits`
`objectBoundingBox`/`userSpaceOnUse`, `gradientTransform`, and `href`
inheritance) rendered as native PDF axial/radial shadings. Size the box with the
`<svg>` `width`/`height` (or its viewBox). Not drawn: tiling `<pattern>`
(falls back to a solid fill), `filter`, `<text>`, `<use>`, and COLRv1 gradient
glyphs.

### Page breaks
`<pagebreak></pagebreak>` (or `<div class="page-break">`) starts the following
content on a new page — see [page-break CSS](#page-breaks).

### Hidden
`head`, `script`, `style`, `title`, `meta`, `link`, `base`, `noscript` →
`display: none` (parsed, not rendered). `<style>` contents feed the stylesheet;
`<script>` runs before layout.

---

## 3. Supported CSS properties

Author CSS in a `<style>` block or with inline `style="…"`. The cascade is
honoured: **user-agent < `<style>` rules (by specificity, then source order) <
inline `style`**. Inheritance works for the inherited properties below.

### Box model

| Property | Values | Notes |
|----------|--------|-------|
| `margin` | 1–4 lengths | CSS shorthand order (all / v h / t h b / t r b l) |
| `margin-top/-right/-bottom/-left` | length | |
| `padding` | 1–4 lengths | same shorthand as `margin` |
| `padding-top/-right/-bottom/-left` | length | |
| `border` / `border-width` | `1px solid #ccc` | the **width** (length) and **colour** are read; line style is always solid |
| `border-color` | [colour](#colours) | |
| `width` | length or `%` | `%` is relative to the containing block |
| `min-width` / `max-width` | length or `%` | clamp the resolved box width |
| `height` / `min-height` | length | minimum box height — content can still grow it |
| `box-sizing` | `content-box` (default), `border-box` | `border-box` makes `width` include padding + border |

### Display & layout

| Property | Values |
|----------|--------|
| `display` | `block`, `inline`, `inline-block`, `list-item`, `table`, `table-row`, `table-cell`, `flex`, `inline-flex`, `grid`, `inline-grid`, `none` |
| `float` | `left` / `right` (approximated as `inline-block`), `none` |

### Flexbox

| Property | Values | Notes |
|----------|--------|-------|
| `flex-direction` | `row` (default), `column` | |
| `justify-content` | `flex-start`/`start`, `center`, `flex-end`/`end`/`right`, `space-between`, `space-around`/`space-evenly` | main-axis distribution (row only) |
| `flex-grow` | number | per-item growth weight |
| `flex` | `<grow> [shrink] [basis]`, `none`, `auto`, `initial` | only the **grow** factor is read |

Items default to equal columns; `flex-grow` gives proportional widths. Cross-axis
sizing is `stretch`. Not modelled: wrap, `align-items`, `order`, `flex-shrink`.

### Grid

| Property | Values | Notes |
|----------|--------|-------|
| `grid-template-columns` | a track list (`1fr 1fr 200px`) or `repeat(N, …)` | only the **column count** matters; children fill equal-width cells, wrapping every N |

### Typography (inherited)

| Property | Values | Notes |
|----------|--------|-------|
| `color` | [colour](#colours) | text colour |
| `font-size` | length or `%` | `%`/`em` relative to the parent size |
| `font-weight` | `bold`, `bolder`, `600`–`900` → bold; anything else → normal | |
| `font-style` | `italic` / `oblique` → italic; else normal | |
| `font-family` | family list | first family wins; `serif`/`Times`/`Georgia` pick a serif, `monospace`/`Courier`/`mono`/`Consol*` pick a mono — see [fonts](#fonts) |
| `text-align` | `left`, `center`, `right`, `justify` | |
| `text-decoration` / `text-decoration-line` | any of `underline`, `line-through`, `overline` (space-separated) | drawn as thin rules over the run |
| `text-transform` | `uppercase`, `lowercase`, `capitalize`, `none` | cases the rendered text |
| `text-indent` | length or `%` | indents **and** shortens the first line of a block |
| `line-height` | unitless multiplier, length, or `%` | |
| `white-space` | `pre*` preserves whitespace/newlines; else collapses | |

### Backgrounds

| Property | Values | Notes |
|----------|--------|-------|
| `background` / `background-color` | [colour](#colours) | solid fill only (first token of `background` is read; no images/gradients) |

### Visibility & opacity

| Property | Values | Notes |
|----------|--------|-------|
| `visibility` | `visible` (default), `hidden` | `hidden` keeps the box's space but paints nothing |
| `opacity` | `0`–`1` | alpha applied to the element's background, border and text rules (inherited) |

### Page breaks

| Property | Values | Effect |
|----------|--------|--------|
| `page-break-before` / `break-before` | `always`, `page`, `left`, `right`, `recto`, `verso` | start this block on a new page |
| `page-break-after` / `break-after` | same | start the **next** content on a new page |

---

## 4. Length units

Resolved to PDF points. `1px = 0.75pt` (the 96dpi convention), `1pt = 1pt`.

| Unit | Meaning |
|------|---------|
| `px` | × 0.75 → pt |
| `pt` | points (1:1) |
| `em` | × current font-size |
| `rem` | × 12pt (root font-size) |
| `%` | of the font-size (for `font-size`/`line-height`) or the container (for `width`) |
| *(unitless)* | treated as `px` |

---

## 5. Colours

| Form | Example |
|------|---------|
| Hex (3 or 6 digits) | `#0a0`, `#00aa00` |
| `rgb()` | `rgb(0, 170, 0)` |
| Named | `black`, `white`, `red`, `green`, `lime`, `blue`, `gray`/`grey`, `silver`, `lightgray`/`lightgrey`, `navy`, `orange`, `yellow`, `purple`, `teal`, `maroon`, `transparent` |

`transparent` (and any unrecognised colour) leaves the property unset.

---

## 6. Selectors

Two selector engines, for two jobs:

### Stylesheet selectors (in `<style>` rules)
`type` (`p`), `.class`, `#id`, the universal `*`, the **descendant** combinator
(`nav a`), and grouping (`h1, h2, h3`). Multiple compounds combine
(`ul.menu li`). Specificity follows CSS (id > class > type); inline `style` wins.

> Child/sibling/attribute combinators (`>`, `+`, `~`, `[attr]`) are **not** used
> by the stylesheet cascade — author those rules with descendant/class selectors.

### `document.querySelector(All)` selectors (in JavaScript)
The DOM API supports the richer set: `>` (child), `+` (adjacent sibling),
`~` (subsequent sibling), and attribute selectors
`[attr]`, `[attr=v]`, `[attr^=v]`, `[attr$=v]`, `[attr*=v]`, `[attr~=v]`,
`[attr|=v]`, in addition to `tag`/`.class`/`#id`/descendant.

---

## 7. Fonts

Text is set in **real embedded fonts** with correct glyphs and metrics — never a
fallback box. Phase 1 (`htmlNeededFonts`) returns the Google font families your
document references; your host downloads them (see [USAGE.md](USAGE.md) §8) and
passes the bytes to phase 2.

Generic families resolve to a bundled face:

| `font-family` keyword | Resolves to |
|-----------------------|-------------|
| `serif`, `Times*`, `Georgia` | a serif face |
| `monospace`, `Courier*`, `*mono*`, `Consol*` | a monospace face |
| anything else | the named family (downloaded), else the default sans |

### Colour emoji

If the resolved font carries **COLR v0 + CPAL** colour tables (e.g.
`font-family: "Noto Color Emoji"`, which the host downloads like any other
Google font), colour glyphs are drawn as **native vector layers** — each layer a
glyph outline filled with its palette colour — right where the text flows.
Non-colour characters in the same run still render as ordinary text. **Apple
`sbix` bitmap emoji** are also drawn (the glyph's PNG is placed on the baseline).
This works in every output (HTML→PDF and, since COLR layers are vector, the
rasterized `renderPage` too). Not drawn: COLRv1 gradient glyphs and `CBDT/CBLC`
bitmap strikes.

---

## 8. JavaScript

A document's inline `<script>` runs **before layout** through the embedded
**Boa** JavaScript engine, so script-generated DOM is rendered. Boa is a full
ES2021+ engine — classes, closures, destructuring, `RegExp`, `Map`/`Set`,
`Symbol`, `JSON`, lazy/infinite generators and spec-ordered `async`/`await` —
exposed to a JavaScript DOM polyfill:

`document.getElementById` · `getElementsByTagName` · `querySelector(All)` ·
`createElement` · `body` · `title`; and on elements `textContent` · `innerHTML` ·
`getAttribute` · `setAttribute` · `appendChild` · `removeChild` · `classList` ·
`style` · `className` · `id` · `children`.

By design the sandbox has **no network and no real timers**. See the
[README “Honest scope”](../README.md) for the JS language detail.

---

## 9. Not supported (use these instead)

The renderer targets **document/report layout**, not full web-app CSS. The
following are intentionally out of scope and are ignored:

- **Positioning**: `position: absolute/relative/fixed/sticky`, `top/left/z-index`
  → use normal flow, tables, flex, or grid.
- **Flex/grid extras**: wrap, `align-items`/`align-self`, `order`, `flex-shrink`,
  named grid lines/areas, `gap` → use margins/padding and column counts.
- **Visual effects**: `box-shadow`, `border-radius`, `transform`, `filter`,
  gradients, background images → use solid `background`/`border`.
- **Sizing**: `overflow` clipping, `aspect-ratio`, `max-height` → the box grows
  with its content (`width`, `min/max-width`, `height`/`min-height` and
  `box-sizing` **are** supported — see [box model](#box-model)).
- **Typography extras**: `letter-spacing`, `text-shadow`, `@font-face` (fonts
  come from the Google-fonts pipeline), multi-column.
- **Media/at-rules**: `@media`, `@page`, `@import`, CSS variables (`var()`),
  `calc()`.

Unknown properties and elements never error — they're simply skipped — so a
richer stylesheet degrades gracefully to the supported subset.
