#!/usr/bin/env python3
"""Train a Hebrew text-line recognizer (CRNN + CTC) and export it to ONNX so it runs on the
RTen pipeline alongside the PaddleOCR models. PaddleOCR/EasyOCR/MMOCR ship no Hebrew model, so
we produce our own — Hebrew is a small, non-stacking alphabet (same profile as Latin, where the
CRNN already beats Tesseract), so a compact CTC recognizer works well.

Charlist convention is aligned EXACTLY with `OcrEngine::load` (crates/ocr-rten/src/lib.rs):
    class 0      = CTC blank
    class 1..K   = CHARS[0..K]            (written, one per line, to <out>.dict.txt)
    class K+1    = space (the trailing ' ' OcrEngine appends)
So the model's output dim = len(CHARS) + 2 and the emitted dict = "\n".join(CHARS).

RTL: a CTC model emits glyphs in VISUAL left-to-right order. We train on visual-order labels
(logical text -> python-bidi get_display -> visual), so at inference the engine reverses the
Hebrew output back to logical order. Embedded digit/Latin runs are handled by the BiDi algorithm.

Usage: python train_hebrew.py --fonts ~/hebrew_fonts --out models/ocr_hebrew \
                              --nlines 60000 --epochs 30 --batch 256
"""
import argparse, os, random, math
import numpy as np
from PIL import Image, ImageDraw, ImageFont

try:
    from bidi.algorithm import get_display
except Exception:
    raise SystemExit("pip install python-bidi  (required for Hebrew RTL visual ordering)")

import torch
import torch.nn as nn
from torch.utils.data import Dataset, DataLoader

REC_H = 48          # must match OcrEngine REC_H
TRAIN_W = 320       # fixed training width (right-padded); inference width is dynamic
DOWNSCALE = 4       # conv width downsampling -> T = W / 4

# Hebrew block: 22 base letters + 5 final forms (separate Unicode points).
HEBREW = [chr(c) for c in range(0x05D0, 0x05EA + 1)]  # א..ת (includes finals ך ם ן ף ץ in range)
DIGITS = list("0123456789")
PUNCT = list(".,:;!?()[]{}\"'-/%&")
LATIN = list("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ")  # mixed he+latin is common
CHARS = HEBREW + DIGITS + PUNCT + LATIN          # NO blank, NO space (space is the last class)
BLANK = 0
SPACE_CLASS = len(CHARS) + 1                      # OcrEngine appends ' ' as the final class
NUM_CLASSES = len(CHARS) + 2
CH2IDX = {c: i + 1 for i, c in enumerate(CHARS)}  # +1: class 0 is blank

# Common Hebrew words for realistic line content (no niqqud — printed text rarely has it).
WORDS = ("של את על לא כל זה הוא היא אני אתה הם אנחנו עם גם רק כי אם אבל או אז כאן שם מה מי "
         "איך למה כמה יום שנה זמן ילד אישה איש בית עיר מים אור חיים עבודה ספר מילה שלום תודה "
         "בוקר ערב לילה גדול קטן טוב רע חדש ישן ראשון שני שלישי מאה אלף שקל רחוב מספר טלפון "
         "ישראל ירושלים תל אביב חיפה מדינה ממשלה חברה משפחה אהבה כסף שוק חנות מסעדה בית ספר").split()


def render_line(text, fonts):
    """Render a logical Hebrew string to a height-REC_H RGB image in VISUAL order; return (img, visual_text)."""
    visual = get_display(text)
    font = ImageFont.truetype(random.choice(fonts), random.randint(28, 40))
    bb = ImageDraw.Draw(Image.new("RGB", (8, 8))).textbbox((0, 0), visual, font=font)
    w, h = max(bb[2] - bb[0] + 12, 8), max(bb[3] - bb[1] + 12, 8)
    img = Image.new("RGB", (w, h), (255, 255, 255))
    ImageDraw.Draw(img).text((6 - bb[0], 6 - bb[1]), visual, font=font, fill=(0, 0, 0))
    # scale to height REC_H
    nw = max(1, round(REC_H * w / h))
    img = img.resize((nw, REC_H), Image.BILINEAR)
    # light scan-like augmentation
    if random.random() < 0.5:
        img = img.rotate(random.uniform(-2, 2), expand=False, fillcolor=(255, 255, 255))
    arr = np.asarray(img, dtype=np.float32)
    if random.random() < 0.5:
        arr += np.random.normal(0, random.uniform(3, 14), arr.shape)
    arr = np.clip(arr, 0, 255)
    return arr, visual


def sample_text():
    n = random.randint(1, 6)
    parts = []
    for _ in range(n):
        r = random.random()
        if r < 0.15:
            parts.append("".join(random.choice(DIGITS) for _ in range(random.randint(1, 4))))
        else:
            parts.append(random.choice(WORDS))
    return " ".join(parts)


