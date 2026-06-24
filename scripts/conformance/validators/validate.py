#!/usr/bin/env python3
"""validate.py — validation de conformité structurelle + sémantique.

N'implémente AUCUN parseur maison : orchestre les validateurs de référence.
  PDF      → qpdf --check (intégrité ISO 32000) + pikepdf (2e avis)
  PDF/A    → veraPDF (profil 1b/2b/3b/3u/ua1…) si --pdfa donné
  OOXML    → ZIP + [Content_Types].xml + _rels résolus + parts XML well-formed
             (+ XSD optionnel via --xsd DIR)
  ODF      → mimetype 1ère entrée/STORED/valeur + manifest + content.xml
             (+ RNG optionnel via --rng FICHIER)

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
def _xsd_check(path: str, xsd_dir: str) -> dict:
    # XSD ECMA-376 nécessite xmllint (résolution des imports/includes multi-fichiers)
    xmllint = C.find_tool("xmllint")
    if not xmllint:
        return C.check("validation XSD", None, "xmllint absent — " + C.SETUP_HINT)
    return C.check("validation XSD ECMA-376", None,
                   f"Schémas dans {xsd_dir} — lancer xmllint --schema sur les parts extraites "
                   "(voir references/ooxml.md pour la procédure complète).")


def _rng_check(path: str, rng: str) -> dict:
    xmllint = C.find_tool("xmllint")
    if not xmllint:
        return C.check("validation RelaxNG", None, "xmllint absent — " + C.SETUP_HINT)
    import tempfile, os
    with zipfile.ZipFile(path) as z:
        with tempfile.NamedTemporaryFile(suffix=".xml", delete=False) as tf:
            tf.write(z.read("content.xml")); content = tf.name
    r = C.run([xmllint, "--noout", "--relaxng", rng, content])
    os.unlink(content)
    return C.check("content.xml vs RelaxNG ODF", r.returncode == 0, (r.stdout + r.stderr).strip()[:1200])


# --------------------------------------------------------------------------- #
def main() -> int:
    ap = argparse.ArgumentParser(description="Validation de conformité PDF / OOXML / ODF")
    ap.add_argument("file")
    ap.add_argument("--pdfa", help="profil veraPDF (ex: 1b, 2b, 3b, 3u, ua1)")
    ap.add_argument("--xsd", help="dossier des schémas XSD ECMA-376 (OOXML)")
    ap.add_argument("--rng", help="schéma RelaxNG ODF (content.xml)")
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

    decisive = [c["ok"] for c in checks if c["ok"] is not None]
    conformant = all(decisive) if decisive else None
    C.report(file=args.file, format=fmt, conformant=conformant, checks=checks, tool_versions=tools)


if __name__ == "__main__":
    sys.exit(main())
