# Document-format conformance gate

Continuously proves that **what the engine emits stays in spec** — so an archival
(PDF/A) or Office/ODF regression can never ship silently (the way PDF/A did before
it was fixed). Wired into CI via [`.github/workflows/conformance.yml`](../../.github/workflows/conformance.yml).

The gate validates the engine's *real* output with **reference validators only** —
never a home-grown parser:

| Fixture | Format | Reference validator | Spec |
|---|---|---|---|
| `sample.pdf` | PDF | **qpdf** `--check` (+ pikepdf 2nd opinion) | ISO 32000 |
| `sample.pdfa-1b.pdf` | PDF/A-1b | **veraPDF** `-f 1b` | ISO 19005-1 |
| `sample.pdfa-1a.pdf` | PDF/A-1a | **veraPDF** `-f 1a` | ISO 19005-1 (Tagged, level A) |
| `sample.pdfa-2b.pdf` | PDF/A-2b | **veraPDF** `-f 2b` | ISO 19005-2 |
| `sample.pdfa-2u.pdf` | PDF/A-2u | **veraPDF** `-f 2u` | ISO 19005-2 (Unicode) |
| `sample.pdfa-2a.pdf` | PDF/A-2a | **veraPDF** `-f 2a` | ISO 19005-2 (Tagged, level A) |
| `sample.pdfa-3b.pdf` | PDF/A-3b | **veraPDF** `-f 3b` | ISO 19005-3 |
| `sample.docx` | OOXML | OPC structural **+ ECMA-376 XSD** (`xmllint --schema`) | ECMA-376 / ISO 29500 (Transitional) |
| `sample.xlsx` | OOXML | OPC structural **+ ECMA-376 XSD** (`xmllint --schema`) | ECMA-376 / ISO 29500 (Transitional) |
| `sample.pptx` | OOXML | OPC structural **+ ECMA-376 XSD** (`xmllint --schema`) | ECMA-376 / ISO 29500 (Transitional) |
| `sample.odt` | ODF | ODF structural **+ OASIS RelaxNG** (`xmllint --relaxng`) | ISO 26300 |
| `sample.ods` | ODF | ODF structural **+ OASIS RelaxNG** (`xmllint --relaxng`) | ISO 26300 |
| `sample.odp` | ODF | ODF structural **+ OASIS RelaxNG** (`xmllint --relaxng`) | ISO 26300 |

All fixtures are derived from one embedded HTML page rendered by the native
`htmlToPdf` engine, then exported through `toPdfA(level)` and
`toDocx/toXlsx/toPptx/toOdt/toOds/toOdp`.

## What "gated" means

`run.sh` calls `validate.py` per fixture and treats the exit code strictly:

- **0** → conformant (pass)
- **1** → non-conformant (**fail**)
- **2** → indeterminate, e.g. a validator is missing (**fail** — never a vacuous pass)

A hard pre-flight refuses to run at all if **qpdf** or **veraPDF** is absent, so a
missing PDF/A validator can't quietly downgrade those checks to "not tested".

## Run it locally

```bash
bash scripts/conformance/run.sh
```

Idempotent. It will, in order:

1. Make the reference validators available — preferring the
   [`document-format-conformance` skill](../../../.claude/skills/document-format-conformance)
   if present, otherwise a **self-contained vendored fallback**: a local
   `.venv` from [`validators/requirements.txt`](validators/requirements.txt) +
   `qpdf`/`xmllint` (apt) + headless **veraPDF** under `~/.local/share/verapdf`.
2. Build the SDK (`sdk/scripts/build-wasm.sh` + `pnpm build`) if `sdk/dist` /
   `sdk/gigapdf.wasm` are missing.
3. Generate fixtures via [`gen-fixtures.mjs`](gen-fixtures.mjs) into `fixtures/`.
4. Validate each and print a pass/fail summary (JSON reports land next to each
   fixture as `*.report.json`).

The [`validators/`](validators) directory vendors `validate.py` + `_common.py`
straight from the skill so the gate is **self-contained on any runner** — it does
not depend on a user-home skill being present.

## Strong schema validation (ECMA-376 XSD / OASIS RelaxNG)

The structural gate above always runs. On top of it, every Office/ODF fixture is
validated against its **official element-model schema**, failing the build on any
violation:

- **OOXML** → each XML part is validated with `xmllint --schema` against the
  **ECMA-376 / ISO 29500 Transitional** XSD set (the variant gigapdf-lib emits).
  `validate.py` maps each part to its top-level schema
  (`word/document.xml`→`wml.xsd`, `xl/workbook.xml` + `xl/worksheets/sheet*.xml` +
  `xl/styles.xml`→`sml.xsd`, `ppt/presentation.xml` + `ppt/slides/slide*.xml`→
  `pml.xsd`, `docProps/app.xml`→`shared-documentPropertiesExtended.xsd`, …). The
  OPC-only parts (`[Content_Types].xml`, `_rels`, `docProps/core.xml`) have no
  format XSD and remain covered by the structural checks.
- **ODF** → `content.xml`, `styles.xml` and `meta.xml` are validated with
  `xmllint --relaxng` against the **OASIS / ISO 26300 OpenDocument RelaxNG** schema.

### Schemas are fetched, not vendored (license + size)

