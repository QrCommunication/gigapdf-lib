#!/usr/bin/env python3
"""Train a CLEAN Latin/Cyrillic/Greek **handwriting** line recognizer (CRNN + CTC) and export it to
a DYNAMIC-WIDTH ONNX so it runs on the RTen pipeline like the PaddleOCR/Hebrew recognizers.

This replaces the *reused legacy* `ocr_alpha_hw.gpocr`: that model's custom GRU unrolled during ONNX
tracing → a FIXED-width graph, and the trailing padding needed at inference corrupted the backward
pass → accuracy capped below the float model. Here we use a **standard `nn.LSTM`** (exports to the
ONNX `LSTM` op with a dynamic sequence length, exactly like PaddleOCR's recognizers), so the engine
feeds each line at its NATURAL width — no padding, no washout.

Same data pipeline as the retired `tools/train_ocr_crnn.py` (the one that produced the IAM-0.309
model): synthetic printed+handwriting-font lines (corpora × fonts via render_lines) PLUS real
handwriting lines (IAM/RIMES/NorHand/… via hw_datasets, cached in /tmp/ocr_hw). Same strip
convention as render_lines / the engine's LegacyGray32 profile:
    grayscale, height 32, ink = 1 on a 0 background, float32 in [0, 1], 1 channel.
CTC charlist convention (engine `Profile::LegacyGray32`):
    class 0..K-1 = alphabet chars (one per line in <out>.dict.txt), class K = blank (last).

Run on the VPS (PYTHONPATH must include the legacy tools/ocr for scripts/corpora/fonts/render_lines/
hw_datasets):
    PYTHONPATH=~/gigapdf-lib/tools/ocr GIGA_OCR_HW_REAL="iam,rimes,belfort,esposalles,newseye,norhand,popp" \
    GIGA_OCR_HW_FRAC=0.45 ~/ocrvenv/bin/python train_handwriting.py --out models/ocr_handwriting \
        --nlines 40000 --epochs 30 --batch 192
"""
import argparse
import os
import random
import sys

import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F
from torch.utils.data import DataLoader, Dataset

# tools/ocr modules (scripts/corpora/fonts/render_lines/hw_datasets) come from PYTHONPATH.
import corpora
import fonts as fontmod
import hw_datasets
import render_lines as rl
from scripts import alphabet_for

H = 32           # strip height — MUST equal render_lines.STRIP_H + hw_datasets.STRIP_H + the engine
GROUP = "alpha"  # Latin-extended + Cyrillic + Greek share one model (see scripts.py)


class CRNN(nn.Module):
    """conv backbone (W/4 downsample, height collapsed) → bidirectional LSTM → CTC logits.
    Standard ops only (Conv2d/BatchNorm/MaxPool/LSTM/Linear) so the ONNX export is a clean,
    DYNAMIC-length graph that RTen runs at any width — unlike the legacy unrolled GRU."""

    def __init__(self, n_cls: int):
        super().__init__()

        def block(i, o, pool):
            return nn.Sequential(
                nn.Conv2d(i, o, 3, 1, 1), nn.BatchNorm2d(o), nn.ReLU(True), nn.MaxPool2d(pool, pool)
            )

        self.cnn = nn.Sequential(
            block(1, 64, (2, 2)),    # 32×W → 16 × W/2
            block(64, 128, (2, 2)),  # → 8 × W/4
        )
        self.head = nn.Sequential(nn.Conv2d(128, 256, 3, 1, 1), nn.BatchNorm2d(256), nn.ReLU(True))
        self.rnn = nn.LSTM(256, 256, num_layers=2, bidirectional=True, batch_first=True)
        self.fc = nn.Linear(512, n_cls)

    def forward(self, x):           # x: [B, 1, 32, W]
        x = self.head(self.cnn(x))  # [B, 256, 8, W/4]
        x = x.mean(dim=2)           # collapse height → [B, 256, W/4]
        x = x.permute(0, 2, 1)      # [B, T=W/4, 256]
        x, _ = self.rnn(x)          # [B, T, 512]
        return self.fc(x)           # [B, T, n_cls]


