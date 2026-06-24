"""Socle commun aux scripts du skill document-format-conformance.

Responsabilités :
  1. Auto-détection + ré-exécution dans le .venv du skill (deps python lourdes).
  2. Découverte des validateurs CLI externes (qpdf, xmllint, verapdf).
  3. Détection robuste du format d'un fichier (PDF / OOXML / ODF) par magic bytes.
  4. Helpers de sortie JSON normalisée pour tous les rapports de conformité.

Aucun script ne réimplémente un parseur : ce module localise les vrais outils
installés par scripts/setup.sh et signale clairement quoi lancer s'il en manque.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import zipfile
from pathlib import Path

SKILL_ROOT = Path(__file__).resolve().parent.parent
VENV_DIR = SKILL_ROOT / ".venv"
VENV_PYTHON = VENV_DIR / "bin" / "python"
SETUP_HINT = f"Lancer le bootstrap : bash {SKILL_ROOT / 'scripts' / 'setup.sh'}"


# --------------------------------------------------------------------------- #
# 1. Ré-exécution dans le venv                                                 #
# --------------------------------------------------------------------------- #
def reexec_in_venv() -> None:
    """Si on ne tourne pas déjà dans le .venv du skill, s'y ré-exécute.

    Idempotent et protégé contre toute boucle infinie via un drapeau d'env.
    À appeler en TÊTE de chaque script qui importe pikepdf / facturx / docx...
    """
    if os.environ.get("DFC_VENV_REEXEC") == "1":
        return  # déjà ré-exécuté
    # Comparer sys.prefix (et NON l'exécutable résolu) : dans un venv, bin/python
    # est souvent un symlink vers /usr/bin/python3 — resolve() donnerait un faux
    # positif. sys.prefix pointe la racine du venv uniquement quand on y tourne.
    if Path(sys.prefix).resolve() == VENV_DIR.resolve():
        return  # on est déjà dans le bon venv
    if VENV_PYTHON.exists():
        os.environ["DFC_VENV_REEXEC"] = "1"
        os.execv(str(VENV_PYTHON), [str(VENV_PYTHON), *sys.argv])
    # Pas de venv : on continue avec le python courant (certaines deps manqueront).


def harden_path() -> None:
    """Retire le dossier scripts/ de sys.path après import de _common.

    Évite que des noms de fichiers du skill (ex: un futur 'json.py') masquent un
    module stdlib ou tiers lors d'imports faits par pikepdf / lxml / facturx.
    À appeler juste après reexec_in_venv() (une fois _common déjà importé)."""
    here = str(Path(__file__).resolve().parent)
    sys.path[:] = [p for p in sys.path if p and Path(p).resolve() != Path(here)]


# --------------------------------------------------------------------------- #
# 2. Découverte des validateurs CLI                                            #
# --------------------------------------------------------------------------- #
_EXTRA_BIN = [Path.home() / ".local" / "bin", Path.home() / ".local" / "share" / "verapdf"]


def find_tool(name: str) -> str | None:
    """Localise un exécutable (PATH + ~/.local/bin + install veraPDF)."""
    hit = shutil.which(name)
    if hit:
        return hit
    for d in _EXTRA_BIN:
        cand = d / name
        if cand.exists() and os.access(cand, os.X_OK):
            return str(cand)
    return None


def require_tool(name: str) -> str:
    path = find_tool(name)
    if not path:
        fail(f"Validateur '{name}' introuvable. {SETUP_HINT}")
    return path


def run(cmd: list[str], timeout: int = 180) -> subprocess.CompletedProcess:
    """Exécute un outil externe en capturant stdout/stderr (jamais de shell)."""
    return subprocess.run(
        cmd, capture_output=True, text=True, timeout=timeout, check=False
    )


# Signatures de corruption RÉELLE dans la sortie de `qpdf --check` (vs warnings bénins
# type /Size mismatch produits par un incremental update légitime).
_QPDF_DAMAGE = ("damaged", "reconstruct cross-reference", "xref not found",
                "can't find startxref", "unable to find")


def qpdf_verdict(returncode: int, output: str) -> tuple[bool, str]:
    """Interprète `qpdf --check` pour un verdict de conformité forte.

    returncode 2 = erreurs ; 3 = warnings ; 0 = clean.
    Un fichier réellement endommagé (xref reconstruit) est NON conforme même si
    qpdf parvient à le récupérer. Un warning bénin (/Size) reste conforme.
    Retourne (ok, résumé)."""
    low = output.lower()
    damaged = any(sig in low for sig in _QPDF_DAMAGE)
    if returncode == 2:
        return False, "erreurs structurelles"
    if damaged:
        return False, "fichier endommagé (xref reconstruit) — non byte-clean"
    if returncode == 3:
        return True, "conforme avec warnings bénins (non bloquants)"
    return True, "intègre (aucun warning)"


# --------------------------------------------------------------------------- #
# 3. Détection de format                                                       #
# --------------------------------------------------------------------------- #
OOXML_CT = {
    "word/document.xml": "docx",
    "xl/workbook.xml": "xlsx",
    "ppt/presentation.xml": "pptx",
}


def detect_format(path: str | Path) -> dict:
    """Retourne {'family': 'pdf'|'ooxml'|'odf'|'zip'|'unknown', 'subtype': ...}.

    PDF : magic %PDF-. OOXML/ODF : conteneur ZIP, discriminé par le mimetype
    (ODF) ou la présence des parts principales (OOXML).
    """
    p = Path(path)
    if not p.is_file():
        fail(f"Fichier introuvable : {p}")
    with p.open("rb") as fh:
        head = fh.read(8)
    if head[:5] == b"%PDF-":
        return {"family": "pdf", "subtype": "pdf", "version": head[5:8].decode("latin1", "ignore")}
    if head[:2] == b"PK":
        try:
            with zipfile.ZipFile(p) as z:
                names = set(z.namelist())
                if "mimetype" in names:
                    mt = z.read("mimetype").decode("ascii", "ignore").strip()
                    if mt.startswith("application/vnd.oasis.opendocument"):
                        return {"family": "odf", "subtype": mt.rsplit(".", 1)[-1], "mimetype": mt}
                for part, sub in OOXML_CT.items():
                    if part in names:
                        return {"family": "ooxml", "subtype": sub}
                if "[Content_Types].xml" in names:
                    return {"family": "ooxml", "subtype": "unknown-opc"}
                return {"family": "zip", "subtype": "zip"}
        except zipfile.BadZipFile:
            return {"family": "unknown", "subtype": "corrupt-zip"}
    return {"family": "unknown", "subtype": "unknown"}


# --------------------------------------------------------------------------- #
# 4. Sortie JSON normalisée                                                    #
# --------------------------------------------------------------------------- #
def report(
    *, file: str, format: dict, conformant: bool | None,
    checks: list[dict], tool_versions: dict | None = None, extra: dict | None = None,
) -> None:
    """Émet un rapport JSON normalisé sur stdout puis exit (0=conforme, 1=non, 2=indéterminé)."""
    out = {
        "file": file,
        "format": format,
        "conformant": conformant,
        "checks": checks,
        "tools": tool_versions or {},
    }
    if extra:
        out.update(extra)
    print(json.dumps(out, indent=2, ensure_ascii=False))
    sys.exit(0 if conformant else (1 if conformant is False else 2))


def check(name: str, ok: bool | None, detail: str = "", spec: str = "") -> dict:
    """Fabrique une entrée de check : ok=True/False/None (non testé)."""
    return {"check": name, "ok": ok, "detail": detail, "spec": spec}


def fail(msg: str, code: int = 2) -> "None":
    print(json.dumps({"error": msg}, ensure_ascii=False), file=sys.stderr)
    sys.exit(code)
