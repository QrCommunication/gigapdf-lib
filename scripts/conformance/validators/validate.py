#!/usr/bin/env python3
"""validate.py — validation de conformité structurelle + sémantique.

N'implémente AUCUN parseur maison : orchestre les validateurs de référence.
  PDF      → qpdf --check (intégrité ISO 32000) + pikepdf (2e avis)
  PDF/A    → veraPDF (profil 1b/2b/3b/3u/ua1…) si --pdfa donné
  OOXML    → ZIP + [Content_Types].xml + _rels résolus + parts XML well-formed
             (+ XSD ECMA-376 fort via --xsd DIR : xmllint --schema par part,
              chaque part mappée vers wml/sml/pml/dml/shared-*.xsd)
  ODF      → mimetype 1ère entrée/STORED/valeur + manifest + content.xml
             (+ RelaxNG ODF fort via --rng FICHIER : content/styles/meta.xml)

Sortie : rapport JSON normalisé. Exit 0 conforme / 1 non conforme / 2 indéterminé.

Usage :
  python validate.py <fichier> [--pdfa 3b] [--xsd <dir>] [--rng <schema.rng>]
"""
from __future__ import annotations

import argparse
import posixpath
import sys
import zipfile

import _common as C

C.reexec_in_venv()
C.harden_path()

CT_NS = "http://schemas.openxmlformats.org/package/2006/content-types"
REL_NS = "http://schemas.openxmlformats.org/package/2006/relationships"
ODF_MIMETYPES = {
    "application/vnd.oasis.opendocument.text",
    "application/vnd.oasis.opendocument.spreadsheet",
    "application/vnd.oasis.opendocument.presentation",
    "application/vnd.oasis.opendocument.graphics",
}


# --------------------------------------------------------------------------- #
def validate_pdf(path: str, pdfa: str | None) -> tuple[list[dict], dict]:
    import pikepdf

    checks, tools = [], {}
    qpdf = C.require_tool("qpdf")
    tools["qpdf"] = C.run([qpdf, "--version"]).stdout.split("\n")[0]
    r = C.run([qpdf, "--check", path])
    out = (r.stdout + r.stderr).strip()
    ok, summary = C.qpdf_verdict(r.returncode, out)
    checks.append(C.check(
        "qpdf --check (intégrité structurelle)",
        ok,
        f"{summary} | {out[:1100]}" if out else summary,
        "ISO 32000-2 §7 (file structure, xref, trailer)",
    ))
    try:
        with pikepdf.open(path) as pdf:
            checks.append(C.check("pikepdf open (2e avis parseur)", True,
                                  f"{len(pdf.pages)} page(s), encrypted={pdf.is_encrypted}"))
    except Exception as e:  # noqa: BLE001
        checks.append(C.check("pikepdf open (2e avis parseur)", False, str(e)))

    if pdfa:
        vera = C.find_tool("verapdf")
        if not vera:
            checks.append(C.check(f"PDF/A {pdfa} (veraPDF)", None, "veraPDF non installé — " + C.SETUP_HINT))
        else:
            tools["verapdf"] = C.run([vera, "--version"]).stdout.strip().split("\n")[0]
            rv = C.run([vera, "-f", pdfa, "--format", "text", path], timeout=300)
            txt = (rv.stdout + rv.stderr).strip()
            passed = txt.startswith("PASS") or " PASS" in txt.split("\n")[0]
            checks.append(C.check(f"PDF/A {pdfa} (veraPDF)", passed, txt[:1500],
                                  "ISO 19005 (PDF/A) — profil veraPDF"))
    return checks, tools


# --------------------------------------------------------------------------- #
def _wellformed(data: bytes) -> tuple[bool, str]:
    from lxml import etree
    try:
        etree.fromstring(data)
        return True, "well-formed"
    except etree.XMLSyntaxError as e:
        return False, str(e)