The ECMA-376 and OASIS schemas are not redistributed in this repo. They are
provisioned by [`fetch-schemas.sh`](fetch-schemas.sh) from **pinned URLs**, each
verified against a **SHA-256** before use — a missing or altered schema is a
**fatal** error (the gate never silently downgrades to structural-only):

| Schema | Pinned source | Integrity |
|---|---|---|
| ECMA-376 Transitional XSD set (`wml/sml/pml/dml/shared-*.xsd`) | `ECMA-376_2nd_edition_december_2008.zip` (ecma-international.org) → Part 4 → `OfficeOpenXML-XMLSchema-Transitional.zip` | SHA-256 on the outer archive **and** the inner XSD payload |
| OASIS ODF 1.3 RelaxNG (`OpenDocument-schema.rng`) | `docs.oasis-open.org/.../OpenDocument-v1.3/os/schemas/OpenDocument-v1.3-schema.rng` | SHA-256 |
| W3C `xml.xsd` (resolves `xml:space`/`xml:lang`) | `www.w3.org/2001/xml.xsd` | structural invariant |

> The ECMA-376 **5th edition** archive ships the *Strict* schemas only; the
> **2nd edition** is pinned because it carries the *Transitional* set, whose
> namespaces (`…/2006/main`) match what the engine — and Word/Excel/PowerPoint —
> actually write.
>
> The ECMA XSDs `<xsd:import>` the `xml` namespace **without** a `schemaLocation`
> (the spec leaves it to the consumer). `validate.py` resolves this **without
> editing the official schemas**: a tiny generated driver XSD imports the W3C
> `xml.xsd` alongside the real part schema, so both populate one `xmllint` schema
> set. (Without this, `xmllint` cannot compile `wml.xsd`: *"xml:space does not
> resolve"*.)

`fetch-schemas.sh` writes to `scripts/conformance/schemas/{ooxml,odf}/` (git-ignored)
and caches the raw downloads in `.schema-cache/`. It is idempotent. In CI the fetch
is a **dedicated hard-failing step** (and the gate also runs with `REQUIRE_SCHEMAS=1`),
with the downloads cached across runs.

### Known pre-existing exporter violations (baseline)

Wiring the strong gate surfaced a few **genuine exporter output bugs** (the engine
emits parts that Word/LibreOffice accept but that are not strictly schema-valid).
Those are not fixed here — this gate owns *validation*, the exporter fixes belong
to a separate Office-export follow-up — so they are **waived precisely** via
[`known-schema-issues.json`](known-schema-issues.json): each entry waives exactly
one `part` + error-`signature`. A **new or regressed** schema violation (any other
part or signature) still fails the build; once an exporter is fixed, its entry
shows up as stale (non-fatal) so it can be removed. Current entries:

| Fixture | Part | Violation |
|---|---|---|
| `sample.docx` | `word/document.xml` | `wps:wsp` (MS-2010 wordprocessingShape) emitted raw, not wrapped in `mc:AlternateContent`/`mc:Fallback` |
| `sample.xlsx` | `xl/worksheets/sheet1.xml` | `xml:space="preserve"` on the inline-string `<t>` — invalid (SpreadsheetML `CT_Rst` `t` is `ST_Xstring`, no attributes) |
| `sample.ods` | `content.xml` | `<table:table-row>` emitted with no preceding `<table:table-column>` (ODF requires columns before rows) |

## Run it locally with strong schema validation

```bash
bash scripts/conformance/run.sh          # auto-fetches the schemas if absent
# or force a (re)fetch first:
bash scripts/conformance/fetch-schemas.sh
```

`run.sh` auto-invokes `fetch-schemas.sh` when the schemas are missing. Offline, it
falls back to the **structural** gate with a clear warning (set `REQUIRE_SCHEMAS=1`
to make a failed fetch fatal locally too, as CI does). To validate a single file
by hand:

```bash
scripts/conformance/.venv/bin/python scripts/conformance/validators/validate.py \
  some.docx --xsd scripts/conformance/schemas/ooxml \
  --known-issues scripts/conformance/known-schema-issues.json
scripts/conformance/.venv/bin/python scripts/conformance/validators/validate.py \
  some.odt --rng scripts/conformance/schemas/odf/OpenDocument-schema.rng
```

## Files

| File | Role |
|---|---|
| `run.sh` | The gate. Sets up validators, provisions schemas, builds SDK if needed, generates + validates fixtures, strict pass/fail. |
| `fetch-schemas.sh` | Provisions the official ECMA-376 XSD / OASIS RelaxNG schemas from pinned URLs (checksum-verified). |
| `gen-fixtures.mjs` | Emits the 13 fixtures from the SDK (native engine output). |
| `validators/validate.py`, `validators/_common.py` | Vendored reference-validator wrapper (veraPDF / qpdf / OPC + XSD / ODF + RelaxNG). Self-contained. |
| `validators/requirements.txt` | Minimal Python deps for the validators (`pikepdf`, `lxml`). |
| `known-schema-issues.json` | Baseline of documented, pre-existing exporter schema violations (precise part+signature waivers). |
| `schemas/` *(git-ignored)* | Fetched XSD / RelaxNG for schema validation. |
| `.schema-cache/` *(git-ignored)* | Cached raw schema downloads. |
| `fixtures/` *(git-ignored)* | Generated fixtures + `*.report.json` reports. |
