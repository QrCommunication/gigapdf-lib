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
| `sample.docx` | OOXML | OPC structural (+ optional XSD) | ECMA-376 / ISO 29500 |
| `sample.xlsx` | OOXML | OPC structural (+ optional XSD) | ECMA-376 / ISO 29500 |
| `sample.pptx` | OOXML | OPC structural (+ optional XSD) | ECMA-376 / ISO 29500 |
| `sample.odt` | ODF | ODF structural (+ optional RelaxNG) | ISO 26300 |
| `sample.ods` | ODF | ODF structural (+ optional RelaxNG) | ISO 26300 |
| `sample.odp` | ODF | ODF structural (+ optional RelaxNG) | ISO 26300 |

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

## Strong schema validation (XSD / RelaxNG) — opt-in

The structural gate above always runs. **Full schema validation** (ECMA-376 XSD
for OOXML, OASIS RelaxNG for ODF) is **opt-in** because the official schemas are
**not freely fetchable from a stable URL** — they ship inside registration-gated
archives that must be vendored manually. Rather than fake a validation that does
not run, the gate documents the procedure and stays structural until the schemas
are dropped in.

When the schemas exist, `run.sh` wires them automatically:

```
scripts/conformance/schemas/
├── ooxml/            # ECMA-376 XSD set (→ validate.py --xsd)
│   ├── wml.xsd  sml.xsd  pml.xsd  shared-*.xsd  opc-*.xsd  ...
└── odf/
    └── OpenDocument-schema.rng   # OASIS ODF RelaxNG (→ validate.py --rng)
```

`schemas/` is git-ignored (large + license-restricted). To vendor:

### OOXML — ECMA-376 XSD

1. ECMA International → *Standards* → **ECMA-376** → download the
   *"Office Open XML File Formats"* archive (free).
2. Extract the XSD schema folder (Transitional **and** Strict are provided —
   match the variant the engine emits; gigapdf-lib emits Transitional).
   *Practical alternative:* the same XSDs ship with the `DocumentFormat.OpenXml`
   SDK.
3. Place them under `scripts/conformance/schemas/ooxml/` and re-run `run.sh`.
   Multi-file XSD imports are resolved by `xmllint --schema` (lxml alone does not
   follow relative imports reliably). See [`validators` ← skill `references/ooxml.md`](../../../.claude/skills/document-format-conformance/references/ooxml.md).

### ODF — OASIS RelaxNG

1. OASIS ODF 1.3 spec page → **OpenDocument-v1.3** archive → the `*-schema.rng`
   files (schema, manifest, dsig). 1.2/1.3 are back-compatible for most docs.
2. Place `OpenDocument-schema.rng` under `scripts/conformance/schemas/odf/` and
   re-run. Validated with `xmllint --relaxng`. See skill `references/odf.md`.

Once vendored, OOXML/ODF fixtures are validated against the full schema in
addition to the structural invariants, and any schema-level regression fails CI.

## Files

| File | Role |
|---|---|
| `run.sh` | The gate. Sets up validators, builds SDK if needed, generates + validates fixtures, strict pass/fail. |
| `gen-fixtures.mjs` | Emits the 11 fixtures from the SDK (native engine output). |
| `validators/validate.py`, `validators/_common.py` | Vendored reference-validator wrapper (veraPDF / qpdf / OPC / ODF). Self-contained. |
| `validators/requirements.txt` | Minimal Python deps for the validators (`pikepdf`, `lxml`). |
| `schemas/` *(git-ignored)* | Vendored XSD / RelaxNG for opt-in schema validation. |
| `fixtures/` *(git-ignored)* | Generated fixtures + `*.report.json` reports. |
