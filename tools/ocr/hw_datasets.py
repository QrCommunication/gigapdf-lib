#!/usr/bin/env python3
"""Real handwriting line datasets for OCR training, via the HuggingFace
**datasets-server** REST API — no `datasets`/`pyarrow` dependency (only PIL + numpy +
urllib). Each dataset yields (image, transcription) line pairs that we normalise to the
**same strip convention as render_lines.py** (grayscale, height 32, ink = high on a 0
background, float32 in [0, 1]) so they drop straight into the CRNN+CTC trainer next to
the synthetic printed/handwriting-font samples.

Why the REST API: line-level handwriting corpora (IAM, RIMES, …) are the gold standard
but the canonical distributions are gated. These Teklia mirrors are ungated and the
datasets-server streams rows as JSON {image:{src:url}, text:str}, so we page + fetch the
JPEGs directly. See docs/OCR_TRAINING_DATA.md.

CLI:  python3 tools/ocr/hw_datasets.py <dataset> <n> [split]   # prebuild the disk cache
      python3 tools/ocr/hw_datasets.py iam 5000
"""
from __future__ import annotations

import glob
import io
import json
import os
import re
import sys
import urllib.parse
import urllib.request

import numpy as np
from PIL import Image

STRIP_H = 32  # MUST match render_lines.STRIP_H and the Rust runtime (ocr_crnn.rs)
CACHE_ROOT = "/tmp/ocr_hw"
_ROWS_URL = "https://datasets-server.huggingface.co/rows"
_PAGE = 100  # datasets-server max rows per request

# Friendly aliases → HF dataset id + transcription field + target script `group`.
# `config` is auto-resolved from /splits when omitted. Only **ungated** line-level
# (image, text) mirrors are listed; gated corpora (official IAM/CASIA/KHATT, most
# Arabic/Indic handwriting) need an HF token (see _hf_token) — add them once available.
DATASETS: dict[str, dict] = {
    # Latin → `alpha` group (Latin-ext + Cyrillic + Greek share one model)
    "iam": {"id": "Teklia/IAM-line", "text": "text", "group": "alpha"},  # English
    "rimes": {"id": "Teklia/RIMES-2011-line", "text": "text", "group": "alpha"},  # French
    "norhand": {"id": "Teklia/NorHand-v3-line", "text": "text", "group": "alpha"},  # Norwegian
    "newseye": {"id": "Teklia/NewsEye-Austrian-line", "text": "text", "group": "alpha"},  # German
    "belfort": {"id": "Teklia/Belfort-line", "text": "text", "group": "alpha"},  # French
    "esposalles": {"id": "Teklia/Esposalles-line", "text": "text", "group": "alpha"},  # Catalan
    # Cyrillic → `alpha` (real-style handwriting; fonts cover Cyrillic poorly)
    "cyrillic": {"id": "deepcopy/synthetic-handwritten-cyrillic-180k", "text": "text", "group": "alpha"},
    # Chinese → `cjk` (CASIA-HWDB2 line mirror — gated upstream, open here)
    "casia": {"id": "Teklia/CASIA-HWDB2-line", "text": "text", "group": "cjk"},
}


def _hf_token() -> str | None:
    """HF access token from env or the standard CLI cache — unlocks gated datasets
    (IAM/CASIA/KHATT mirrors, many non-Latin handwriting corpora). Optional: ungated
    datasets work without it."""
    for env in ("HF_TOKEN", "HUGGINGFACE_TOKEN", "HUGGING_FACE_HUB_TOKEN"):
        if os.environ.get(env):
            return os.environ[env].strip()
    for path in (os.path.expanduser("~/.huggingface/token"),
                 os.path.expanduser("~/.cache/huggingface/token")):
        try:
            with open(path, encoding="utf-8") as f:
                tok = f.read().strip()
                if tok:
                    return tok
        except OSError:
            continue
    return None


def _fetch(url: str, timeout: int = 30) -> bytes:
    headers = {"User-Agent": "Mozilla/5.0"}
    tok = _hf_token()
    if tok and "huggingface.co" in url:
        headers["Authorization"] = f"Bearer {tok}"
    req = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read()


def _to_strip(jpg: bytes, target_h: int = STRIP_H):
    """JPEG bytes (dark text on light paper) → float32 [target_h × W] strip matching
    render_lines: ink **inverted to high** on a 0 background, cropped to ink, height
    normalised. None if the image is blank/too small."""
    try:
        im = Image.open(io.BytesIO(jpg)).convert("L")
    except Exception:
        return None
    inv = 255.0 - np.asarray(im, np.float32)  # ink (dark) → high, paper (light) → 0
    ys, xs = np.where(inv > 40)
    if len(xs) < 1:
        return None
    crop = inv[ys.min() : ys.max() + 1, xs.min() : xs.max() + 1]
    h, w = crop.shape
    nw = max(1, round(w * (target_h / h)))
    resized = Image.fromarray(crop.astype(np.uint8)).resize((nw, target_h), Image.BILINEAR)
    return np.asarray(resized, np.float32) / 255.0


