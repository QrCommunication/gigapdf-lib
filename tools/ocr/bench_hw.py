#!/usr/bin/env python3
"""Benchmark OCR models on REAL handwriting test lines (held-out), comparing models
A/B (e.g. printed-only vs handwriting-augmented). Reuses the gigapdf `ocr_image` binary
(host-loads a `.gpocr` via `$GIGAPDF_OCR_MODEL`) and eval.py CER/WER; Tesseract optional.

Strips from hw_datasets are ink-high (text bright on 0); we invert them back to a normal
dark-on-light page so the `ocr()` pipeline (Sauvola etc.) sees a regular scan.

Usage:
  python3 tools/ocr/bench_hw.py iam 80 --split=test \
      --models=models/ocr_alpha_print.gpocr,models/ocr_alpha_hw.gpocr [--tesseract=eng]
"""
from __future__ import annotations

import os
import subprocess
import sys
import tempfile

import numpy as np
from PIL import Image

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import eval as ev  # noqa: E402
import hw_datasets  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
GIGA_BIN = os.path.join(ROOT, "target", "release", "examples", "ocr_image")


def _strip_to_png(strip: np.ndarray, path: str) -> None:
    """Ink-high float strip [0,1] → dark-text-on-white uint8 PNG (a normal scan)."""
    arr = (255.0 - strip * 255.0).clip(0, 255).astype(np.uint8)
    Image.fromarray(arr, "L").save(path)


def _gigapdf_text(png: str, model: str) -> str:
    os.environ["GIGAPDF_OCR_MODEL"] = model
    return subprocess.run([GIGA_BIN, png], capture_output=True, text=True, timeout=60).stdout.strip()


def main(argv: list[str]) -> int:
    pos = [a for a in argv[1:] if not a.startswith("-")]
    opt = {
        a.split("=", 1)[0].lstrip("-"): a.split("=", 1)[1]
        for a in argv[1:]
        if a.startswith("-") and "=" in a
    }
    if len(pos) < 2:
        print("usage: bench_hw.py <dataset> <n> [--split=test] [--models=a,b] [--tesseract=lang]", file=sys.stderr)
        return 2
    name, n = pos[0], int(pos[1])
    split = opt.get("split", "test")
    models = [m for m in opt.get("models", "models/ocr_alpha.gpocr").split(",") if m]
    tess = opt.get("tesseract")
    if not os.path.exists(GIGA_BIN):
        sys.exit("build first: cargo build --release -p gigapdf-core --example ocr_image")

    pairs = hw_datasets.fetch_lines(name, n, split=split)
    refs = [t for _, t in pairs]
    print(f"HW test lines: {len(pairs)}  (dataset={name}, split={split})")

    with tempfile.TemporaryDirectory() as td:
        pngs = []
        for i, (strip, _) in enumerate(pairs):
            p = os.path.join(td, f"{i:04d}.png")
            _strip_to_png(strip, p)
            pngs.append(p)
        print("\n=== CER / WER on real handwriting — lower is better ===")
        for model in models:
            cers, wers = [], []
            for p, r in zip(pngs, refs):
                hyp = _gigapdf_text(p, model)  # one OCR pass per image
                cers.append(ev.cer(r, hyp))
                wers.append(ev.wer(r, hyp))
            print(f"  gigapdf [{os.path.basename(model):26s}] CER={np.mean(cers):.4f} WER={np.mean(wers):.4f}")
        if tess and ev.tesseract_available():
            cers, wers = [], []
            for p, r in zip(pngs, refs):
                hyp = ev.tesseract_text(p, lang=tess, psm=7) or ""
                cers.append(ev.cer(r, hyp))
                wers.append(ev.wer(r, hyp))
            print(f"  tesseract [{tess:24s}] CER={np.mean(cers):.4f} WER={np.mean(wers):.4f}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
