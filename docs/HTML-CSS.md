# HTML + CSS → PDF — Supported features reference

gigapdf-lib renders **HTML + CSS + JavaScript to PDF with no headless browser**
(no Chromium, no Playwright). The renderer is a real — if pragmatic —
implementation of the CSS visual formatting model written in Rust: an HTML
parser, a cascading stylesheet engine, a box-tree layout (block / inline / table
/ flex / grid / multi-column / float / positioned), pagination, and a painter
that sets text in **embedded Google fonts** (real glyphs and metrics) and draws
backgrounds, gradients, rounded borders and shadows as native PDF graphics.

This page is the **exhaustive, code-grounded** list of what the renderer
understands, including the precise limitations of partially-supported features,
so you know exactly what you can author. Anything not listed is ignored
gracefully (unknown properties/elements don't break the render).

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
`u` / `ins` (underline), `s` / `strike` / `del` (line-through), `sup` / `sub`
(super/subscript), `mark` (highlight), `small` (10pt), `code` / `kbd` / `samp`
(monospace), `br` (line break). Any other inline element flows as plain inline text.

### Pre-formatted
`pre` — block, **`white-space: pre`** (whitespace and newlines preserved),
monospace.

### Lists
`ul` / `ol` — block, 30pt left padding, 8pt top/bottom margin.
`li` — `display: list-item`; gets a **marker** (overridable with
[`list-style-type`](#list-markers)):

- inside `<ul>` → a bullet `•`,
- inside `<ol>` → an incrementing number `1.`, `2.`, …

### Tables
`table` (`display: table`), `tr` (`table-row`), `td` / `th` (`table-cell`,
2pt padding, 1pt grey border). `th` is bold and centred. `thead` / `tbody` /
`tfoot` are traversed transparently. Cells are laid **side-by-side**, equal
width; each row's height is its tallest cell. `border-collapse: collapse`
(the UA default for `<table>`) draws shared interior rules once.

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

> Note: `<img>` (the element) is the way to place a raster picture. The CSS
> `background-image: url(...)` property is **not** rasterized — see
> [backgrounds](#backgrounds).

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
`var()` (custom properties) and `calc(+ − …)` are resolved in length contexts.

### Box model

| Property | Values | Notes |
|----------|--------|-------|
| `margin` | 1–4 lengths | shorthand order (all / v h / t h b / t r b l); `margin: auto` left/right centres a block of explicit `width` |
| `margin-top/-right/-bottom/-left` | length / `auto` | |
| `padding` | 1–4 lengths | same shorthand as `margin` |
| `padding-top/-right/-bottom/-left` | length | |
| `width` | length or `%` | `%` is relative to the containing block |
| `min-width` / `max-width` | length or `%` | clamp the resolved box width |
| `height` | length | **definite** box height — taller content overflows (and is clipped under `overflow: hidden`); floored by `min-height` |
| `min-height` | length | a floor only — the box still grows with its content |
| `aspect-ratio` | `<w> / <h>` or a number (`16/9`, `1.5`, `auto 16/9`) | when no definite `height` is set, the height is derived as `width / ratio` (taller content then overflows) |
| `box-sizing` | `content-box` (default), `border-box` | `border-box` makes `width` include padding + border |

### Borders

| Property | Values | Notes |
|----------|--------|-------|
| `border` / `border-<side>` / `border-width` / `border-<side>-width` | `1px solid #ccc` | width, colour and style are read **per side** |
| `border-color` / `border-<side>-color` | [colour](#colours) | independent per edge |
| `border-style` / `border-<side>-style` | `solid`, `dashed`, `dotted`, `double`, `inset`, `outset`, `groove`, `ridge` | the 3-D styles shade the top/left vs bottom/right sides darker/lighter to fake depth (`groove`/`ridge` split each side into two half-width tones) |
| `border-radius` (+ `border-<corner>-radius`) | 1–4 lengths, optional `/` for **elliptical** radii (`rx / ry`) | rounds the background fill and **uniform** borders. Caveats: child content is **not clipped** to the curve; a *per-side styled* border stays square |
| `border-collapse` | `collapse`, `separate` | on a `<table>`: shared interior rules drawn once |

### Display & positioning

| Property | Values | Notes |
|----------|--------|-------|
| `display` | `block`, `inline`, `inline-block`, `list-item`, `table`, `table-row`, `table-cell`, `flex`, `inline-flex`, `grid`, `inline-grid`, `none` | |
| `float` | `left`, `right`, `none` | the box leaves flow and **inline** content wraps beside it. Caveats: shrink-to-fit width ≈ ⅓ of the line when none is given; block-level siblings do **not** wrap beside a float |
| `clear` | `left`, `right`, `both`, `none` | drop below earlier floats |
| `position` | `static`, `relative`, `absolute`, `fixed`, `sticky` | `relative` shifts by `inset`; `absolute` is placed by `inset` against the nearest positioned ancestor; `fixed` against the page box; `sticky` is treated as `relative` (no scroll model) |
| `top` / `right` / `bottom` / `left` | length or `%` | offsets for positioned boxes (`%` of the containing block) |
| `z-index` | integer | paint order among positioned boxes |
| `overflow` | `visible`, `hidden`, `clip` | `hidden`/`clip` emit a **real PDF clip** (`q … re W n … Q`): fragments fully outside the padding box are dropped, those straddling an edge are pixel-clipped to it (text, images, backgrounds, gradients — text runs carry their advance width). Nested clipping boxes intersect |
| `opacity` | `0`–`1` | alpha on the element's background, borders and text (inherited) |
| `visibility` | `visible`, `hidden` | `hidden` keeps the box's space but paints nothing |

### Flexbox

| Property | Values | Notes |
|----------|--------|-------|
| `flex-direction` | `row` (default), `column`, `row-reverse`, `column-reverse` | reverse runs the main axis from the far end |
| `flex-wrap` | `nowrap`, `wrap` | wraps onto new lines (row axis) |
| `justify-content` | `flex-start`/`start`, `center`, `flex-end`/`end`, `space-between`, `space-around`, `space-evenly` | main-axis distribution (`space-evenly` = `n + 1` equal gaps) |
| `align-items` / `align-self` | `stretch` (default), `flex-start`, `center`, `flex-end` | cross-axis alignment |
| `order` | integer | reorders items before layout |
| `flex-grow` | number | growth weight (both axes) |
| `flex-shrink` | number | shrink weight; on the column axis it needs a definite container `height` to shrink against |
| `flex-basis` | length / `auto` | initial main size — on the **column** axis it (or a definite `height`) sets the item's height |
| `flex` | `<grow> [shrink] [basis]`, `none`, `auto`, `initial` | shorthand for the three above |
| `gap` / `row-gap` / `column-gap` | length | spacing between items |

### Grid

| Property | Values | Notes |
|----------|--------|-------|
| `grid-template-columns` | track list — `px`, `%`, `fr`, `auto`, `minmax()`, `repeat(N, …)` | fully resolved to real column widths |
| `grid-template-rows` | track list — `pt`, `%`, `fr`, `auto`, `minmax()` | `pt` fixed; `auto` sizes to the tallest cell; `%` and `fr` resolve against the grid's **definite `height`** (`%` = that fraction, `fr` shares the leftover) — with no definite height they fall back to content, the correct auto-height behaviour |
| `grid-column` / `grid-row` (+ `-start` / `-end`) | `N`, `N / M`, `span N` | numeric line placement and spanning |
| `gap` / `row-gap` / `column-gap` / `grid-gap` | length | gutters |

> Named areas (`grid-template-areas`, `grid-area: name`) and named grid lines
> are **not** parsed — use numeric line placement.

### Multi-column

| Property | Values | Notes |
|----------|--------|-------|
| `column-count` | integer | flow content splits into N height-balanced columns |
| `column-gap` | length | gutter between columns (default 1em) |

### Typography (inherited)

| Property | Values | Notes |
|----------|--------|-------|
| `color` | [colour](#colours) | text colour |
| `font-size` | length or `%` | `%`/`em` relative to the parent size |
| `font-weight` | `bold`/`bolder`/`600`–`900` → bold; else normal | the numeric weight is preserved on the run |
| `font-style` | `italic` / `oblique` → italic; else normal | |
| `font-family` | family list | first family wins; generics map to bundled faces — see [fonts](#fonts) |
| `font` | shorthand | size / line-height / family are parsed |
| `text-align` | `left`, `center`, `right`, `justify` | |
| `text-decoration` / `text-decoration-line` | any of `underline`, `line-through`, `overline` (space-separated) | thin rules over the run |
| `text-transform` | `uppercase`, `lowercase`, `capitalize`, `none` | |
| `text-indent` | length or `%` | indents the first line of a block |
| `letter-spacing` | length | extra space between characters |
| `word-spacing` | length | extra space between words |
| `line-height` | unitless multiplier, length, or `%` | |
| `vertical-align` | `super`, `sub`, length (inline); `top`, `middle`, `bottom` (table cells) | |
| `direction` | `ltr` (default), `rtl` | line-level reordering + right alignment; no mixed/bidirectional reflow |
| `white-space` | `pre*` preserves whitespace/newlines; else collapses | |

<a id="list-markers"></a>
### Lists

| Property | Values | Notes |
|----------|--------|-------|
| `list-style-type` / `list-style` | `disc`, `circle`, `square`, `decimal`, `lower-alpha`/`lower-latin`, `upper-alpha`/`upper-latin`, `lower-roman`, `upper-roman`, `none` | the `<li>` marker drawn in the left gutter |

### Backgrounds

| Property | Values | Notes |
|----------|--------|-------|
| `background` / `background-color` | [colour](#colours) | solid fill |
| `background` / `background-image` | `linear-gradient(…)` | **real PDF axial shading** — angle + all colour stops |
| `background` / `background-image` | `radial-gradient(…)` | **real PDF radial shading** — all stops |
| `background` / `background-image` | `conic-gradient(…)` | approximated by 180 flat sectors (slight banding); stops honoured |

> `background-image: url(…)` (a raster image) is **ignored** — place pictures with
> the `<img>` element instead.

### Page breaks

| Property | Values | Effect |
|----------|--------|--------|
| `page-break-before` / `break-before` | `always`, `page`, `left`, `right`, `recto`, `verso` | start this block on a new page |
| `page-break-after` / `break-after` | same | start the **next** content on a new page |
| `page-break-inside` / `break-inside` | `avoid` | try to keep the block on a single page |

### Shadows

| Property | Values | Notes |
|----------|--------|-------|
| `box-shadow` | `<dx> <dy> [blur] [spread] <colour>`, comma-separated for **multiple** layers | offset, colour and spread are exact; **blur is an approximation** (a 6-ring soft edge, not a Gaussian); **`inset` shadows are dropped** |

---

## 4. Length units

Resolved to PDF points. `1px = 0.75pt` (the 96dpi convention), `1pt = 1pt`.

| Unit | Meaning |
|------|---------|
| `px` | × 0.75 → pt |
| `pt` | points (1:1) |
| `in` | inches — × 72pt |
| `cm` / `mm` | × 72/2.54 and × 72/25.4 (anchored at `1in = 72pt`) |
| `pc` | picas — × 12pt |
| `q` | quarter-millimetres — × 72/101.6 |
| `em` | × current font-size |
| `rem` | × 12pt (root font-size) |
| `ex` / `ch` | × 0.5 × current font-size (approximation — no per-font metrics) |
| `vw` / `vh` | % of the content-band width / height |
| `%` | of the font-size (for `font-size`/`line-height`) or the containing block (for box sizes) |
| `calc(…)` | `+ − * /` over the units above |
| *(unitless)* | treated as `px` |

> `ex`/`ch` use the 0.5em approximation rather than true font x-height / `0`-advance.

---

## 5. Colours

| Form | Examples | Notes |
|------|----------|-------|
| Hex | `#0a0`, `#00aa00`, `#0a0f`, `#00aa00ff` | 3/4/6/8 digits; the **alpha** (4-/8-digit) is applied — folded into the fill/text/border opacity |
| `rgb()` / `rgba()` | `rgb(0 170 0)`, `rgba(0, 170, 0, .5)` | comma- or space-separated, `/`-alpha; the alpha is **applied** (the colour's transparency) |
| `hsl()` / `hsla()` | `hsl(120 100% 33%)`, `hsla(120, 100%, 33%, .5)` | converted to RGB; the alpha is **applied** |

> A colour's alpha is multiplied into the opacity of whatever it paints (text,
> background, border, table cell). It composes with the element's
> [`opacity`](#display--positioning).
| Named | the ~139 CSS named colours (`rebeccapurple`, `tomato`, `slategray`, …) + `transparent` | |
| `currentColor` | `border-color: currentColor`, `border: 1px solid currentColor`, `background: currentColor` | resolves to the element's cascaded `color` (case-insensitive) |

`transparent` (and any unrecognised colour) leaves the property unset — the
fill/border is simply not drawn.

---

## 6. Selectors

Two selector engines, for two jobs:

### Stylesheet selectors (in `<style>` rules)
- **Simple**: `type` (`p`), `.class` (combine for AND: `ul.menu`), `#id`,
  the universal `*`, and attribute presence/equality `[attr]`, `[attr=value]`
  / `[attr="value"]`.
- **Combinators**: descendant (`nav a`), child `>` (`ul > li`), adjacent
  sibling `+` (`h2 + p`), general sibling `~` (`h2 ~ p`).
- **Selector lists**: `h1, h2, h3`.
- The cascade follows **specificity** (id > class > type), then source order;
  inline `style` always wins.

> Limitations: the attribute *operators* `~= ^= $= *= |=` are treated as bare
> presence (not enforced). **Pseudo-classes / pseudo-elements** (`:hover`,
> `:first-child`, `::before`, …) are **not** supported — the `:` part is skipped,
> so `li:first-child` matches every `li`. `@media` blocks survive (their rules
> apply) but the media query itself is **not** evaluated.

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
following are out of scope; unknown properties and elements never error — they're
simply skipped — so a richer stylesheet degrades gracefully to the supported
subset.

- **Layout/sizing**: a true scroll model for `position: sticky`.
  `grid-template-areas` / named grid lines (use numeric placement).
- **Visual effects**: `transform`, `filter`, `text-shadow`, `box-shadow: inset`
  and true Gaussian blur, `background-image: url()` raster (use `<img>`),
  CSS tiling patterns. (Gradients, rounded corners and offset/spread shadows
  **are** supported — see [backgrounds](#backgrounds) and [shadows](#shadows).)
- **Typography**: `@font-face` (fonts come from the Google-fonts pipeline),
  full bidirectional/mixed-script reordering (only line-level `direction: rtl`).
- **Selectors**: pseudo-classes / pseudo-elements (`:hover`, `:first-child`,
  `::before`), attribute *operators* (`~= ^= $= *= |=`) in stylesheets.
- **At-rules / values**: `@media` query evaluation, `@page`, `@import`.
  (`var()` custom properties and `calc()` **are** supported in length contexts.)
- **Units**: `cm`, `mm`, `in`, `pc`, `q`, `ex`, `ch` (use `pt`, `px`, `em`,
  `rem`, `%`, `vw`/`vh`).