def validate_ooxml(path: str, xsd_dir: str | None) -> tuple[list[dict], dict]:
    from lxml import etree

    checks: list[dict] = []
    with zipfile.ZipFile(path) as z:
        names = set(z.namelist())
        # [Content_Types].xml obligatoire (OPC, ISO/IEC 29500-2 §10.1.2)
        has_ct = "[Content_Types].xml" in names
        checks.append(C.check("[Content_Types].xml présent", has_ct, spec="OPC ISO 29500-2 §10.1.2"))
        declared_overrides: set[str] = set()
        if has_ct:
            wf, det = _wellformed(z.read("[Content_Types].xml"))
            checks.append(C.check("[Content_Types].xml well-formed", wf, det))
            if wf:
                root = etree.fromstring(z.read("[Content_Types].xml"))
                declared_overrides = {el.get("PartName") for el in root.findall(f"{{{CT_NS}}}Override")}
        # _rels/.rels racine
        checks.append(C.check("_rels/.rels présent", "_rels/.rels" in names, spec="OPC §9.3"))
        # résolution des cibles de relations (TargetMode interne)
        broken, total = [], 0
        for n in names:
            if not n.endswith(".rels"):
                continue
            try:
                rroot = etree.fromstring(z.read(n))
            except etree.XMLSyntaxError as e:
                broken.append(f"{n}: XML invalide ({e})")
                continue
            base = posixpath.dirname(posixpath.dirname(n))  # _rels parent
            for rel in rroot.findall(f"{{{REL_NS}}}Relationship"):
                if rel.get("TargetMode") == "External":
                    continue
                total += 1
                tgt = posixpath.normpath(posixpath.join(base, rel.get("Target", "")))
                if tgt not in names:
                    broken.append(f"{n} → {rel.get('Target')} (cible absente)")
        checks.append(C.check("relations internes résolues", not broken,
                              f"{total} relation(s), {len(broken)} cassée(s): {broken[:5]}",
                              "OPC §9.3 relationship targets"))
        # well-formedness de toutes les parts XML
        bad_xml = []
        for n in names:
            if n.endswith(".xml") or n.endswith(".rels"):
                wf, det = _wellformed(z.read(n))
                if not wf:
                    bad_xml.append(f"{n}: {det}")
        checks.append(C.check("parts XML well-formed", not bad_xml, f"{len(bad_xml)} invalide(s): {bad_xml[:5]}"))

    if xsd_dir:
        checks.append(_xsd_check(path, xsd_dir))
    else:
        checks.append(C.check("validation XSD ECMA-376", None,
                              "Schémas non fournis (--xsd DIR). Voir references/ooxml.md pour les obtenir."))
    return checks, {"lxml": __import__("lxml.etree", fromlist=["__version__"]).__version__}


# --------------------------------------------------------------------------- #
def validate_odf(path: str, fmt: dict, rng: str | None) -> tuple[list[dict], dict]:
    from lxml import etree

    checks: list[dict] = []
    with zipfile.ZipFile(path) as z:
        infos = z.infolist()
        names = set(z.namelist())
        first = infos[0] if infos else None
        # mimetype : 1ère entrée, STORED, valeur ODF — ISO 26300-3 §3.3
        checks.append(C.check("mimetype = 1ère entrée du ZIP", bool(first and first.filename == "mimetype"),
                              spec="ODF ISO 26300-3 §3.3"))
        checks.append(C.check("mimetype STORED (non compressé)",
                              bool(first and first.compress_type == zipfile.ZIP_STORED),
                              spec="ODF ISO 26300-3 §3.3"))
        mt = fmt.get("mimetype", "")
        checks.append(C.check("mimetype valeur ODF reconnue", mt in ODF_MIMETYPES, mt))
        # manifest
        has_manifest = "META-INF/manifest.xml" in names
        checks.append(C.check("META-INF/manifest.xml présent", has_manifest, spec="ODF §3.4"))
        if has_manifest:
            wf, det = _wellformed(z.read("META-INF/manifest.xml"))
            checks.append(C.check("manifest.xml well-formed", wf, det))
        # content.xml
        has_content = "content.xml" in names
        checks.append(C.check("content.xml présent", has_content))
        if has_content:
            wf, det = _wellformed(z.read("content.xml"))
            checks.append(C.check("content.xml well-formed", wf, det))

    if rng:
        checks.append(_rng_check(path, rng))
    else:
        checks.append(C.check("validation RelaxNG ODF", None,
                              "Schéma non fourni (--rng FICHIER). Voir references/odf.md."))
    return checks, {"lxml": __import__("lxml.etree", fromlist=["__version__"]).__version__}