def build_samples(alphabet, n_lines, max_chars, hw_frac, hw_real, hw_real_n):
    """Reuse the proven train_ocr_crnn.py pipeline: synthetic lines (corpus × fonts, with a
    handwriting-font fraction) + real handwriting lines (cached HF mirrors). Returns
    [(strip[H,W] float32 ink, [class indices])]."""
    idx = {c: i for i, c in enumerate(alphabet)}
    rsel = random.Random(7)

    fonts = fontmod.system_fonts_for_group(GROUP, limit=int(os.environ.get("GIGA_OCR_FONTLIMIT", 60))) \
        or fontmod.fonts_for_group(GROUP)
    if not fonts:
        sys.exit("no fonts for 'alpha' — check fonts.py / network")
    hw_fonts = fontmod.local_handwriting_fonts(GROUP) if hw_frac > 0 else []
    lines = corpora.sample_lines(GROUP, n_lines, seed=7, max_chars=max_chars)
    print(f"  corpus lines={len(lines)} fonts={len(fonts)} hw_fonts={len(hw_fonts)} @ frac={hw_frac}",
          flush=True)

    def pick_font(text):
        if hw_fonts and rsel.random() < hw_frac:
            for _ in range(6):
                f = rsel.choice(hw_fonts)
                if fontmod.font_covers(f, text):
                    return f
        return rsel.choice(fonts)

    samples = []
    for text in lines:
        r = rl.render_line(text, pick_font(text), augment=True, rng=rsel)
        if r is None:
            continue
        arr, t = r
        tgt = [idx[c] for c in t if c in idx]
        if tgt and arr.shape[1] >= 8:
            samples.append((arr.astype(np.float32), tgt))
    print(f"  synthetic samples={len(samples)}", flush=True)

    if hw_real:
        added = 0
        for ds in (d.strip() for d in hw_real.split(",") if d.strip()):
            for arr, t in hw_datasets.fetch_lines(ds, hw_real_n):
                tgt = [idx[c] for c in t if c in idx]
                if tgt and arr.shape[1] >= 8:
                    samples.append((arr.astype(np.float32), tgt))
                    added += 1
        print(f"  + real handwriting samples={added} (from {hw_real})", flush=True)
    return samples


class LineDS(Dataset):
    def __init__(self, samples):
        self.samples = samples

    def __len__(self):
        return len(self.samples)

    def __getitem__(self, i):
        return self.samples[i]


def collate(batch):
    maxw = max(a.shape[1] for a, _ in batch)
    x = np.zeros((len(batch), 1, H, maxw), np.float32)
    widths, targets, tlens = [], [], []
    for i, (a, t) in enumerate(batch):
        x[i, 0, :, : a.shape[1]] = a
        widths.append(a.shape[1] // 4)  # T = W/4 (two stride-2 pools)
        targets.extend(t)
        tlens.append(len(t))
    return torch.from_numpy(x), torch.tensor(widths), torch.tensor(targets), torch.tensor(tlens)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--nlines", type=int, default=40000)
    ap.add_argument("--epochs", type=int, default=30)
    ap.add_argument("--batch", type=int, default=192)
    ap.add_argument("--workers", type=int, default=12)
    a = ap.parse_args()
    torch.manual_seed(7)
    random.seed(7)
    np.random.seed(7)

    alphabet = alphabet_for(GROUP)
    k = len(alphabet)
    blank = k  # CTC blank = last class (engine Profile::LegacyGray32)
    print(f"alphabet={k} chars  blank={blank}  classes={k + 1}", flush=True)

    samples = build_samples(
        alphabet,
        n_lines=int(os.environ.get("GIGA_OCR_NLINES", a.nlines)),
        max_chars=int(os.environ.get("GIGA_OCR_MAXCHARS", 48)),
        hw_frac=float(os.environ.get("GIGA_OCR_HW_FRAC", 0.45)),
        hw_real=os.environ.get("GIGA_OCR_HW_REAL", "").strip(),
        hw_real_n=int(os.environ.get("GIGA_OCR_HW_REAL_N", 8000)),
    )
    if not samples:
        sys.exit("no training samples")
    print(f"total samples={len(samples)}", flush=True)

    dl = DataLoader(LineDS(samples), batch_size=a.batch, shuffle=True, num_workers=a.workers,
                    collate_fn=collate, drop_last=True,
                    persistent_workers=a.workers > 0, prefetch_factor=4 if a.workers > 0 else None)
    model = CRNN(k + 1)
    opt = torch.optim.AdamW(model.parameters(), lr=1e-3)
    sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, a.epochs)
    ctc = nn.CTCLoss(blank=blank, zero_infinity=True)

    for ep in range(a.epochs):
        model.train()
        tot, nb = 0.0, 0
        for x, widths, targets, tlens in dl:
            logits = model(x)                              # [B, T, C]
            logp = logits.log_softmax(2).permute(1, 0, 2)  # [T, B, C]
            loss = ctc(logp, targets, widths.clamp(max=logp.shape[0]), tlens)
            opt.zero_grad()
            loss.backward()
            nn.utils.clip_grad_norm_(model.parameters(), 5.0)
            opt.step()
            tot += loss.item()
            nb += 1
        sched.step()
        print(f"epoch {ep + 1}/{a.epochs}  ctc_loss={tot / max(nb, 1):.4f}", flush=True)

    os.makedirs(os.path.dirname(a.out) or ".", exist_ok=True)
    model.eval()
    dummy = torch.randn(1, 1, H, 320)
    onnx_path = a.out + ".onnx"
    torch.onnx.export(
        model, dummy, onnx_path, input_names=["x"], output_names=["logits"],
        dynamic_axes={"x": {0: "b", 3: "w"}, "logits": {0: "b", 1: "t"}},
        opset_version=17, dynamo=False,  # legacy exporter: dynamic LSTM graph, no onnxscript dep
    )
    with open(a.out + ".dict.txt", "w", encoding="utf-8") as f:
        f.write("\n".join(alphabet))
    print(f"exported {onnx_path}  +  {a.out}.dict.txt  ({k} chars, blank={blank})", flush=True)


if __name__ == "__main__":
    main()
