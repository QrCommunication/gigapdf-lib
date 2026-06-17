#!/usr/bin/env python3
"""Fetch + sample text corpora per script group for synthetic line rendering.

Primary source: **Tesseract `langdata_lstm`** (`*.training_text`, Apache-2.0) — the
text Tesseract itself trains on, one file per language. Cached under /tmp. Lines are
filtered to the group's class set (chars outside the alphabet are dropped) so the
renderer never produces glyphs the model has no class for.

For larger/cleaner corpora (Leipzig, Wikipedia) drop sentence files into the cache
dir named `<lang>.extra.txt`; `load_lines` picks them up too. See
docs/OCR_TRAINING_DATA.md.

Run:  python3 tools/ocr/corpora.py <group> [n]      # print n sample lines
"""
from __future__ import annotations

import os
import random
import sys
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from scripts import SCRIPTS, alphabet_for  # noqa: E402

CACHE = "/tmp/ocr_corpora"
LANGDATA = "https://raw.githubusercontent.com/tesseract-ocr/langdata_lstm/main/{l}/{l}.training_text"


def fetch_langdata(lang: str, cache: str = CACHE) -> str | None:
    """Download (and cache) `<lang>.training_text`; return the local path or None."""
    os.makedirs(cache, exist_ok=True)
    dest = os.path.join(cache, f"{lang}.training_text")
    if os.path.exists(dest) and os.path.getsize(dest) > 0:
        return dest
    try:
        req = urllib.request.Request(
            LANGDATA.format(l=lang), headers={"User-Agent": "gigapdf-ocr/1.0"}
        )
        with urllib.request.urlopen(req, timeout=30) as r:
            data = r.read()
    except Exception as e:  # 404 for a missing lang, network error, …
        print(f"  (langdata {lang}: {e})", file=sys.stderr)
        return None
    with open(dest, "wb") as f:
        f.write(data)
    return dest


def load_lines(lang: str, cache: str = CACHE) -> list[str]:
    """All non-empty lines for a language (training_text + any `<lang>.extra.txt`)."""
    lines: list[str] = []
    paths = [fetch_langdata(lang, cache), os.path.join(cache, f"{lang}.extra.txt")]
    for p in paths:
        if not p or not os.path.exists(p):
            continue
        with open(p, encoding="utf-8", errors="ignore") as f:
            lines.extend(ln.strip() for ln in f if ln.strip())
    return lines


def filter_to_alphabet(line: str, allowed: set[str]) -> str:
    """Drop characters not in the class set (keep single spaces); collapse runs."""
    out = [ch if ch in allowed else " " for ch in line]
    return " ".join("".join(out).split())


def sample_lines(
    group: str,
    n: int,
    *,
    seed: int = 7,
    min_chars: int = 2,
    max_chars: int = 48,
    cache: str = CACHE,
) -> list[str]:
    """Sample `n` corpus lines for a group, filtered to its alphabet and trimmed to
    `max_chars`. Deterministic for a fixed `seed`."""
    allowed = set(alphabet_for(group)) | {" "}
    pool: list[str] = []
    for lang in SCRIPTS[group]["langs"]:
        for ln in load_lines(lang, cache):
            f = filter_to_alphabet(ln, allowed)
            if len(f) > max_chars:
                f = f[:max_chars].rsplit(" ", 1)[0] or f[:max_chars]
            if min_chars <= len(f) <= max_chars:
                pool.append(f)
    rng = random.Random(seed)
    rng.shuffle(pool)
    return pool[:n]


def main(argv: list[str]) -> int:
    if len(argv) < 2 or argv[1] not in SCRIPTS:
        print(f"usage: {argv[0]} <{'|'.join(SCRIPTS)}> [n]", file=sys.stderr)
        return 2
    group = argv[1]
    n = int(argv[2]) if len(argv) > 2 else 10
    lines = sample_lines(group, n)
    print(f"# {len(lines)} sample lines for '{group}'")
    for ln in lines:
        print(ln)
    return 0 if lines else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