# --------------------------------------------------------------------------- #
# OOXML XSD validation (ECMA-376 / ISO 29500, Transitional).
#
# Each OOXML XML part is validated against its top-level schema. The ECMA XSD set
# declares the part roots as global elements, so `xmllint --schema <part>.xsd`
# validates a part's root element against the right declaration and follows the
# schema's <xsd:import>s (dml-main, shared-*) relative to the schema dir.
#
# Gotcha handled here: the ECMA wml/sml/pml schemas reference xml:space / xml:lang
# but <xsd:import namespace=".../XML/1998/namespace"> with NO schemaLocation (the
# spec leaves it to the consumer — see the commented block in wml.xsd). Without it
# xmllint cannot compile the schema ("xml:space does not resolve"). We resolve it
# WITHOUT touching the official schemas: a tiny generated driver schema imports the
# W3C xml.xsd alongside the real part schema, so both populate one schema set.

# part-path glob → top-level schema file (relative to the vendored ooxml/ dir).
# Order matters: first matching pattern wins.
_OOXML_PART_SCHEMA = [
    # WordprocessingML
    ("word/document.xml", "wml.xsd"),
    ("word/document2.xml", "wml.xsd"),
    ("word/glossary/document.xml", "wml.xsd"),
    ("word/styles.xml", "wml.xsd"),
    ("word/numbering.xml", "wml.xsd"),
    ("word/settings.xml", "wml.xsd"),
    ("word/webSettings.xml", "wml.xsd"),
    ("word/fontTable.xml", "wml.xsd"),
    ("word/footnotes.xml", "wml.xsd"),
    ("word/endnotes.xml", "wml.xsd"),
    ("word/header*.xml", "wml.xsd"),
    ("word/footer*.xml", "wml.xsd"),
    ("word/comments.xml", "wml.xsd"),
    # SpreadsheetML
    ("xl/workbook.xml", "sml.xsd"),
    ("xl/worksheets/sheet*.xml", "sml.xsd"),
    ("xl/chartsheets/sheet*.xml", "sml.xsd"),
    ("xl/styles.xml", "sml.xsd"),
    ("xl/sharedStrings.xml", "sml.xsd"),
    ("xl/comments*.xml", "sml.xsd"),
    ("xl/calcChain.xml", "sml.xsd"),
    ("xl/tables/table*.xml", "sml.xsd"),
    ("xl/pivotTables/pivotTable*.xml", "sml.xsd"),
    ("xl/drawings/drawing*.xml", "dml-spreadsheetDrawing.xsd"),
    ("word/drawings/*.xml", "dml-wordprocessingDrawing.xsd"),
    # PresentationML
    ("ppt/presentation.xml", "pml.xsd"),
    ("ppt/slides/slide*.xml", "pml.xsd"),
    ("ppt/slideMasters/slideMaster*.xml", "pml.xsd"),
    ("ppt/slideLayouts/slideLayout*.xml", "pml.xsd"),
    ("ppt/notesSlides/notesSlide*.xml", "pml.xsd"),
    ("ppt/notesMasters/notesMaster*.xml", "pml.xsd"),
    ("ppt/handoutMasters/handoutMaster*.xml", "pml.xsd"),
    ("ppt/presProps.xml", "pml.xsd"),
    ("ppt/viewProps.xml", "pml.xsd"),
    ("ppt/tableStyles.xml", "dml-main.xsd"),
    # DrawingML (charts/themes referenced from any document type)
    ("ppt/theme/theme*.xml", "dml-main.xsd"),
    ("word/theme/theme*.xml", "dml-main.xsd"),
    ("xl/theme/theme*.xml", "dml-main.xsd"),
    ("*/charts/chart*.xml", "dml-chart.xsd"),
    # Shared extended document properties
    ("docProps/app.xml", "shared-documentPropertiesExtended.xsd"),
    ("docProps/custom.xml", "shared-documentPropertiesCustom.xsd"),
]

