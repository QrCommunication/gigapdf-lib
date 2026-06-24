# Changelog

All notable changes to **gigapdf-lib** (the Rust engine + the
`@qrcommunication/gigapdf-lib` TypeScript SDK) are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/) and the project adheres
to [Semantic Versioning](https://semver.org/).

The per-release SDK detail also lives in [`sdk/CHANGELOG.md`](sdk/CHANGELOG.md).

## [0.87.0] - 2026-06-24

HTML/CSS renderer — colour alpha ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item C).

### Added

- **Colour alpha is now applied** instead of parsed-then-dropped. `rgba()`,
  `hsla()`, `#rgba` and `#rrggbbaa` carry their alpha through to the paint: it is
  folded into the opacity of whatever the colour paints — text, background,
  border, rounded box and table cell — and composes with the element `opacity`.
  New public `parse_color_alpha()` returns `(rgb, alpha)`; `parse_color()` stays a
  thin wrapper that drops the alpha.

### Fixed

- A function colour with **internal spaces** (`rgba(0, 0, 0, .5)`,
  `hsl(0 100% 50%)`) is now parsed in the `background` and `border` shorthands
  (the whitespace tokeniser previously split it mid-function and dropped it).

## [0.86.1] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item A).

### Added

- **`grid-template-rows` with `%` and `fr`** (previously only fixed `pt` rows were
  honoured). `%` rows resolve against the grid's definite `height`; `fr` rows share
  the leftover space after the fixed / `%` / `auto` rows and gaps, growing the rows
  (and shifting their already-placed content) to fill the container. With no
  definite grid height `%`/`fr` fall back to content sizing — the correct
  auto-height behaviour. `auto` and `minmax()` unchanged.

## [0.86.0] - 2026-06-24

HTML/CSS renderer — real `overflow` clipping ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item A).

### Added

- **`overflow: hidden` / `clip` now emit a real PDF clip** (`q … re W n … Q`)
  instead of whole-fragment culling only. Content straddling a box edge — text,
  images, backgrounds, gradients — is pixel-clipped to the padding box; nested
  clipping boxes intersect. New `Fragment::Clipped` carries the clip through
  pagination and band offsetting; new public `Document::push_clip_rect` /
  `Document::restore_graphics` emit the clip ops.
- **Definite `height`.** `height` is now a definite box height (taller content
  overflows, and is clipped under `overflow: hidden`) rather than an alias of
  `min-height`. `min-height` remains a pure floor; both compose.
- **Text runs carry their advance width** (`Fragment::Text.w`), so horizontally
  overflowing text registers as straddling and is clipped (previously a run was a
  zero-width point and never clipped).

## [0.85.4] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item A).

### Added

- **`flex-direction: row-reverse` / `column-reverse`** now run the main axis from
  the far end (the items are reversed after the `order` sort) instead of
  collapsing to the forward axis. New `Style::flex_reverse`, parsed from both the
  `flex-direction` longhand and the `flex-flow` shorthand.

## [0.85.3] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item A).

### Fixed

- **`justify-content: space-evenly`** now distributes `n + 1` equal gaps (one
  before each item and one after the last) instead of being aliased to
  `space-around` (which puts half-size gaps at the ends). New `Justify::SpaceEvenly`.

## [0.85.2] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item C).

### Added

- **`currentColor`** now resolves to the element's cascaded `color` in the
  HTML→PDF renderer (case-insensitive): `border-color: currentColor`,
  `background: currentColor`, and as a sub-token of the `border` shorthand
  (`1px solid currentColor`). Previously it was unrecognised → the property was
  left unset.

## [0.85.1] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item E).

### Added

- **Absolute & relative CSS length units** in the HTML→PDF renderer: `cm`, `mm`,
  `in`, `pc`, `q` (anchored at `1in = 72pt`) and `ex`/`ch` (0.5em approximation).
  Resolved by `parse_len_px`; a single `LENGTH_UNITS` table keeps unit detection
  (flex-basis / font-size) in lock-step with resolution.

## [0.85.0] - 2026-06-24

