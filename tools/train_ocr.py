#!/usr/bin/env python3
"""Offline OCR classifier trainer (build-time only — the runtime stays zero-dep).

Two public, complementary sources, the way a real OCR is trained:
  * EMNIST byclass (NIST SD19, public domain) — handwritten 0-9 A-Z a-z.
  * Synthetic glyphs rendered from ~220 system fonts (Tesseract `text2image`
    style) — printed text, punctuation, and accented Latin (more languages).

Trains a small MLP (784 -> HIDDEN -> classes) in pure numpy and exports the
weights **int8-quantized** to `crates/core/src/raster/ocr_model.rs`. The shipped
engine embeds only the quantized weights + scales and runs a pure-`std` forward
pass — no ML dependency at runtime.

Run:  python3 tools/train_ocr.py
"""
import glob
import gzip
import os
import struct
import sys
import numpy as np
from PIL import Image, ImageDraw, ImageFont

# Class set: EMNIST's 62 alphanumeric first (so EMNIST labels map directly),
# then synthetic-only punctuation and accented Latin.
ALNUM = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
PUNCT = ".,;:!?'\"()-/&%@#$+=*"
ACCENT = "àâäçéèêëîïôöùûüñáíóúÀÂÇÉÈÊÔÜ"
CHARS = ALNUM + PUNCT + ACCENT
CHAR_INDEX = {c: i for i, c in enumerate(CHARS)}
N_CLASSES = len(CHARS)

SIZE = 28          # network input is SIZE x SIZE (EMNIST-native)
INK_BOX = 22       # glyph scaled to fit INK_BOX, centered in SIZE
MAX_FONTS = 6000   # use them all — system + downloaded Google Fonts
AUG_PER = 0        # font diversity (~4500 faces) is the augmentation
EMNIST_CAP = 4000  # max handwritten samples per class (byclass is unbalanced)
HIDDEN = 256
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EMNIST_DIR = "/tmp/emnist/gzip"
GFONTS_DIR = "/tmp/gfonts"


# ── EMNIST (handwritten) ────────────────────────────────────────────────────
def load_idx_images(path):
    with gzip.open(path, "rb") as f:
        _, n, r, c = struct.unpack(">IIII", f.read(16))
        data = np.frombuffer(f.read(), np.uint8).reshape(n, r, c)
    # EMNIST stores glyphs transposed; transpose each to upright.
    return np.transpose(data, (0, 2, 1))


def load_idx_labels(path):
    with gzip.open(path, "rb") as f:
        _, n = struct.unpack(">II", f.read(8))
        return np.frombuffer(f.read(), np.uint8)


def load_emnist():
    imgs_p = os.path.join(EMNIST_DIR, "emnist-byclass-train-images-idx3-ubyte.gz")
    lbls_p = os.path.join(EMNIST_DIR, "emnist-byclass-train-labels-idx1-ubyte.gz")
    map_p = os.path.join(EMNIST_DIR, "emnist-byclass-mapping.txt")
    if not os.path.exists(imgs_p):
        print("  (EMNIST not found — printed-only model)")
        return np.zeros((0, SIZE * SIZE), np.float32), np.zeros(0, np.int64)
    label_to_char = {}
    for line in open(map_p):
        idx, ascii_code = line.split()
        label_to_char[int(idx)] = chr(int(ascii_code))
    imgs = load_idx_images(imgs_p)        # (N,28,28)
    lbls = load_idx_labels(lbls_p)
    X, Y, per = [], [], {}
    for img, lb in zip(imgs, lbls):
        ch = label_to_char.get(int(lb))
        ci = CHAR_INDEX.get(ch)
        if ci is None:
            continue
        if per.get(ci, 0) >= EMNIST_CAP:
            continue
        per[ci] = per.get(ci, 0) + 1
        X.append(img.astype(np.float32).reshape(-1) / 255.0)
        Y.append(ci)
    return np.asarray(X, np.float32), np.asarray(Y, np.int64)