_W3C_XML_XSD_NS = "http://www.w3.org/XML/1998/namespace"


def _fnmatch_part(name: str, pattern: str) -> bool:
    import fnmatch
    # Patterns use POSIX-style ZIP paths; fnmatch on the full part name. The '*'
    # in "*/charts/chart*.xml" must cross '/' (ppt|xl|word), so normalise to a
    # plain fnmatch which already treats '*' as any-run-including-slash here.
    return fnmatch.fnmatch(name, pattern)


def _schema_for_part(name: str) -> str | None:
    for pat, schema in _OOXML_PART_SCHEMA:
        if _fnmatch_part(name, pat):
            return schema
    return None


def _ensure_xml_xsd(xsd_dir: str) -> str | None:
    """Locate (or fetch) the W3C xml.xsd next to the OOXML schemas.

    The ECMA schemas import the xml namespace without a schemaLocation; xml.xsd
    supplies xml:space / xml:lang. Prefer a vendored copy; if absent and the
    network allows, fetch the canonical W3C copy once (cached in xsd_dir)."""
    import os
    local = os.path.join(xsd_dir, "xml.xsd")
    if os.path.isfile(local):
        return local
    # Offline-friendly: only attempt a fetch if curl is present; never hard-fail
    # here — _xsd_check reports indeterminate if xml.xsd cannot be provided.
    curl = C.find_tool("curl")
    if curl:
        r = C.run([curl, "-fsSL", "-o", local, "https://www.w3.org/2001/xml.xsd"], timeout=30)
        if r.returncode == 0 and os.path.isfile(local) and os.path.getsize(local) > 0:
            return local
    return None


def _driver_schema(tmp: str, xsd_dir: str, part_schema: str, xml_xsd: str) -> str:
    """Write a tiny driver XSD that imports the W3C xml.xsd + the real part schema.

    Both imports land in one xmllint schema set, so xml:space/xml:lang resolve
    while the official ECMA schema stays byte-for-byte untouched. targetNamespace
    of each part schema is read so the import carries the right namespace."""
    import os
    from lxml import etree

    XSD = "http://www.w3.org/2001/XMLSchema"
    part_path = os.path.join(xsd_dir, part_schema)
    tns = etree.parse(part_path).getroot().get("targetNamespace", "")
    driver = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        f'<xsd:schema xmlns:xsd="{XSD}">\n'
        f'  <xsd:import namespace="{_W3C_XML_XSD_NS}" schemaLocation="{_uri(xml_xsd)}"/>\n'
        f'  <xsd:import namespace="{tns}" schemaLocation="{_uri(part_path)}"/>\n'
        "</xsd:schema>\n"
    )
    drv = os.path.join(tmp, f"_driver-{part_schema}")
    with open(drv, "w", encoding="utf-8") as fh:
        fh.write(driver)
    return drv


def _uri(p: str) -> str:
    from pathlib import Path
    return Path(p).resolve().as_uri()


