#!/usr/bin/env python3
"""Benchmark gigapdf OCR vs Tesseract on a synthetic held-out test set (CER/WER).

Renders N labelled lines (corpus × fonts, a held-out seed) as dark-on-white PNGs, runs
both engines on the identical images, and reports micro-averaged CER/WER per engine —
the operational "Tesseract level" gap (docs/OCR_ARCHITECTURE.md §5).

gigapdf is invoked via the `ocr_image` example built with the group's `ocr-*` feature:
    cargo build --release -p gigapdf-core --features ocr-<group> --example ocr_image
Tesseract via its CLI (`--psm 7`, single line). Install language packs to match the
group (e.g. tesseract-ocr-{eng,fra,rus,ell} for alpha).

Run: /tmp/ocrvenv/bin/python tools/ocr/bench.py <group> [n] [--lang=eng+fra+rus+ell]
"""
from __future__ import annotations

import os
import random
import subprocess
import sys
import tempfile

from PIL import Image

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import corpora
import eval as ev
import fonts as fontmod
import render_lines as rl
from scripts import SCRIPTS

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
GIGA_BIN = os.path.join(ROOT, "target", "release", "examples", "ocr_image")
UPSCALE = 3  # enlarge lines so both segmenters have comfortable glyph sizes


def render_test_set(group: str, n: int, seed: int = 999):
    fonts = fontmod.system_fonts_for_group(group, limit=20, seed=seed) or fontmod.fonts_for_group(group)
    if not fonts:
        sys.exit(f"no fonts for '{group}'")
    lines = corpora.sample_lines(group, n, seed=seed, max_chars=24)
    rng = random.Random(seed)
    d = tempfile.mkdtemp(prefix=f"ocrbench_{group}_")
    out = []
    for i, text in enumerate(lines):
        r = rl.render_line(text, rng.choice(fonts), augment=False, rng=rng)
        if r is None:
            continue
        arr, t = r
        # ink=1 → dark text on a white page, then upscale.
        img = Image.fromarray(((1.0 - arr) * 255).astype("uint8"))
        img = img.resize((img.width * UPSCALE, img.height * UPSCALE), Image.LANCZOS)
        p = os.path.join(d, f"{i:04d}.png")
        img.save(p)
        out.append((p, t))
    return out, d


def giga_ocr(png: str) -> str:
    try:
        return subprocess.run([GIGA_BIN, png], capture_output=True, text=True, timeout=60).stdout.strip()
    except Exception:
        return ""


def main(argv: list[str]) -> int:
    group = argv[1] if len(argv) > 1 and not argv[1].startswith("-") else "alpha"
    if group not in SCRIPTS:
        print(f"usage: {argv[0]} <{'|'.join(SCRIPTS)}> [n] [--lang=…]", file=sys.stderr)
        return 2
    n = next((int(a) for a in argv[2:] if a.isdigit()), 100)
    lang = next((a.split("=", 1)[1] for a in argv if a.startswith("--lang=")), "eng+fra+rus+ell")
    model = next((a.split("=", 1)[1] for a in argv if a.startswith("--model=")), None)
    if model:  # host-loaded .gpocr path: the example loads it via $GIGAPDF_OCR_MODEL
        os.environ["GIGAPDF_OCR_MODEL"] = model
    if not os.path.exists(GIGA_BIN):
        sys.exit(f"build first: cargo build --release -p gigapdf-core --features ocr-{group} --example ocr_image")

    test, _ = render_test_set(group, n)
    print(f"test lines: {len(test)}  (group={group}, tesseract lang={lang})")
    giga = [(ref, giga_ocr(png)) for png, ref in test]
    tess = [(ref, ev.tesseract_text(png, lang=lang, psm=7) or "") for png, ref in test] if ev.tesseract_available() else []

    print("\n=== CER / WER — lower is better ===")
    ev.report(giga, "gigapdf")
    if tess:
        ev.report(tess, "tesseract")
    else:
        print("  tesseract: not installed (apt install tesseract-ocr + lang packs)")
    # A couple of qualitative samples.
    print("\n=== samples (ref | gigapdf) ===")
    for ref, hyp in giga[:5]:
        print(f"  {ref!r:32} | {hyp!r}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
