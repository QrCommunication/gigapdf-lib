#!/usr/bin/env bash
# run.sh — document-conformance gate for gigapdf-lib.
#
# Generates fixtures from the SDK (native engine output) and validates each with
# REFERENCE validators — never a home-grown parser:
#   • PDF      → qpdf --check                   (ISO 32000 structural integrity)
#   • PDF/A    → veraPDF -f 1b|1a|2b|2u|2a|3b    (ISO 19005, archival profiles —
#                                                 incl. level A / Tagged PDF: 1a, 2a)
#   • Office   → OPC invariants         (ECMA-376 / ISO 29500: [Content_Types],
#                                        _rels, relation targets, XML well-formed)
#   • ODF      → ODF invariants         (ISO 26300: mimetype-first/STORED,
#                                        manifest, content.xml well-formed)
#
# A fixture is GATED: validate.py exit 0 = pass; exit 1 (non-conformant) and
# exit 2 (indeterminate — e.g. a validator missing) are BOTH hard failures, so a
# missing veraPDF can never let a PDF/A check pass vacuously.
#
# Schema-level validation (XSD ECMA-376 / RelaxNG ODF) is OPT-IN: drop the
# schemas under scripts/conformance/schemas/ (see README) and they get wired
# automatically. Until then the gate is structural (the schemas are not freely
# fetchable without manual vendoring — documented, not faked).
#
# Reusable locally: `bash scripts/conformance/run.sh`. Idempotent.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$HERE/fixtures}"
SCHEMA_DIR="${SCHEMA_DIR:-$HERE/schemas}"
SKILL_DIR="${DFC_SKILL_DIR:-$HOME/.claude/skills/document-format-conformance}"
VERAPDF_HOME="$HOME/.local/share/verapdf"
LOCAL_BIN="$HOME/.local/bin"

say()  { printf '\033[1;34m▸ %s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m! %s\033[0m\n' "$*" >&2; }
err()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; }

# --------------------------------------------------------------------------- #
# 1. Validators — prefer the live skill, else a self-contained vendored setup. #
# --------------------------------------------------------------------------- #
VALIDATE_PY=""
VENV_PY=""

setup_from_skill() {
  [ -f "$SKILL_DIR/scripts/validate.py" ] || return 1
  say "using skill validators: $SKILL_DIR"
  if [ ! -x "$SKILL_DIR/.venv/bin/python" ]; then
    bash "$SKILL_DIR/scripts/setup.sh" || warn "skill setup.sh reported issues — continuing with what is present"
  fi
  VALIDATE_PY="$SKILL_DIR/scripts/validate.py"
  VENV_PY="$SKILL_DIR/.venv/bin/python"
  [ -x "$VENV_PY" ] || VENV_PY="python3"
  return 0
}

setup_vendored() {
  say "using vendored validators: $HERE/validators"
  local venv="$HERE/.venv"
  if [ ! -x "$venv/bin/python" ]; then
    python3 -m venv "$venv"
  fi
  "$venv/bin/pip" install -q -U pip
  "$venv/bin/pip" install -q -r "$HERE/validators/requirements.txt"
  VALIDATE_PY="$HERE/validators/validate.py"
  VENV_PY="$venv/bin/python"

  # qpdf + xmllint via apt when available (CLI tools, not pip).
  local need=()
  command -v qpdf    >/dev/null 2>&1 || need+=(qpdf)
  command -v xmllint >/dev/null 2>&1 || need+=(libxml2-utils)
  if [ "${#need[@]}" -gt 0 ] && command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update -q || true
    sudo apt-get install -y -q "${need[@]}" || warn "apt install failed for: ${need[*]}"
  fi
  install_verapdf
}