def _save_cache(path: str, pairs: list[tuple]) -> None:
    """Pickle-free cache: strips quantised to uint8 (concatenated row-major) + their
    widths + the texts as JSON. Loadable with allow_pickle=False (no code execution)."""
    widths = np.array([s.shape[1] for s, _ in pairs], np.int32)
    pix = (
        np.concatenate([(s * 255.0).astype(np.uint8).reshape(-1) for s, _ in pairs])
        if pairs
        else np.zeros(0, np.uint8)
    )
    os.makedirs(os.path.dirname(path), exist_ok=True)
    np.savez_compressed(path, widths=widths, pix=pix, texts=json.dumps([t for _, t in pairs]))


def _load_cache(path: str) -> list[tuple]:
    z = np.load(path, allow_pickle=False)
    widths, pix = z["widths"], z["pix"]
    texts = json.loads(str(z["texts"]))
    out, off = [], 0
    for w, t in zip(widths.tolist(), texts):
        size = STRIP_H * int(w)
        strip = pix[off : off + size].astype(np.float32).reshape(STRIP_H, int(w)) / 255.0
        off += size
        out.append((strip, t))
    return out


def _best_cache(safe: str, split: str, n: int) -> str | None:
    """An existing cache file for (safe, split) reusable for `n` lines: the smallest
    cache holding ≥ n (caller truncates), else the largest available. Lets a `_4000`
    cache serve any n ≤ 4000 without re-downloading (filename n need not match exactly)."""
    pat = re.compile(rf"^{re.escape(safe)}_{re.escape(split)}_(\d+)\.npz$")
    sized = [
        (int(m.group(1)), p)
        for p in glob.glob(os.path.join(CACHE_ROOT, f"{safe}_{split}_*.npz"))
        if (m := pat.match(os.path.basename(p))) and os.path.getsize(p) > 0
    ]
    if not sized:
        return None
    ge = sorted(c for c in sized if c[0] >= n)
    return ge[0][1] if ge else max(sized)[1]


def _resolve_config(dataset: str) -> str:
    """First config name from /splits (most datasets have one: 'default' or the name)."""
    try:
        q = urllib.parse.urlencode({"dataset": dataset})
        sp = json.loads(_fetch(f"https://datasets-server.huggingface.co/splits?{q}"))
        return sp["splits"][0]["config"]
    except Exception:
        return "default"


def fetch_lines(name: str, n: int, split: str = "train", *, cache: bool = True) -> list[tuple]:
    """Up to `n` (strip, transcription) pairs from a handwriting dataset. Cached to
    `CACHE_ROOT/<name>_<split>_<n>.npz` (real download is slow: one JPEG HTTP GET per
    line). `name` is a key of DATASETS or a raw "owner/dataset" id."""
    spec = DATASETS.get(name, {"id": name, "text": "text"})
    safe = name.replace("/", "_")
    cache_path = os.path.join(CACHE_ROOT, f"{safe}_{split}_{n}.npz")
    if cache:
        best = _best_cache(safe, split, n)
        if best is not None:
            return _load_cache(best)[:n]  # reuse any cache with ≥ n lines (or the largest)

    config = spec.get("config") or _resolve_config(spec["id"])
    out: list[tuple] = []
    offset = 0
    while len(out) < n:
        q = urllib.parse.urlencode(
            {"dataset": spec["id"], "config": config,
             "split": split, "offset": offset, "length": _PAGE}
        )
        try:
            page = json.loads(_fetch(f"{_ROWS_URL}?{q}"))
        except Exception as e:
            print(f"  [{name}] page @{offset} failed: {e}", file=sys.stderr)
            break
        rows = page.get("rows", [])
        if not rows:
            break
        for r in rows:
            row = r["row"]
            img = row.get("image") or {}
            src = img.get("src") if isinstance(img, dict) else None
            text = (row.get(spec["text"]) or "").strip()
            if not src or not text:
                continue
            try:
                strip = _to_strip(_fetch(src))
            except Exception:
                continue
            if strip is not None and strip.shape[1] >= 8:
                out.append((strip, text))
                if len(out) >= n:
                    break
        offset += _PAGE
        if offset % 500 == 0:
            print(f"  [{name}] fetched {len(out)}/{n}", flush=True)

    if cache and out:
        _save_cache(cache_path, out)
    return out


def main(argv: list[str]) -> int:
    if len(argv) < 3:
        print(f"usage: {argv[0]} <{'|'.join(DATASETS)}|owner/dataset> <n> [split]", file=sys.stderr)
        return 2
    name, n = argv[1], int(argv[2])
    split = argv[3] if len(argv) > 3 else "train"
    print(f"fetching {n} '{name}' [{split}] handwriting lines → {CACHE_ROOT}")
    pairs = fetch_lines(name, n, split)
    print(f"done: {len(pairs)} (strip, text) pairs cached")
    if pairs:
        ws = [p[0].shape[1] for p in pairs]
        print(f"  strip widths: min={min(ws)} med={int(np.median(ws))} max={max(ws)}")
        print(f"  sample text: {pairs[0][1][:60]!r}")
    return 0 if pairs else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