class HebrewLines(Dataset):
    def __init__(self, n, fonts):
        self.n, self.fonts = n, fonts

    def __len__(self):
        return self.n

    def __getitem__(self, _):
        arr, visual = render_line(sample_text(), self.fonts)
        # to [3, REC_H, W], normalize (x/255 - 0.5)/0.5  (matches OcrEngine)
        t = torch.from_numpy((arr / 255.0 - 0.5) / 0.5).permute(2, 0, 1)
        label = [CH2IDX[c] if c in CH2IDX else SPACE_CLASS for c in visual if c == " " or c in CH2IDX]
        return t, torch.tensor(label, dtype=torch.long)


def collate(batch):
    maxw = max(x.shape[2] for x, _ in batch)
    maxw = max(maxw, TRAIN_W // 2)
    imgs = torch.full((len(batch), 3, REC_H, maxw), 1.0)  # white (normalized) padding on the right
    labels, lengths = [], []
    for i, (x, y) in enumerate(batch):
        imgs[i, :, :, : x.shape[2]] = x
        labels.append(y)
        lengths.append(len(y))
    return imgs, torch.cat(labels) if labels else torch.tensor([]), torch.tensor(lengths)


class CRNN(nn.Module):
    """Compact CRNN: conv backbone (W/4 downsample) -> collapse height -> BiLSTM -> CTC logits."""
    def __init__(self, n_cls):
        super().__init__()
        def block(i, o, pool):
            return nn.Sequential(nn.Conv2d(i, o, 3, 1, 1), nn.BatchNorm2d(o), nn.ReLU(True), nn.MaxPool2d(pool, pool))
        self.cnn = nn.Sequential(
            block(3, 64, (2, 2)),       # 48xW -> 24 x W/2
            block(64, 128, (2, 2)),     # -> 12 x W/4
            block(128, 256, (2, 1)),    # -> 6  x W/4   (height only)
            block(256, 256, (2, 1)),    # -> 3  x W/4
        )
        self.rnn = nn.LSTM(256, 256, num_layers=2, bidirectional=True, batch_first=True)
        self.fc = nn.Linear(512, n_cls)

    def forward(self, x):
        x = self.cnn(x)              # [B, 256, H', W']
        x = x.mean(dim=2)            # collapse height -> [B, 256, W']
        x = x.permute(0, 2, 1)       # [B, W', 256]
        x, _ = self.rnn(x)           # [B, W', 512]
        return self.fc(x)            # [B, T, C]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fonts", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--nlines", type=int, default=60000)
    ap.add_argument("--epochs", type=int, default=30)
    ap.add_argument("--batch", type=int, default=256)
    ap.add_argument("--workers", type=int, default=16)
    a = ap.parse_args()
    torch.manual_seed(7); random.seed(7); np.random.seed(7)

    fonts = [os.path.join(a.fonts, f) for f in os.listdir(a.fonts) if f.lower().endswith((".ttf", ".otf"))]
    assert fonts, "no fonts found"
    print(f"fonts={len(fonts)} classes={NUM_CLASSES} (blank=0, space={SPACE_CLASS})", flush=True)

    ds = HebrewLines(a.nlines, fonts)
    dl = DataLoader(ds, batch_size=a.batch, shuffle=True, num_workers=a.workers, collate_fn=collate, drop_last=True)
    model = CRNN(NUM_CLASSES)
    opt = torch.optim.AdamW(model.parameters(), lr=1e-3)
    sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, a.epochs)
    ctc = nn.CTCLoss(blank=BLANK, zero_infinity=True)

    for ep in range(a.epochs):
        model.train(); tot = 0.0; nb = 0
        for imgs, labels, lengths in dl:
            logits = model(imgs)                       # [B, T, C]
            logp = logits.log_softmax(2).permute(1, 0, 2)  # [T, B, C]
            T = logp.shape[0]
            in_len = torch.full((imgs.shape[0],), T, dtype=torch.long)
            loss = ctc(logp, labels, in_len, lengths)
            opt.zero_grad(); loss.backward()
            nn.utils.clip_grad_norm_(model.parameters(), 5.0)
            opt.step()
            tot += loss.item(); nb += 1
        sched.step()
        print(f"epoch {ep+1}/{a.epochs}  ctc_loss={tot/max(nb,1):.4f}", flush=True)

    os.makedirs(os.path.dirname(a.out) or ".", exist_ok=True)
    model.eval()
    dummy = torch.randn(1, 3, REC_H, TRAIN_W)
    onnx_path = a.out + ".onnx"
    torch.onnx.export(
        model, dummy, onnx_path, input_names=["x"], output_names=["logits"],
        dynamic_axes={"x": {0: "b", 3: "w"}, "logits": {0: "b", 1: "t"}}, opset_version=17,
        dynamo=False,  # legacy TorchScript exporter — no onnxscript dep, predictable graph for RTen
    )
    with open(a.out + ".dict.txt", "w", encoding="utf-8") as f:
        f.write("\n".join(CHARS))
    print(f"exported {onnx_path}  +  {a.out}.dict.txt  ({len(CHARS)} chars)", flush=True)


if __name__ == "__main__":
    main()