# veraPDF headless install (mirrors the skill's installer) — required for PDF/A.
install_verapdf() {
  if command -v verapdf >/dev/null 2>&1 || [ -x "$VERAPDF_HOME/verapdf" ]; then
    ok "veraPDF already installed"
    return 0
  fi
  command -v java >/dev/null 2>&1 || { warn "Java absent — veraPDF needs a JRE (apt install default-jre)"; return 1; }
  local tmp; tmp="$(mktemp -d)"
  say "downloading veraPDF installer…"
  if ! curl -fsSL -o "$tmp/verapdf.zip" "https://software.verapdf.org/releases/verapdf-installer.zip"; then
    warn "veraPDF download failed"; rm -rf "$tmp"; return 1
  fi
  ( cd "$tmp" && unzip -q verapdf.zip )
  local jar; jar="$(find "$tmp" -name 'verapdf-izpack-installer-*.jar' | head -1)"
  cat > "$tmp/auto.xml" <<XML
<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<AutomatedInstallation langpack="eng">
    <com.izforge.izpack.panels.htmlhello.HTMLHelloPanel id="welcome"/>
    <com.izforge.izpack.panels.target.TargetPanel id="install_dir">
        <installpath>$VERAPDF_HOME</installpath>
    </com.izforge.izpack.panels.target.TargetPanel>
    <com.izforge.izpack.panels.packs.PacksPanel id="sdk_pack_select">
        <pack index="0" name="veraPDF GUI" selected="true"/>
        <pack index="1" name="veraPDF Mac and *nix Scripts" selected="true"/>
        <pack index="2" name="veraPDF Validation model" selected="true"/>
        <pack index="3" name="veraPDF Documentation" selected="false"/>
        <pack index="4" name="veraPDF Sample Plugins" selected="false"/>
    </com.izforge.izpack.panels.packs.PacksPanel>
    <com.izforge.izpack.panels.install.InstallPanel id="install"/>
    <com.izforge.izpack.panels.finish.FinishPanel id="finish"/>
</AutomatedInstallation>
XML
  rm -rf "$VERAPDF_HOME"
  java -jar "$jar" "$tmp/auto.xml" >/dev/null
  mkdir -p "$LOCAL_BIN"
  ln -sf "$VERAPDF_HOME/verapdf" "$LOCAL_BIN/verapdf"
  rm -rf "$tmp"
  export PATH="$LOCAL_BIN:$PATH"
  ok "veraPDF installed → $VERAPDF_HOME"
}

# --------------------------------------------------------------------------- #
# 2. SDK build (only if dist/wasm missing — keeps local runs self-sufficient). #
# --------------------------------------------------------------------------- #
ensure_sdk() {
  if [ -f "$REPO/sdk/dist/index.js" ] && [ -f "$REPO/sdk/gigapdf.wasm" ]; then
    ok "SDK build present"
    return 0
  fi
  say "building SDK (wasm + tsup)…"
  bash "$REPO/sdk/scripts/build-wasm.sh"
  ( cd "$REPO/sdk" && { [ -d node_modules ] || pnpm install --frozen-lockfile; } && pnpm build )
}

# --------------------------------------------------------------------------- #
# 3. Validate one fixture; exit 0 = pass, anything else = FAIL.                #
# --------------------------------------------------------------------------- #
PASS=0; FAIL=0
declare -a FAILED=()

gate() {
  local file="$1"; shift
  local label="$1"; shift                 # human label
  local path="$OUT_DIR/$file"
  [ -f "$path" ] || { err "$label: fixture missing ($file)"; FAIL=$((FAIL+1)); FAILED+=("$file [missing]"); return; }
  set +e
  "$VENV_PY" "$VALIDATE_PY" "$path" "$@" >"$OUT_DIR/$file.report.json" 2>&1
  local rc=$?
  set -e
  if [ "$rc" -eq 0 ]; then
    ok "$label — conformant"
    PASS=$((PASS+1))
  else
    local reason="non-conformant"; [ "$rc" -eq 2 ] && reason="indeterminate (validator missing/error)"
    err "$label — $reason (exit $rc)"
    sed 's/^/    /' "$OUT_DIR/$file.report.json" | head -40 >&2
    FAIL=$((FAIL+1)); FAILED+=("$file [$reason]")
  fi
}