def _xsd_check(path: str, xsd_dir: str) -> dict:
    """Validate every schema-mapped OOXML part against the ECMA-376 XSD set.

    ok=True only if ALL mapped parts validate; False on any schema violation;
    None (indeterminate) if xmllint is missing or xml.xsd cannot be provided."""
    import os
    import tempfile

    xmllint = C.find_tool("xmllint")
    if not xmllint:
        return C.check("validation XSD ECMA-376", None, "xmllint absent — " + C.SETUP_HINT)
    if not os.path.isdir(xsd_dir):
        return C.check("validation XSD ECMA-376", None,
                       f"dossier de schémas absent : {xsd_dir} (voir README pour vendorer)")

    xml_xsd = _ensure_xml_xsd(xsd_dir)
    if not xml_xsd:
        return C.check("validation XSD ECMA-376", None,
                       "xml.xsd (W3C) introuvable et non récupérable — requis pour résoudre "
                       "xml:space/xml:lang des schémas ECMA (offline ?). Voir README.")

    violations: list[dict] = []
    validated = skipped = 0
    with tempfile.TemporaryDirectory() as tmp:
        driver_cache: dict[str, str] = {}
        with zipfile.ZipFile(path) as z:
            for name in sorted(z.namelist()):
                if not name.endswith(".xml"):
                    continue
                schema = _schema_for_part(name)
                if not schema:
                    skipped += 1
                    continue
                if not os.path.isfile(os.path.join(xsd_dir, schema)):
                    violations.append({"part": name, "schema": schema,
                                       "message": f"schéma {schema} absent du jeu vendoré"})
                    continue
                # extract the part to a temp file (xmllint needs a path)
                part_file = os.path.join(tmp, name.replace("/", "__"))
                with open(part_file, "wb") as fh:
                    fh.write(z.read(name))
                drv = driver_cache.get(schema)
                if drv is None:
                    drv = _driver_schema(tmp, xsd_dir, schema, xml_xsd)
                    driver_cache[schema] = drv
                r = C.run([xmllint, "--noout", "--schema", drv, part_file], timeout=120)
                validated += 1
                if r.returncode != 0:
                    msg = (r.stdout + r.stderr).strip().replace(part_file, name)
                    violations.append({"part": name, "schema": schema, "message": msg[:600]})

    if validated == 0:
        return C.check("validation XSD ECMA-376", None,
                       f"aucune part mappable trouvée ({skipped} part(s) hors mapping) — "
                       "format inattendu ?")
    ok = not violations
    detail = f"{validated} part(s) validée(s), {len(violations)} violation(s), {skipped} part(s) OPC hors-XSD"
    if violations:
        detail += " | " + " ;; ".join(f"{v['part']} (vs {v['schema']}): {v['message'][:300]}"
                                      for v in violations[:6])
    return C.check("validation XSD ECMA-376 (Transitional)", ok, detail,
                   "ISO 29500-1 / ECMA-376 Part 4 (Transitional) — xmllint --schema par part",
                   violations=violations)


# --------------------------------------------------------------------------- #
# ODF RelaxNG validation (ISO 26300 / OASIS).
_ODF_RNG_PARTS = ("content.xml", "styles.xml", "meta.xml")


def _rng_check(path: str, rng: str) -> dict:
    """Validate each present ODF body part against the OASIS RelaxNG schema.

    ok=True only if ALL present parts validate; False on any violation; None if
    xmllint or the schema is missing."""
    import os
    import tempfile

    xmllint = C.find_tool("xmllint")
    if not xmllint:
        return C.check("validation RelaxNG ODF", None, "xmllint absent — " + C.SETUP_HINT)
    if not os.path.isfile(rng):
        return C.check("validation RelaxNG ODF", None, f"schéma RelaxNG absent : {rng}")

    violations: list[dict] = []
    validated = 0
    with tempfile.TemporaryDirectory() as tmp:
        with zipfile.ZipFile(path) as z:
            names = set(z.namelist())
            for part in _ODF_RNG_PARTS:
                if part not in names:
                    continue
                part_file = os.path.join(tmp, part)
                with open(part_file, "wb") as fh:
                    fh.write(z.read(part))
                r = C.run([xmllint, "--noout", "--relaxng", rng, part_file], timeout=120)
                validated += 1
                if r.returncode != 0:
                    msg = (r.stdout + r.stderr).strip().replace(part_file, part)
                    violations.append({"part": part, "schema": os.path.basename(rng),
                                       "message": msg[:600]})

    if validated == 0:
        return C.check("validation RelaxNG ODF", None,
                       "aucune part ODF (content/styles/meta) trouvée — format inattendu ?")
    ok = not violations
    detail = f"{validated} part(s) validée(s), {len(violations)} violation(s)"
    if violations:
        detail += " | " + " ;; ".join(f"{v['part']}: {v['message'][:300]}" for v in violations[:6])
    return C.check("validation RelaxNG ODF", ok, detail,
                   "ISO 26300 / OASIS ODF — xmllint --relaxng (content/styles/meta)",
                   violations=violations)


