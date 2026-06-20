#!/usr/bin/env python3
"""Build a data-driven CJK character set from a text-recognition dataset's transcriptions.

The default `scripts.py` CJK group ships a tiny fallback (~150 chars) — useless for real
Chinese OCR. This samples transcriptions (TEXT ONLY, via the HF datasets-server `/rows`
endpoint — no image downloads, so it's fast) from a large Chinese corpus, counts character
frequency, and writes the top-K most frequent CJK characters plus the ASCII/punct that
co-occur. Wire the result into training with `GIGA_OCR_CJK_CHARSET=<path>` (see scripts.py).

Usage:  python3 tools/ocr/build_cjk_charset.py [n_samples] [top_k] [out]
        python3 tools/ocr/build_cjk_charset.py 20000 4000 tools/ocr/cjk_charset.txt
"""
from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import Counter

DATASET = os.environ.get("GIGA_OCR_CJK_SOURCE", "priyank-m/chinese_text_recognition")
TEXT_FIELD = os.environ.get("GIGA_OCR_CJK_TEXTFIELD", "")  # auto-detect if empty
ROWS_URL = "https://datasets-server.huggingface.co/rows"
PAGE = 100


def _tok() -> str | None:
    for e in ("HF_TOKEN", "HUGGINGFACE_TOKEN", "HUGGING_FACE_HUB_TOKEN"):
        if os.environ.get(e):
            return os.environ[e].strip()
    for p in (os.path.expanduser("~/.huggingface/token"), os.path.expanduser("~/.cache/huggingface/token")):
        try:
            t = open(p, encoding="utf-8").read().strip()
            if t:
                return t
        except OSError:
            pass
    return None


def _get(url: str, retries: int = 6):
    h = {"User-Agent": "Mozilla/5.0"}
    t = _tok()
    if t:
        h["Authorization"] = f"Bearer {t}"
    delay = 5.0
    for attempt in range(retries):
        try:
            return json.loads(urllib.request.urlopen(urllib.request.Request(url, headers=h), timeout=30).read())
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503, 504) and attempt < retries - 1:
                ra = e.headers.get("Retry-After") if e.headers else None
                time.sleep(min(float(ra) if (ra and ra.isdigit()) else delay, 120.0))
                delay = min(delay * 2, 120.0)
                continue
            raise


def _is_cjk(ch: str) -> bool:
    o = ord(ch)
    return (
        0x4E00 <= o <= 0x9FFF       # CJK Unified Ideographs
        or 0x3400 <= o <= 0x4DBF    # Ext A
        or 0x3040 <= o <= 0x30FF    # Hiragana + Katakana
        or 0xAC00 <= o <= 0xD7A3    # Hangul syllables
        or 0xF900 <= o <= 0xFAFF    # CJK compatibility ideographs
    )


def _config(dataset: str) -> str:
    try:
        sp = _get(f"https://datasets-server.huggingface.co/splits?dataset={urllib.parse.quote(dataset)}")
        return sp["splits"][0]["config"]
    except Exception:
        return "default"


def build(n_samples: int, top_k: int, out: str) -> int:
    cfg = _config(DATASET)
    # auto-detect the transcription field from the first row
    field = TEXT_FIELD
    freq: Counter = Counter()
    seen = 0
    offset = 0
    while seen < n_samples:
        q = urllib.parse.urlencode({"dataset": DATASET, "config": cfg, "split": "train",
                                    "offset": offset, "length": PAGE})
        try:
            page = _get(f"{ROWS_URL}?{q}")
        except Exception as e:
            print(f"  page @{offset} failed: {e}", file=sys.stderr)
            break
        rows = page.get("rows", [])
        if not rows:
            break
        if not field:  # detect a string field that isn't an image
            row0 = rows[0]["row"]
            for k, v in row0.items():
                if isinstance(v, str) and any(_is_cjk(c) for c in v):
                    field = k
                    break
            field = field or "text"
            print(f"  text field = '{field}'")
        for r in rows:
            txt = r["row"].get(field) or ""
            if isinstance(txt, str):
                freq.update(txt)
                seen += 1
        offset += PAGE
        if offset % 2000 == 0:
            print(f"  sampled {seen} transcriptions, {len(freq)} distinct chars", flush=True)

    # keep the top-K CJK chars by frequency, + the FULL printable ASCII (digits, Latin,
    # punctuation). Real CJK documents mix in alphanumerics (prices, dates, codes, URLs)
    # even when a synthetic corpus is pure-script — so bake the ASCII classes in regardless
    # of co-occurrence. NOTE: the model must also SEE these glyphs during training, so render
    # some Latin synthetic lines too (e.g. GIGA_OCR_LANGS includes 'eng'); classes without
    # training signal stay dead.
    cjk = [c for c, _ in freq.most_common() if _is_cjk(c)][:top_k]
    ascii_full = [chr(c) for c in range(0x21, 0x7F)]  # '!'..'~' (printable, space added by scripts.py)
    co_occur = sorted({c for c in freq if 0x20 <= ord(c) < 0x7F})  # anything else seen (full-width etc.)
    charset = "".join(dict.fromkeys(cjk + ascii_full + co_occur))  # de-dup, preserve order
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
    with open(out, "w", encoding="utf-8") as f:
        f.write(charset)
    print(f"wrote {out}: {len(charset)} classes "
          f"({len(cjk)} CJK + {len(ascii_punct)} ASCII/punct) from {seen} transcriptions")
    return 0 if charset else 1


def main(argv: list[str]) -> int:
    n = int(argv[1]) if len(argv) > 1 else 20000
    k = int(argv[2]) if len(argv) > 2 else 4000
    out = argv[3] if len(argv) > 3 else os.path.join(os.path.dirname(__file__), "cjk_charset.txt")
    print(f"building CJK charset from {DATASET} (sample {n}, top {k})")
    return build(n, k, out)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
