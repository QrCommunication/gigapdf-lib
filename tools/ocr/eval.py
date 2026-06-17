#!/usr/bin/env python3
"""OCR evaluation — Character/Word Error Rate + an optional Tesseract baseline.

CER/WER are Levenshtein distance normalized by reference length, micro-averaged over
a corpus (sum of edits / sum of reference lengths) — the standard OCR metric and the
operational definition of "Tesseract level" (see docs/OCR_ARCHITECTURE.md §5).

`tesseract_text()` shells out to a locally-installed `tesseract` to produce the
baseline on the *same* images; if Tesseract isn't installed it returns None (install
`tesseract-ocr` + the per-language `tessdata` packs to enable the comparison).

Self-test:  python3 tools/ocr/eval.py
"""
from __future__ import annotations

import shutil
import subprocess
import sys


def _lev(a: list, b: list) -> int:
    """Levenshtein edit distance between two sequences (O(n·m) time, O(m) space)."""
    n, m = len(a), len(b)
    if n == 0:
        return m
    if m == 0:
        return n
    prev = list(range(m + 1))
    for i in range(1, n + 1):
        cur = [i] + [0] * m
        ai = a[i - 1]
        for j in range(1, m + 1):
            cost = 0 if ai == b[j - 1] else 1
            cur[j] = min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + cost)
        prev = cur
    return prev[m]


def cer(ref: str, hyp: str) -> float:
    """Character error rate for a single pair."""
    return _lev(list(ref), list(hyp)) / max(1, len(ref))


def wer(ref: str, hyp: str) -> float:
    """Word error rate for a single pair (whitespace-tokenized)."""
    rw = ref.split()
    return _lev(rw, hyp.split()) / max(1, len(rw))


def corpus_cer(pairs: list[tuple[str, str]]) -> float:
    """Micro-averaged CER over (ref, hyp) pairs."""
    edits = sum(_lev(list(r), list(h)) for r, h in pairs)
    total = sum(len(r) for r, _ in pairs)
    return edits / max(1, total)


def corpus_wer(pairs: list[tuple[str, str]]) -> float:
    """Micro-averaged WER over (ref, hyp) pairs."""
    edits = sum(_lev(r.split(), h.split()) for r, h in pairs)
    total = sum(len(r.split()) for r, _ in pairs)
    return edits / max(1, total)


def report(pairs: list[tuple[str, str]], label: str = "model") -> dict:
    """Print and return a {cer, wer, n} summary for a set of (ref, hyp) pairs."""
    res = {"cer": corpus_cer(pairs), "wer": corpus_wer(pairs), "n": len(pairs)}
    print(f"  {label:12s}  CER={res['cer']:.4f}  WER={res['wer']:.4f}  (n={res['n']})")
    return res


# ── Tesseract baseline ───────────────────────────────────────────────────────
def tesseract_available() -> bool:
    return shutil.which("tesseract") is not None


def tesseract_text(image_path: str, lang: str = "eng", psm: int | None = None) -> str | None:
    """OCR an image with the local Tesseract (`-l lang`, optional `--psm`); None if
    not installed. Use `psm=7` for single-line images."""
    if not tesseract_available():
        return None
    cmd = ["tesseract", image_path, "stdout", "-l", lang]
    if psm is not None:
        cmd += ["--psm", str(psm)]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=120, check=False)
        return out.stdout.strip()
    except Exception as e:  # pragma: no cover
        print(f"  (tesseract: {e})", file=sys.stderr)
        return None


# ── self-test ────────────────────────────────────────────────────────────────
def _selftest() -> int:
    assert _lev(list("hello"), list("hello")) == 0
    assert _lev(list("hello"), list("hallo")) == 1  # one substitution
    assert abs(cer("hello", "hallo") - 0.2) < 1e-9  # 1/5
    assert abs(cer("abc", "ab") - (1 / 3)) < 1e-9  # one deletion
    assert abs(wer("the cat sat", "the dog sat") - (1 / 3)) < 1e-9
    pairs = [("hello world", "hallo world"), ("foo bar", "foo bar")]
    c = corpus_cer(pairs)  # 1 edit / (11+7)=18
    assert abs(c - (1 / 18)) < 1e-9, c
    report(pairs, "selftest")
    print(f"tesseract available: {tesseract_available()}")
    print("eval.py self-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(_selftest())
