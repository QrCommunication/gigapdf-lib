#!/usr/bin/env python3
"""Render whole text LINES (corpus × fonts) → (strip, transcription) pairs.

This is the data pivot for the line-level CRNN+CTC recognizer: instead of rendering
isolated glyphs (tools/train_ocr.py), it renders a full string with a font onto a
variable-width strip of fixed height (`STRIP_H`, **must equal** the Rust
`ocr_crnn::STRIP_H`), normalized to ink=1, with optional scan-like augmentation.

RTL note: correct Arabic/Hebrew shaping needs PIL built with libraqm
(`PIL.features.check('raqm')`); without it Arabic renders unshaped. See
docs/OCR_TRAINING_DATA.md.

Run:  python3 tools/ocr/render_lines.py <group> [n] [font.ttf]
"""
from __future__ import annotations

import os
import random
import sys

import numpy as np
from PIL import Image, ImageDraw, ImageFilter, ImageFont

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import corpora  # noqa: E402
import fonts as fontmod  # noqa: E402

STRIP_H = 32  # keep in sync with crates/core/src/raster/ocr_crnn.rs::STRIP_H
FONT_PX = 40
PAD = 4


def _augment(a: np.ndarray, rng: random.Random) -> np.ndarray:
    """Light scan-like degradations on a float32 ink image (ink≈1, bg≈0)."""
    img = Image.fromarray((np.clip(a, 0, 1) * 255).astype(np.uint8))
    if rng.random() < 0.5:
        img = img.filter(ImageFilter.GaussianBlur(rng.uniform(0.3, 1.1)))
    out = np.asarray(img, np.float32) / 255.0
    if rng.random() < 0.6:  # additive sensor noise
        out = out + np.asarray(
            [[rng.gauss(0, 0.06) for _ in range(out.shape[1])] for _ in range(out.shape[0])],
            np.float32,
        )
    return np.clip(out, 0.0, 1.0)


def render_line(
    text: str,
    font_path: str,
    *,
    target_h: int = STRIP_H,
    font_px: int = FONT_PX,
    augment: bool = False,
    rng: random.Random | None = None,
) -> tuple[np.ndarray, str] | None:
    """Render `text` in a font → (float32 `target_h × W` ink image, text). None if
    the font can't render it or the result is empty."""
    try:
        font = ImageFont.truetype(font_path, font_px)
    except Exception:
        return None
    probe = ImageDraw.Draw(Image.new("L", (8, 8), 0))
    try:
        bbox = probe.textbbox((0, 0), text, font=font)
    except Exception:
        return None
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    if tw <= 0 or th <= 0:
        return None
    canvas = Image.new("L", (tw + 2 * PAD, th + 2 * PAD), 0)
    ImageDraw.Draw(canvas).text((PAD - bbox[0], PAD - bbox[1]), text, font=font, fill=255)
    arr = np.asarray(canvas)
    ys, xs = np.where(arr > 40)
    if len(xs) < 1:
        return None
    crop = canvas.crop((int(xs.min()), int(ys.min()), int(xs.max()) + 1, int(ys.max()) + 1))
    w, h = crop.size
    nw = max(1, round(w * (target_h / h)))
    crop = crop.resize((nw, target_h), Image.BILINEAR)
    a = np.asarray(crop, np.float32) / 255.0
    if augment and rng is not None:
        a = _augment(a, rng)
    return a, text


def dataset_iter(
    group: str,
    n: int,
    font_paths: list[str],
    *,
    seed: int = 7,
    augment: bool = True,
):
    """Yield up to `n` (strip, transcription) pairs for a script group."""
    if not font_paths:
        raise ValueError("no fonts — run fonts.fonts_for_group(group) first")
    rng = random.Random(seed)
    lines = corpora.sample_lines(group, n, seed=seed)
    for text in lines:
        fp = rng.choice(font_paths)
        r = render_line(text, fp, augment=augment, rng=rng)
        if r is not None:
            yield r


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(f"usage: {argv[0]} <group> [n] [font.ttf]", file=sys.stderr)
        return 2
    group, n = argv[1], int(argv[2]) if len(argv) > 2 else 6
    font_paths = [argv[3]] if len(argv) > 3 else fontmod.fonts_for_group(group)
    out_dir = f"/tmp/ocr_render/{group}"
    os.makedirs(out_dir, exist_ok=True)
    count = 0
    for i, (arr, text) in enumerate(dataset_iter(group, n, font_paths, augment=True)):
        assert arr.shape[0] == STRIP_H, arr.shape
        Image.fromarray((arr * 255).astype("uint8")).save(f"{out_dir}/{i:03d}.png")
        print(f"  {arr.shape} '{text[:40]}'")
        count += 1
    print(f"rendered {count} lines → {out_dir}")
    return 0 if count else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