# ── synthetic (printed, from fonts) ─────────────────────────────────────────
def usable_fonts():
    # System fonts + the downloaded Google Fonts library.
    paths = sorted(glob.glob("/usr/share/fonts/**/*.ttf", recursive=True))
    paths += sorted(glob.glob(os.path.join(GFONTS_DIR, "*.ttf")))
    out = []
    for p in paths:
        try:
            f = ImageFont.truetype(p, 24)
            img = Image.new("L", (40, 40), 0)
            ImageDraw.Draw(img).text((4, 2), "Ag5", font=f, fill=255)
            if np.asarray(img).sum() > 200:  # renders basic Latin
                out.append(p)
        except Exception:
            pass
        if len(out) >= MAX_FONTS:
            break
    return out


def render_glyph(font, ch):
    canvas = Image.new("L", (64, 64), 0)
    ImageDraw.Draw(canvas).text((14, 6), ch, font=font, fill=255)
    arr = np.asarray(canvas)
    ys, xs = np.where(arr > 60)
    if len(xs) < 2:
        return None
    crop = canvas.crop((xs.min(), ys.min(), xs.max() + 1, ys.max() + 1))
    w, h = crop.size
    scale = INK_BOX / max(w, h)
    nw, nh = max(1, round(w * scale)), max(1, round(h * scale))
    crop = crop.resize((nw, nh), Image.BILINEAR)
    out = Image.new("L", (SIZE, SIZE), 0)
    out.paste(crop, ((SIZE - nw) // 2, (SIZE - nh) // 2))
    return out


def shift_vec(img, dx, dy):
    out = Image.new("L", (SIZE, SIZE), 0)
    out.paste(img, (dx, dy))
    return np.asarray(out, np.float32).reshape(-1) / 255.0


def load_synthetic(fonts):
    X, Y = [], []
    for fp in fonts:
        try:
            font = ImageFont.truetype(fp, 40)
        except Exception:
            continue
        for ch in CHARS:
            g = render_glyph(font, ch)
            if g is None:
                continue
            ci = CHAR_INDEX[ch]
            X.append(shift_vec(g, 0, 0)); Y.append(ci)
            for _ in range(AUG_PER):
                dx, dy = np.random.randint(-2, 3), np.random.randint(-2, 3)
                X.append(shift_vec(g, dx, dy)); Y.append(ci)
    return np.asarray(X, np.float32), np.asarray(Y, np.int64)


# ── pure-numpy MLP ──────────────────────────────────────────────────────────
def he(shape):
    return (np.random.randn(*shape) * np.sqrt(2.0 / shape[0])).astype(np.float32)


def softmax(z):
    z = z - z.max(1, keepdims=True)
    e = np.exp(z)
    return e / e.sum(1, keepdims=True)


def train(X, Y, epochs=30, bs=256, lr=0.1):
    n, d = X.shape
    rng = np.random.default_rng(7)
    perm = rng.permutation(n)
    X, Y = X[perm], Y[perm]
    nval = n // 12
    Xv, Yv, Xt, Yt = X[:nval], Y[:nval], X[nval:], Y[nval:]
    W1, b1 = he((d, HIDDEN)), np.zeros(HIDDEN, np.float32)
    W2, b2 = he((HIDDEN, N_CLASSES)), np.zeros(N_CLASSES, np.float32)
    vW1 = np.zeros_like(W1); vb1 = np.zeros_like(b1)
    vW2 = np.zeros_like(W2); vb2 = np.zeros_like(b2)
    mom, best = 0.9, 0.0
    for ep in range(epochs):
        idx = rng.permutation(len(Xt)); Xt, Yt = Xt[idx], Yt[idx]
        lr_ep = lr * (0.5 ** (ep // 10))
        for i in range(0, len(Xt), bs):
            xb, yb = Xt[i:i + bs], Yt[i:i + bs]
            h = np.maximum(0, xb @ W1 + b1)
            p = softmax(h @ W2 + b2)
            g = p.copy(); g[np.arange(len(yb)), yb] -= 1; g /= len(yb)
            gW2 = h.T @ g; gb2 = g.sum(0)
            gh = (g @ W2.T) * (h > 0)
            gW1 = xb.T @ gh; gb1 = gh.sum(0)
            vW2 = mom * vW2 - lr_ep * gW2; W2 += vW2
            vb2 = mom * vb2 - lr_ep * gb2; b2 += vb2
            vW1 = mom * vW1 - lr_ep * gW1; W1 += vW1
            vb1 = mom * vb1 - lr_ep * gb1; b1 += vb1
        hv = np.maximum(0, Xv @ W1 + b1)
        acc = (softmax(hv @ W2 + b2).argmax(1) == Yv).mean()
        best = max(best, acc)
        print(f"  epoch {ep+1:2d}/{epochs}  val_acc={acc:.3f}  lr={lr_ep:.4f}", flush=True)
    return W1, b1, W2, b2, best


# ── int8 export ─────────────────────────────────────────────────────────────
def quant(name, W):
    scale = float(np.abs(W).max()) / 127.0 or 1.0
    q = np.clip(np.round(W / scale), -127, 127).astype(np.int8)
    body = ", ".join(str(int(v)) for v in q.reshape(-1))
    rs = f"pub static {name}: [i8; {q.size}] = [{body}];\n"
    return rs, scale


def emit_rust(W1, b1, W2, b2, acc):
    w1, s1 = quant("W1Q", W1)
    w2, s2 = quant("W2Q", W2)

    def fvec(name, a):
        return f"pub static {name}: [f32; {a.size}] = [{', '.join(f'{v:.6}' for v in a)}];\n"

    labels = CHARS.replace("\\", "\\\\").replace('"', '\\"')
    out = (
        "//! OCR classifier — a small MLP trained OFFLINE (tools/train_ocr.py) on\n"
        "//! EMNIST (handwritten) + synthetic font glyphs (printed/punctuation/\n"
        "//! accented Latin). Weights are int8-quantized; the runtime forward pass\n"
        "//! is pure `std` (see ocr.rs). Re-run the trainer to improve without\n"
        "//! touching any runtime code.\n"
        f"//! Input {SIZE}x{SIZE} (ink=1), hidden {HIDDEN}, {N_CLASSES} classes, "
        f"val_acc {acc:.3f}.\n\n"
        f"pub const SIZE: usize = {SIZE};\n"
        f"pub const INK_BOX: usize = {INK_BOX};\n"
        f"pub const INPUT: usize = {SIZE * SIZE};\n"
        f"pub const HIDDEN: usize = {HIDDEN};\n"
        f"pub const CLASSES: usize = {N_CLASSES};\n"
        f"pub const W1_SCALE: f32 = {s1:.8};\n"
        f"pub const W2_SCALE: f32 = {s2:.8};\n"
        f'/// Class index -> character (UTF-8).\npub static LABELS: &str = "{labels}";\n\n'
        + w1 + fvec("B1", b1) + w2 + fvec("B2", b2)
    )
    dest = os.path.join(ROOT, "crates/core/src/raster/ocr_model.rs")
    open(dest, "w").write(out)
    print("wrote", dest, f"({os.path.getsize(dest)//1024} KB source)")


def main():
    print(f"classes={N_CLASSES} size={SIZE}")
    fonts = usable_fonts()
    print(f"fonts={len(fonts)}")
    Xe, Ye = load_emnist()
    print(f"emnist samples={len(Xe)}")
    Xs, Ys = load_synthetic(fonts)
    print(f"synthetic samples={len(Xs)}")
    X = np.concatenate([Xe, Xs]) if len(Xe) else Xs
    Y = np.concatenate([Ye, Ys]) if len(Xe) else Ys
    print(f"total={len(X)}")
    W1, b1, W2, b2, acc = train(X, Y)
    emit_rust(W1, b1, W2, b2, acc)


if __name__ == "__main__":
    sys.exit(main())