# --------------------------------------------------------------------------- #
main() {
  setup_from_skill || setup_vendored
  [ -n "$VALIDATE_PY" ] || { err "no validate.py available"; exit 2; }
  export PATH="$LOCAL_BIN:$PATH"

  # Hard pre-flight: the gate is meaningless without these.
  command -v qpdf >/dev/null 2>&1 || { err "qpdf not found — cannot gate PDF structure"; exit 2; }
  if ! command -v verapdf >/dev/null 2>&1 && [ ! -x "$VERAPDF_HOME/verapdf" ]; then
    err "veraPDF not found — PDF/A checks would be indeterminate; refusing to gate vacuously"
    exit 2
  fi

  ensure_sdk

  say "generating fixtures from the SDK…"
  node "$REPO/scripts/conformance/gen-fixtures.mjs" "$OUT_DIR"

  # Optional schema validation (opt-in via vendored schemas).
  local xsd_arg=() rng_arg_odt=() rng_arg_ods=() rng_arg_odp=()
  if [ -d "$SCHEMA_DIR/ooxml" ]; then
    xsd_arg=(--xsd "$SCHEMA_DIR/ooxml")
    ok "OOXML XSD schemas found → schema validation enabled"
  else
    warn "no OOXML XSD schemas in $SCHEMA_DIR/ooxml — OOXML gate is structural (see README to vendor)"
  fi
  if [ -f "$SCHEMA_DIR/odf/OpenDocument-schema.rng" ]; then
    local rng="$SCHEMA_DIR/odf/OpenDocument-schema.rng"
    rng_arg_odt=(--rng "$rng"); rng_arg_ods=(--rng "$rng"); rng_arg_odp=(--rng "$rng")
    ok "ODF RelaxNG schema found → schema validation enabled"
  else
    warn "no ODF RelaxNG schema in $SCHEMA_DIR/odf — ODF gate is structural (see README to vendor)"
  fi

  say "validating fixtures…"
  # PDF (structural)
  gate "sample.pdf"          "PDF (qpdf)"
  # PDF/A — veraPDF, six conformance levels (incl. level A / Tagged PDF: 1a, 2a)
  gate "sample.pdfa-1b.pdf"  "PDF/A-1b (veraPDF)" --pdfa 1b
  gate "sample.pdfa-1a.pdf"  "PDF/A-1a (veraPDF)" --pdfa 1a
  gate "sample.pdfa-2b.pdf"  "PDF/A-2b (veraPDF)" --pdfa 2b
  gate "sample.pdfa-2u.pdf"  "PDF/A-2u (veraPDF)" --pdfa 2u
  gate "sample.pdfa-2a.pdf"  "PDF/A-2a (veraPDF)" --pdfa 2a
  gate "sample.pdfa-3b.pdf"  "PDF/A-3b (veraPDF)" --pdfa 3b
  # Office (OPC structural, + XSD if vendored)
  gate "sample.docx"         "DOCX (OPC)"  "${xsd_arg[@]}"
  gate "sample.xlsx"         "XLSX (OPC)"  "${xsd_arg[@]}"
  gate "sample.pptx"         "PPTX (OPC)"  "${xsd_arg[@]}"
  # ODF (structural, + RelaxNG if vendored)
  gate "sample.odt"          "ODT (ODF)"   "${rng_arg_odt[@]}"
  gate "sample.ods"          "ODS (ODF)"   "${rng_arg_ods[@]}"
  gate "sample.odp"          "ODP (ODF)"   "${rng_arg_odp[@]}"

  echo
  say "conformance summary: $PASS passed, $FAIL failed"
  if [ "$FAIL" -gt 0 ]; then
    err "regressed/failed fixtures:"
    printf '    - %s\n' "${FAILED[@]}" >&2
    exit 1
  fi
  ok "all conformance fixtures pass"
}

main "$@"