# --------------------------------------------------------------------------- #
def _apply_known_issues(file: str, checks: list[dict], ki_path: str) -> list[dict]:
    """Waive documented, pre-existing schema violations from a baseline file.

    A violation is waived ONLY if its `part` matches a baselined entry for this
    fixture AND its `message` contains the entry's `signature` substring. Any
    other violation (new part, new signature) still fails. A waived check is
    flipped ok=True but the waived items are recorded under `known_issues` so the
    report stays honest. New, schema-clean fixtures with stale baseline entries
    are reported as `stale_known_issues` (the entry can be dropped).

    Baseline schema (scripts/conformance/known-schema-issues.json):
      { "<fixture-basename>": [ {"part": "...", "signature": "...",
                                 "issue": "#NN", "note": "..."} , ... ] }
    """
    import json
    import os

    try:
        with open(ki_path, encoding="utf-8") as fh:
            baseline = json.load(fh)
    except (OSError, ValueError) as e:  # noqa: BLE001
        # A malformed/absent baseline must NOT silently weaken the gate.
        return [{"check": "known-issues baseline", "ok": False,
                 "detail": f"illisible: {ki_path} ({e})", "spec": ""}]

    key = os.path.basename(file)
    entries = baseline.get(key, [])
    if not entries:
        return checks

    matched_idx: set[int] = set()
    waived_all: list[dict] = []
    for c in checks:
        vio = c.get("violations")
        if not vio:
            continue
        remaining, waived = [], []
        for v in vio:
            hit = next((i for i, e in enumerate(entries)
                        if e.get("part") == v.get("part")
                        and e.get("signature", "") in v.get("message", "")), None)
            if hit is None:
                remaining.append(v)
            else:
                matched_idx.add(hit)
                waived.append({**v, "waived_by": entries[hit].get("issue", "?"),
                               "note": entries[hit].get("note", "")})
        c["violations"] = remaining
        if waived:
            waived_all.extend(waived)
            # Flip to ok only if every violation was waived; else stays False.
            if not remaining and c["ok"] is False:
                c["ok"] = True
                c["detail"] += f" | {len(waived)} violation(s) attendue(s) (known-issues) waivée(s)"

    extra: list[dict] = []
    if waived_all:
        extra.append({"check": "known-issues waivées", "ok": True,
                      "detail": f"{len(waived_all)} violation(s) pré-existante(s) tolérée(s) — "
                                "voir known-schema-issues.json (suivi d'un follow-up)",
                      "spec": "", "violations": waived_all})
    stale = [e for i, e in enumerate(entries) if i not in matched_idx]
    if stale:
        # Stale entries = the engine now emits a clean part. Surface (non-fatal so a
        # fix doesn't redden CI), prompting baseline cleanup in the follow-up.
        extra.append({"check": "known-issues périmées", "ok": None,
                      "detail": f"{len(stale)} entrée(s) ne correspondent plus (part corrigée ?) — "
                                f"à retirer de la baseline: {stale}", "spec": ""})
    return checks + extra


def main() -> int:
    ap = argparse.ArgumentParser(description="Validation de conformité PDF / OOXML / ODF")
    ap.add_argument("file")
    ap.add_argument("--pdfa", help="profil veraPDF (ex: 1b, 2b, 3b, 3u, ua1)")
    ap.add_argument("--xsd", help="dossier des schémas XSD ECMA-376 (OOXML)")
    ap.add_argument("--rng", help="schéma RelaxNG ODF (content.xml)")
    ap.add_argument("--known-issues", help="baseline JSON des violations de schéma pré-existantes "
                                           "à tolérer (waive précis part+signature)")
    args = ap.parse_args()

    fmt = C.detect_format(args.file)
    if fmt["family"] == "pdf":
        checks, tools = validate_pdf(args.file, args.pdfa)
    elif fmt["family"] == "ooxml":
        checks, tools = validate_ooxml(args.file, args.xsd)
    elif fmt["family"] == "odf":
        checks, tools = validate_odf(args.file, fmt, args.rng)
    else:
        C.fail(f"Format non supporté : {fmt}")

    if args.known_issues:
        checks = _apply_known_issues(args.file, checks, args.known_issues)

    decisive = [c["ok"] for c in checks if c["ok"] is not None]
    conformant = all(decisive) if decisive else None
    C.report(file=args.file, format=fmt, conformant=conformant, checks=checks, tool_versions=tools)


if __name__ == "__main__":
    sys.exit(main())
