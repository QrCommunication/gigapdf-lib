#!/usr/bin/env bash
# fetch-schemas.sh — deterministically provision the official validation schemas
# for the strong (schema-level) conformance gate, WITHOUT vendoring them in git.
#
#   • OOXML  → ECMA-376 (ISO/IEC 29500) **Transitional** XSD set
#              (wml/sml/pml/dml/shared-*.xsd) — the variant gigapdf-lib emits.
#   • ODF    → OASIS / ISO 26300 OpenDocument **RelaxNG** schema (content/styles/meta).
#   • xml.xsd → W3C xml namespace schema, required to resolve xml:space / xml:lang
#               that the ECMA schemas import WITHOUT a schemaLocation.
#
# Each download is from a PINNED, immutable URL and is verified against a known
# SHA-256 before use — a fetch/integrity failure is FATAL (the gate never passes
# vacuously on a missing/altered schema). Output lands under
# scripts/conformance/schemas/{ooxml,odf}/ (git-ignored). Idempotent: re-running
# with the schemas already present is a no-op (checksums re-verified cheaply).
#
# Licensing: the ECMA-376 and OASIS ODF schemas are published by ECMA / OASIS and
# are NOT redistributed in this repo — they are fetched at gate time from their
# canonical hosts. See README.md "Strong schema validation".
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCHEMA_DIR="${SCHEMA_DIR:-$HERE/schemas}"
OOXML_DIR="$SCHEMA_DIR/ooxml"
ODF_DIR="$SCHEMA_DIR/odf"
CACHE="${SCHEMA_CACHE:-$HERE/.schema-cache}"

say()  { printf '\033[1;34m▸ %s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }
err()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; }

# --- Pinned sources (URL + SHA-256). Verified 2026-06-25. -------------------- #
# ECMA-376, 2nd edition (Dec 2008) full archive. Part 4 carries the Transitional
# schemas; the 5th-edition archive ships Strict ONLY, hence the 2nd edition here.
ECMA_URL="https://www.ecma-international.org/wp-content/uploads/ECMA-376_2nd_edition_december_2008.zip"
ECMA_SHA="7ce0080a5b111082414491d0a8531f2cd2f801e3157ad7475db88bdbb2c6c937"
# Inner zip (a 2nd integrity gate on the exact XSD payload we extract).
ECMA_PART4="ECMA-376, Second Edition, Part 4 - Transitional Migration Features.zip"
ECMA_XSD_ZIP="OfficeOpenXML-XMLSchema-Transitional.zip"
ECMA_XSD_ZIP_SHA="0c95eede4bc6505f67699fa0ad7e3c48deed7447c4457868c077636a56fcb047"

# OASIS OpenDocument 1.3 RelaxNG schema (single file).
ODF_URL="https://docs.oasis-open.org/office/OpenDocument/v1.3/os/schemas/OpenDocument-v1.3-schema.rng"
ODF_SHA="40bad03efdbb02825230d357da0aa6ac679934c5bf56c6281752c0c24d58e4e6"

# W3C xml namespace schema (resolves xml:space / xml:lang for the ECMA imports).
# Verified by a structural invariant rather than a hash: the W3C occasionally
# re-edits trailing whitespace, but the declarations we rely on are stable.
XMLXSD_URL="https://www.w3.org/2001/xml.xsd"

sha256_of() { sha256sum "$1" | cut -d' ' -f1; }

verify() { # <file> <expected-sha> <label>
  local got; got="$(sha256_of "$1")"
  if [ "$got" != "$2" ]; then
    err "checksum MISMATCH for $3"
    err "  expected $2"
    err "  got      $got"
    return 1
  fi
  ok "$3 checksum verified"
}

need_ooxml() { [ ! -f "$OOXML_DIR/wml.xsd" ] || [ ! -f "$OOXML_DIR/sml.xsd" ] || \
               [ ! -f "$OOXML_DIR/pml.xsd" ] || [ ! -f "$OOXML_DIR/xml.xsd" ]; }
need_odf()   { [ ! -f "$ODF_DIR/OpenDocument-schema.rng" ]; }

fetch_ooxml() {
  need_ooxml || { ok "OOXML XSD set already present"; return 0; }
  command -v curl  >/dev/null 2>&1 || { err "curl required to fetch schemas"; return 1; }
  command -v unzip >/dev/null 2>&1 || { err "unzip required to extract schemas"; return 1; }
  mkdir -p "$OOXML_DIR" "$CACHE"

  local top="$CACHE/ecma376-2nd.zip"
  if [ ! -f "$top" ] || ! sha256_of "$top" | grep -q "^$ECMA_SHA$"; then
    say "downloading ECMA-376 2nd edition (~53 MB)…"
    curl -fsSL --retry 3 -o "$top" "$ECMA_URL" || { err "ECMA-376 download failed"; return 1; }
  fi
  verify "$top" "$ECMA_SHA" "ECMA-376 archive" || return 1

  local work; work="$(mktemp -d)"
  # nested: top.zip → Part 4.zip → OfficeOpenXML-XMLSchema-Transitional.zip → *.xsd
  unzip -o -q "$top" "$ECMA_PART4" -d "$work"
  unzip -o -q "$work/$ECMA_PART4" "$ECMA_XSD_ZIP" -d "$work"
  verify "$work/$ECMA_XSD_ZIP" "$ECMA_XSD_ZIP_SHA" "OOXML Transitional XSD payload" || { rm -rf "$work"; return 1; }
  unzip -o -q "$work/$ECMA_XSD_ZIP" -d "$OOXML_DIR"
  rm -rf "$work"

  # W3C xml.xsd (verify by structural invariant, robust to whitespace edits).
  say "downloading W3C xml.xsd…"
  curl -fsSL --retry 3 -o "$OOXML_DIR/xml.xsd" "$XMLXSD_URL" || { err "xml.xsd download failed"; return 1; }
  if ! grep -q 'http://www.w3.org/XML/1998/namespace' "$OOXML_DIR/xml.xsd" \
     || ! grep -q 'name="space"' "$OOXML_DIR/xml.xsd"; then
    err "xml.xsd does not look like the W3C xml namespace schema — refusing"
    return 1
  fi
  ok "OOXML Transitional XSD set ready ($(ls "$OOXML_DIR"/*.xsd | wc -l) schemas) → $OOXML_DIR"
}

fetch_odf() {
  need_odf || { ok "ODF RelaxNG schema already present"; return 0; }
  command -v curl >/dev/null 2>&1 || { err "curl required to fetch schemas"; return 1; }
  mkdir -p "$ODF_DIR"
  say "downloading OASIS ODF 1.3 RelaxNG schema…"
  curl -fsSL --retry 3 -o "$ODF_DIR/OpenDocument-schema.rng" "$ODF_URL" \
    || { err "ODF RelaxNG download failed"; return 1; }
  verify "$ODF_DIR/OpenDocument-schema.rng" "$ODF_SHA" "ODF RelaxNG schema" || return 1
  ok "ODF RelaxNG schema ready → $ODF_DIR/OpenDocument-schema.rng"
}

main() {
  local rc=0
  fetch_ooxml || rc=1
  fetch_odf   || rc=1
  if [ "$rc" -ne 0 ]; then
    err "schema provisioning FAILED — strong schema validation cannot run."
    err "  (a missing/altered schema must never let the gate pass vacuously)"
    exit 1
  fi
  ok "all conformance schemas provisioned"
}

main "$@"