Accessibility: **standalone tagged-PDF / PDF-UA authoring**. Resolves
[#18](https://github.com/qrcommunication/gigapdf-lib/issues/18).

### Added

- **`Document::to_tagged(pdf_ua)`** — author a tagged (accessible) PDF: a
  `/StructTreeRoot` logical-structure tree (`P`/`H1`–`H6`/`Table`/`TR`/`TH`/`TD`/
  `L`/`LI`/`Figure`) with marked content (`/MCID`), `/MarkInfo /Marked true`,
  `/Lang`, an (empty) `/RoleMap`, and `/Alt` on every `Figure` — **without**
  forcing PDF/A (no OutputIntent / ICC / `pdfaid`). `pdf_ua` stamps the PDF/UA-1
  identifier (ISO 14289) in XMP. ISO 32000-1 §14.7/§14.8.
- WASM `gp_to_tagged`; SDK `doc.toTagged({ pdfUa? })`.

### Notes

- Reuses the structure builder added for PDF/A level A (`convert/tagged.rs`); the
  standalone path post-processes the tree (figure `/Alt`, `/RoleMap`) without
  altering the PDF/A output. Figures get a non-empty `/Alt` placeholder so the
  file is structurally PDF/UA-valid — meaningful alternate text still requires
  author input (a per-figure alt-text API is future work). For archival +
  accessibility together, `to_pdfa_level(Pdfa2a)` emits the same tree as PDF/A-2a.

## [0.84.0] - 2026-06-24

Security: **public-key (certificate) encryption** + **password management**.
Resolves [#17](https://github.com/qrcommunication/gigapdf-lib/issues/17).

### Added

- **`Document::encrypt_for_recipients(&[cert_der], perms, aes256, encrypt_metadata,
  seed, rng)`** — public-key (certificate) security (`/Filter /Adobe.PubSec`,
  `/SubFilter /adbe.pkcs7.s5`, ISO 32000-1 §7.6.5): a random seed is wrapped per
  X.509 recipient in a CMS `EnvelopedData` (RSA key transport), the file key is
  `Hash(seed || recipients)`, and objects are AESV2/AESV3-encrypted. Only a
  recipient private key can open the file — no shared password.
- **`Document::open_with_private_key(bytes, cert_der, key_der)`** — the read
  counterpart: recovers the seed from the recipient list and decrypts.
- **`Document::change_passwords(…)`** and **`remove_encryption()`** — re-key or
  decrypt an already-opened document.
- **`Document::save_encrypted_ex(…, encrypt_metadata)`** — exposes
  `/EncryptMetadata` for RC4/AESV2/AESV3 (folded into the file-key derivation).
- `RsaPrivateKey::to_pkcs1_der`. WASM `gp_encrypt_for_recipients` /
  `gp_open_with_private_key` / `gp_change_passwords` / `gp_remove_encryption`; SDK
  `encryptForRecipients` / `openWithPrivateKey` / `changePasswords` /
  `removeEncryption`.

### Notes

- Built on the same RustCrypto `cms`/`x509-cert`/`rsa` primitives as the signing
  stack; no new dependency. The engine has no RNG, so public-key encryption takes
  host randomness (`seed` ≥ 20 B, `rng_seed` ≥ 32 B); the SDK fills these from Web
  Crypto by default.
- Verified by a full in-engine round-trip (encrypt to a self-signed recipient →
  open with its private key; a stranger's key is rejected) for both AES-128 and
  AES-256.

## [0.83.0] - 2026-06-24

Press-ready **colour authoring** — fills and text are no longer limited to
DeviceRGB. Resolves
[#11](https://github.com/qrcommunication/gigapdf-lib/issues/11) (CMYK / spot
(`Separation`) / ICC `OutputIntent` / overprint).

### Added

- **`Color`** enum — `Rgb` · `Cmyk` · `Gray` · `Separation { name, tint, cmyk }`
  (a spot ink with its `DeviceCMYK` tint transform) · `IccBased { components,
  profile }` (ISO 32000-1 §8.6).
- **`Document::add_filled_rectangle(page, [x,y,w,h], &Color, opacity)`** and
  **`add_filled_polygon(page, &points, &Color, opacity)`** — fill shapes in any
  colour space (a `Separation`/`ICCBased` colour registers its colour-space
  resource on the page).
- **`add_text_color(page, x, y, size, text, font, &Color, …)`** — base-14 text in
  any colour space (the text-drawing core was refactored to take colour-setting
  operators, so RGB text is unchanged).
- **`set_overprint(page, fill, stroke, mode)`** — an `/ExtGState` with `/op`,
  `/OP`, `/OPM` for prepress trapping (ISO 32000-1 §8.6.7).
- **`add_output_intent(&profile, condition)`** — a document `OutputIntent`
  (`/S /GTS_PDFX`) embedding an ICC profile, decoupled from the PDF/A path; `/N`
  is read from the profile's data-colour-space signature.
- `gp_add_filled_rectangle` / `gp_add_filled_polygon` / `gp_add_text_color` /
  `gp_set_overprint` / `gp_add_output_intent`; SDK `addFilledRectangle` /
  `addFilledPolygon` / `addTextColor` / `setOverprint` / `addOutputIntent` + a
  `Color` union type.

### Notes

- CMYK/Separation/ICC fills + the OutputIntent pass `qpdf --check` (clean).
- Existing `add_rectangle`/`add_ellipse`/`add_polygon`/`add_text*` keep their RGB
  signatures (no breaking change); the new `*_color`/`*_filled_*` methods are the
  any-colour-space path.

## [0.82.0] - 2026-06-24

Gradient **authoring** — the rasterizer could already render shadings, but there
was no API to *produce* them. Resolves
[#12](https://github.com/qrcommunication/gigapdf-lib/issues/12) (gradients; tiling
patterns / blend-mode authoring deferred — see notes).

### Added

- **`Document::add_gradient(page, &GradientSpec)`** — paints a **linear** (axial,
  shading type 2) or **radial** (type 3) gradient over a rectangle, as a
  `PatternType 2` shading pattern (ISO 32000-1 §8.7.4 / §8.7.3). The colour stops
  compile to a PDF interpolation function (a type-2 exponential for two stops, a
  type-3 stitching function for more). New `GradientSpec` / `GradientKind` /
  `GradientStop` types.
- `gp_add_gradient` (kind + flat coords + parallel stop offset/colour arrays) /
  `doc.addGradient(page, { kind, coords, stops, rect, extend?, opacity? })`.

### Notes

- The produced shading patterns + PDF functions pass `qpdf --check` ("No syntax or
  stream encoding errors found").
- **Tiling patterns** (PatternType 1), **blend-mode authoring** (`/BM`, `/CA`),
  transparency-group authoring and the renderer's four non-separable blend modes
  are deferred — only gradient fills ship here.

## [0.81.0] - 2026-06-24

Compact output — **object streams** + a **cross-reference stream** (PDF 1.5+,
ISO 32000-1 §7.5.7/§7.5.8). The serializer previously only wrote the classic
PDF 1.4 structure. Resolves
[#10](https://github.com/qrcommunication/gigapdf-lib/issues/10) (linearization
excepted — see notes).

### Added

- **`serialize::to_pdf_compressed(objects, trailer, use_object_streams)`** — packs
  every non-stream object into Flate-compressed `/Type /ObjStm` object streams
  (type-2 cross-reference entries) and writes a `/Type /XRef` cross-reference
  stream (`/W [1 4 2]`). With `use_object_streams = false` it writes the xref as a
  stream while keeping classic object bodies. Stream objects always stay direct.
- **`Document::save_optimized(object_streams, xref_streams)`** — the compact save
  path (uncompressed streams are Flate-compressed first, like `save_compressed`).
  `gp_save_optimized` / `doc.saveOptimized({ objectStreams, xrefStreams })`.
- Factored the shared `Document::flate_streams()` helper out of `save_compressed`.

### Notes

- **Conformance-validated**: both modes pass `qpdf --check` ("No syntax or stream
  encoding errors found") and a `qpdf --qdf` decompression round-trip — an
  independent authoritative validator, not just the engine's own parser.
- **Linearization** (Fast Web View, ISO 32000-1 Annex F) is a separate byte-layout
  optimization and is **not** produced here.

## [0.80.0] - 2026-06-24

Signature **verification** and **DocMDP certification** (ISO 32000-1 §12.8) — the
signing stack could produce signatures but not check them. Resolves
[#16](https://github.com/qrcommunication/gigapdf-lib/issues/16).

### Added

- **`crates/core/src/sign/verify.rs`** — a detached-CMS verifier built on the
  existing RustCrypto `cms`/`x509-cert`/`rsa` stack: recompute the `/ByteRange`
  SHA-256, check the CMS `messageDigest`, and validate the SignerInfo RSA
  signature under the signer certificate's key (trimming the `/Contents`
  zero-padding to the actual DER element first).
- **`Document::signatures() -> Vec<SignatureInfo>`** — list every `/Sig` field's
  `/V` with its metadata and `/ByteRange`. `gp_signatures_json` / `doc.signatures`.
- **`Document::verify_signatures(&pdf_bytes) -> Vec<SignatureReport>`** — verify
  each signature against the original bytes: `byte_range_ok`, `digest_ok`
  (integrity), `signature_ok`, `covers_whole_document`, signer CN, cert count,
  algorithm. `gp_verify_signatures` / `doc.verifySignatures`.
- **`Document::sign_certify(&Signer, name, reason, date, docmdp_p)`** — produce a
  **certified** PDF: a certifying signature plus the catalog `/Perms /DocMDP` and
  a `/Reference` DocMDP transform with the permission level (1 = no changes,
  2 = fill + sign, 3 = also annotate). `gp_sign_certify` / `doc.certify`.

### Notes

- **RSA + SHA-256** (what this engine produces) is verified; other algorithms are
  reported as `unsupported`. Live OCSP/CRL revocation checking, full
  chain-to-trusted-root validation, FieldMDP field-locking and ECDSA are out of
  scope (they need a trust store / network / clock the engine doesn't have).
  Verification needs the **original file bytes** (the `Document` doesn't retain
  them).

## [0.79.0] - 2026-06-24

Closes the interactive-forms gaps (ISO 32000-1 §12.7): signature fields,
field-level JavaScript, calculation order, field deletion and appearance
regeneration. Resolves
[#15](https://github.com/qrcommunication/gigapdf-lib/issues/15).

### Added

- **`Document::add_signature_field(page, name, rect, &style)`.** Lay out a
  *visible* signature field (`/FT /Sig`) the PAdES signing stack can target, and
  flag the AcroForm `/SigFlags`. `gp_add_signature_field` / `doc.addSignatureField`.
- **`Document::set_field_action(name, FieldTrigger, js)`.** Field-level
  JavaScript in a field's `/AA` for the `Keystroke` (`K`), `Format` (`F`),
  `Validate` (`V`) and `Calculate` (`C`) triggers — input masks, formatting,
  validation and computed values. `gp_set_field_script` / `doc.setFieldScript`.
- **`Document::set_calculation_order(&[name])`.** The AcroForm `/CO` calculation
  order. `gp_set_calculation_order` / `doc.setCalculationOrder`.
- **`Document::remove_field(name)`.** Delete a field from `/Fields`, `/CO` and
  every page's `/Annots`. `gp_remove_field` / `doc.removeField`.
- **`Document::regenerate_field_appearance(name)`.** Rebuild a field's `/AP` from
  its current value and style (text / choice / checkbox) after a programmatic
  value change. `gp_regenerate_field_appearance` / `doc.regenerateFieldAppearance`.
- New `form::FieldTrigger` enum (with `pdf_key` / `from_name`).

### Notes

- Hierarchical fields (`/Kids`, dotted names), per-field `/DR` font resources and
  XFA remain out of scope (XFA intentionally). `regenerate_field_appearance`
  returns `false` for a `/Kids` parent (e.g. a radio group).

## [0.78.0] - 2026-06-24

A single, shared **action & destination model** (ISO 32000-1 §12.6 / §12.3.2)
reused by links, the document open-action and outline bookmarks. Resolves
[#14](https://github.com/qrcommunication/gigapdf-lib/issues/14).

### Added

- **`Action` and `Destination` model (`crates/core/src/action`).** An `Action`
  enum — `GoTo`, `GoToR` (remote), `Uri`, `Named` (NextPage/PrevPage/FirstPage/
  LastPage), `Launch`, `JavaScript`, `SubmitForm`, `ResetForm` — and a
  `Destination` enum with **every** fit mode (`XYZ`, `Fit`, `FitH`, `FitV`,
  `FitR`, `FitB`, `FitBH`, `FitBV`, plus named destinations). `Action::from_json`
  parses the SDK's tagged-object shape; `build_dict` emits the PDF `/A` dictionary
  (and the `/D` array/name).
- **`Document::add_link(page, rect, &Action)`.** A general link carrying any
  action and any destination fit mode (previously links were URI- and
  page-jump-only). `gp_add_link` / `doc.addLink`.
- **`Document::set_open_action(&Action)`.** The document `/OpenAction`, performed
  when the file is opened. `gp_set_open_action` / `doc.setOpenAction`.
- **`Document::remove_link(page, index)`.** Remove the *n*-th `/Link` annotation
  on a page (links counted in order, other annotations untouched).
  `gp_remove_link` / `doc.removeLink`.
- **`Document::set_bookmarks(&[Bookmark])`.** Replace the outline with bookmarks
  that carry **any** `Action` (a `GoTo` becomes a `/Dest`, anything else an
  `/A`). `gp_set_bookmarks` / `doc.setBookmarks`. `set_outline` now delegates to
  it (each `page` → a `/Fit` GoTo), so its behaviour is unchanged.

### Notes

- Form-field widget actions are deferred to the forms issue (#15); they reuse
  this same `Action` model.

## [0.77.0] - 2026-06-24

Adds the missing geometric annotation subtypes and appearance regeneration.
Resolves [#13](https://github.com/qrcommunication/gigapdf-lib/issues/13).

### Added

- **Circle / Polygon / PolyLine / Caret annotations (core + WASM + SDK).**
  `add_circle_annotation`, `add_polygon_annotation`, `add_polyline_annotation` and
  `add_caret_annotation` create the geometric annotation subtypes with border
  width and interior colour (`/IC`), each with a generated `/AP` appearance stream.
  Exposed as `gp_add_circle_annot` / `gp_add_polygon_annot` /
  `gp_add_polyline_annot` / `gp_add_caret_annot` (WASM) and
  `doc.addCircleAnnotation` / `addPolygonAnnotation` / `addPolylineAnnotation` /
  `addCaretAnnotation` (SDK).
- **`regenerate_appearance(page, index)`.** Rebuild an existing annotation's
  `/AP /N` appearance from its stored geometry/style (after editing its colour,
  border or geometry), leaving every other key untouched — for Square, Circle,
  Line, Polygon, PolyLine, Highlight, Underline, StrikeOut, Ink and Caret.
  `gp_regenerate_appearance` / `doc.regenerateAppearance`.

### Notes

- Annotation **actions** (`/A`) are deferred to the action-model issue (#14);
  Sound/Screen/RichMedia subtypes remain out of scope (low priority). A **text
  watermark** already shipped as `add_watermark` / `doc.addWatermark` (positioned,
  with colour/opacity/rotation), so no new watermark API was added.

## [0.76.0] - 2026-06-24

General document metadata: read/write the catalog `/Metadata` **XMP** packet and
set the typed Info-dictionary fields, keeping `/Info` and XMP in sync. Resolves
[#7](https://github.com/qrcommunication/gigapdf-lib/issues/7).

### Added

- **XMP + typed Info metadata (core + WASM + SDK).** `Document::xmp()` reads the
  catalog `/Metadata` XMP packet (decoded) and `Document::set_xmp(&[u8])`
  replaces/creates it (stored uncompressed). `Document::set_info(&InfoFields)`
  writes the standard fields (Title/Author/Subject/Keywords/Creator/Producer/
  CreationDate/ModDate) to **both** the `/Info` dictionary **and** a regenerated
  XMP packet (`dc:`/`xmp:`/`pdf:` namespaces, PDF dates → ISO 8601), as a partial
  merge; `Document::info_fields()` reads them back, and `InfoFields::from_json`
  parses the SDK object. Exposed as `gp_get_xmp` / `gp_set_xmp` /
  `gp_set_info_json` (WASM) and `doc.getXmp()` / `doc.setXmp()` / `doc.setInfo()`
  (SDK), with the new `InfoFields` type. The existing single-key
  `set_metadata(key, value)` is unchanged (Info only).

### Changed

- The internal JSON object reader (`ObjReader`) is now `pub(crate)` so metadata
  (and future config parsers) can reuse it instead of duplicating a parser.

## [0.75.0] - 2026-06-24

Embedded file attachments become **writable** — add/replace/remove document-level
files, anchor `FileAttachment` annotations, and link **associated files** (`/AF`,
PDF/A-3) for hybrid e-invoices (Factur-X / ZUGFeRD / Order-X). Resolves
[#9](https://github.com/qrcommunication/gigapdf-lib/issues/9).

### Added

- **Attachment write API (core + WASM + SDK).** `Document::add_attachment(name,
  bytes, mime, desc)` embeds a file in `/Names /EmbeddedFiles` (FlateDecode-
  compressed; re-using a name replaces it); `Document::remove_attachment(name)`
  drops it (and its `/AF` link), returning whether one was removed;
  `Document::add_associated_file(name, bytes, mime, desc, relationship)` adds the
  file as an **associated file** — its filespec carries `/AFRelationship` and is
  linked from the catalog `/AF` array (the Factur-X/ZUGFeRD invoice-XML mechanism);
  `Document::add_file_attachment_annot(page, rect, name, icon)` anchors a visible
  `FileAttachment` annotation to an embedded file. Exposed as `gp_add_attachment` /
  `gp_add_associated_file` / `gp_remove_attachment` / `gp_add_file_attachment_annot`
  (WASM) and `doc.addAttachment` / `doc.addAssociatedFile` / `doc.removeAttachment`
  / `doc.addFileAttachmentAnnot` (SDK), with the new `AfRelationship` type. Sibling
  `/Names` subtrees (`/Dests`, `/JavaScript`, …) are preserved when the embedded-
  files tree is rewritten.

## [0.74.0] - 2026-06-24

Adds **page labels** (`/PageLabels`) — reading, authoring and resolving the
page-numbering schemes (roman front matter, prefixed appendices, …) that viewers
show in the page navigator. Resolves
[#8](https://github.com/qrcommunication/gigapdf-lib/issues/8).

### Added

- **Page labels — read/write + resolve (core + WASM + SDK).**
  `Document::page_labels()` reads the `/PageLabels` number tree (ISO 32000-1
  §12.4.2; traverses `/Nums` leaves and `/Kids` intermediates) into a sorted
  `Vec<PageLabelRange>` (`start_page` 1-based, `style`, `prefix`, `start_number`);
  `Document::set_page_labels(&[…])` writes a flat-`/Nums` tree (an empty slice
  clears `/PageLabels`); `Document::page_label(page)` formats the viewer-visible
  string (decimal, lower/upper roman, the repeating `a…z, aa…` letter scheme,
  with prefix and `/St` offset), falling back to the decimal page number outside
  any range. Exposed as `gp_page_labels_json` / `gp_set_page_labels` /
  `gp_page_label` (WASM) and `doc.getPageLabels()` / `doc.setPageLabels([…])` /
  `doc.pageLabel(n)` (SDK), with the `PageLabelRange` / `PageLabelStyle` types.

## [0.73.0] - 2026-06-24

Print-production release: the engine now reads and writes **all five ISO 32000-1
page boundary boxes** (MediaBox/CropBox/BleedBox/TrimBox/ArtBox), the prerequisite
for PDF/X export and any commercial-print pipeline (imposition, bleed, finished-size
trimming). Resolves [#6](https://github.com/qrcommunication/gigapdf-lib/issues/6).

### Added

- **Page boxes — full read/write (core + WASM + SDK).** `Document::page_boxes(page)`
  returns a `PageBoxes` with every box (`media`/`crop`/`bleed`/`trim`/`art`) as
  `[x0, y0, x1, y1]` points, with **ISO 32000-1 §14.11.2 inheritance and the per-box
  default chain applied** (CropBox→MediaBox; Bleed/Trim/Art→CropBox; MediaBox &
  CropBox inherited from an ancestor `/Pages` node), plus a `declared` set flagging
  which boxes are explicitly on the page vs inherited/defaulted.
  `Document::set_page_box(page, kind, [x0,y0,x1,y1])` writes one box, normalises the
  rectangle (reversed corners accepted) and **preserves sibling boxes** — boxes are
  no longer dropped on round-trip. Exposed as `gp_page_boxes_json` /
  `gp_set_page_box` (WASM, `kind` 0=media 1=crop 2=bleed 3=trim 4=art) and
  `doc.getPageBoxes(n)` / `doc.setPageBox(n, "trim", { x, y, w, h })` (SDK), with the
  `PAGE_BOX_KINDS` / `PageBoxKind` / `PageBoxes` types.
- **`EngineError::InvalidArgument`.** A new error variant for malformed caller
  arguments (e.g. a degenerate page-box rectangle or an unknown enum discriminant).

## [0.68.0] - 2026-06-23

Format-reach + import/render fidelity release: the unified model now exports
**Markdown / CSV / EPUB** end to end, Office/ODF import preserves far more
structure, the HTML→PDF renderer gains the remaining common CSS, and several
image-codec and rendering bugs are fixed.

### Added

- **Markdown / CSV / EPUB model export — FFI + SDK.** The unified editable model
  can now be raised to **Markdown** (`gp_model_to_md` / `modelToMd`), **CSV**
  (RFC 4180, `gp_model_to_csv` / `modelToCsv`) and **EPUB 3**
  (`gp_model_to_epub` / `modelToEpub`), alongside the existing
  DOCX/XLSX/PPTX/ODT/ODS/ODP/PDF/HTML/RTF targets.
- **Complete Markdown modelling.** `CodeBlock`, `Blockquote` and
  `HorizontalRule` are first-class in the model, giving a full Markdown
  round-trip — headings, runs, links, images, nested lists, GFM tables, code
  blocks, block-quotes, horizontal rules, footnotes and front-matter — rendered
  and exported consistently across every format.
- **Office / ODF import fidelity.** DOCX/XLSX/PPTX and **ODF (`.odt`/`.ods`/
  `.odp`)** import now preserves **images, hyperlinks, strikethrough, text
  highlighting, spreadsheet formulas, grouped shapes, charts, SmartArt text and
  master/layout (theme) inheritance**.
- **HTML / CSS → PDF — remaining common CSS.** **Radial** and **conic**
  gradients, **`font-weight` 100–900**, **`box-shadow`** (blur), **elliptical
  `border-radius`**, dashed/dotted borders, **`linear-gradient`** and
  **`position: sticky`**. (Earlier in the cycle: linear gradient + box-shadow +
  border-radius + dashed/dotted borders.)
- **OpenType text shaping.** **GPOS** mark positioning, **GSUB** contextual
  substitution, script selection and **Arabic joining**, wired into the text
  render path for complex scripts only (Latin output is byte-for-byte
  unchanged).
- **Image codecs.** **SVG `<text>`** rendering and **GIF multi-frame** decoding.
- **Run highlight.** Character-level `background` (`CharStyle.background`) is now
  painted and emitted across the HTML, PDF and Office paths.
- **`set_text_run_style`.** Run-level style bake exposed in core, FFI and the
  SDK.
- **Mermaid flowchart renderer.** `graph TD/LR` with node shapes, typed edges
  and arrowheads, laid out (Sugiyama) into PDF vectors in the HTML engine.

### Fixed

- **AVIF multi-tile decode (corrupt images > 9.4 MP).** A multi-tile AVIF
  (`tile_cols_log2 > 0 || tile_rows_log2 > 0`) was decoded as a single
  continuous tile, garbling pixels. The AV1 spec **forces** multi-tile on any
  picture above 4096×2304 px (~9.4 MP), so essentially every modern phone /
  camera AVIF was silently corrupted. Each tile is now decoded as the
  independent entropy + prediction unit it is (per-tile `Msac` + CDF set, tile
  MI bounds for partition geometry, intra prediction stopped at the tile
  boundary, neighbour contexts reset per tile); the in-loop filters still run
  once over the full frame. Single-tile (≤ 9.4 MP) and all existing fixtures
  are **byte-for-byte unchanged**; validated bit-exact against `dav1d`.
- **WebP lossless (VP8L).** Lossless transforms + meta-Huffman decoding — real
  `cwebp`/libwebp lossless images now decode correctly.

### Changed

- **Non-Device colorspaces resolved.** **Pattern** fills, and `Separation` /
  `ICCBased` colours used in content streams, are now unified through the raster
  colour resolver (consistent with the rasterizer) instead of falling back to a
  device default.
- **Docs honesty.** README corrected: the engine is **near-zero-dependency**
  (hand-written PDF/render/conversion core; **RustCrypto** for standardized
  crypto/signatures; **Boa** for the JS engine — the earlier from-scratch JS
  interpreter is gone), the suite is **1198 tests** (was “284”), and the
  released `.wasm` is **~5.6 MB** (was “~540 KB”, before Boa was bundled).

### Tests

- **1198** `gigapdf-core` tests green, `clippy` clean. New coverage for AVIF
  multi-tile (bit-exact vs `dav1d`, incl. loop filtering across tile seams),
  VP8L, SVG `<text>`, GIF multi-frame, OpenType shaping, the Markdown/CSV/EPUB
  exporters, Office/ODF import fidelity, and the new CSS.

## [0.67.0] - 2026-06-23

See [`sdk/CHANGELOG.md`](sdk/CHANGELOG.md) for 0.67.0 and earlier.
