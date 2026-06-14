#!/usr/bin/env python3
"""Offline OCR classifier trainer — small CNN (build-time only).

The runtime stays zero-dependency: this script trains a compact convolutional
network in PyTorch on two public sources (EMNIST handwritten + synthetic glyphs
rendered from thousands of fonts), quantizes the weights to **int8**, and emits
them as a `crates/core/src/raster/ocr_model.rs` that the engine reads with a
pure-`std` forward pass (conv/maxpool/relu/fc) — no ML dependency ships.

Architecture (≈216K params → ~216 KB int8):
    input  28x28 (ink=1)
    conv1  1 -> 16, 3x3 pad1, ReLU        -> 16x28x28
    maxpool 2x2                           -> 16x14x14
    conv2  16 -> 32, 3x3 pad1, ReLU       -> 32x14x14
    maxpool 2x2                           -> 32x7x7
    flatten (C-major)                     -> 1568
    fc1    1568 -> 128, ReLU
    fc2    128 -> 81 (classes)

Run:  /tmp/ocrvenv/bin/python tools/train_ocr_cnn.py
"""
import os
import sys
import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from train_ocr import (  # noqa: E402  (reuse data pipeline)
    CHARS, N_CLASSES, SIZE, INK_BOX, load_emnist, usable_fonts, load_synthetic,
)

import torch  # noqa: E402
import torch.nn as nn  # noqa: E402
import torch.nn.functional as F  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CACHE = "/tmp/ocr_dataset.npz"
C1_OUT, C2_OUT = 16, 32
KERNEL = 3
FC1 = 128
FLAT = C2_OUT * (SIZE // 4) * (SIZE // 4)   # 32 * 7 * 7 = 1568
EPOCHS = 14
BATCH = 512
torch.manual_seed(7)


# ── dataset (cached to avoid the ~9-min font re-render) ──────────────────────
def build_dataset():
    if os.path.exists(CACHE):
        print(f"loading cached dataset {CACHE}")
        z = np.load(CACHE)
        return z["X"], z["Y"]
    print("building dataset (EMNIST + synthetic fonts)…")
    fonts = usable_fonts()
    print(f"  fonts={len(fonts)}")
    Xe, Ye = load_emnist()
    print(f"  emnist={len(Xe)}")
    Xs, Ys = load_synthetic(fonts)
    print(f"  synthetic={len(Xs)}")
    X = np.concatenate([Xe, Xs]) if len(Xe) else Xs
    Y = np.concatenate([Ye, Ys]) if len(Xe) else Ys
    Xu = np.clip(np.round(X * 255.0), 0, 255).astype(np.uint8)   # compact cache
    np.savez_compressed(CACHE, X=Xu, Y=Y.astype(np.int64))
    print(f"  cached {len(X)} samples → {CACHE}")
    return Xu, Y.astype(np.int64)


class Net(nn.Module):
    def __init__(self):
        super().__init__()
        self.c1 = nn.Conv2d(1, C1_OUT, KERNEL, padding=1)
        self.c2 = nn.Conv2d(C1_OUT, C2_OUT, KERNEL, padding=1)
        self.f1 = nn.Linear(FLAT, FC1)
        self.f2 = nn.Linear(FC1, N_CLASSES)

    def forward(self, x):
        x = F.max_pool2d(F.relu(self.c1(x)), 2)
        x = F.max_pool2d(F.relu(self.c2(x)), 2)
        x = x.flatten(1)                       # C-major: matches the Rust loop
        x = F.relu(self.f1(x))
        return self.f2(x)


def train(Xu, Y):
    n = len(Xu)
    rng = np.random.default_rng(7)
    perm = rng.permutation(n)
    Xu, Y = Xu[perm], Y[perm]
    nval = n // 12
    dev = torch.device("cpu")
    torch.set_num_threads(os.cpu_count() or 4)

    def to_t(a):
        return torch.from_numpy(
            (a.astype(np.float32) / 255.0).reshape(-1, 1, SIZE, SIZE)
        )

    Xv = to_t(Xu[:nval]); Yv = torch.from_numpy(Y[:nval])
    Xt = Xu[nval:]; Yt = Y[nval:]
    net = Net().to(dev)
    opt = torch.optim.Adam(net.parameters(), lr=1e-3)
    sched = torch.optim.lr_scheduler.StepLR(opt, step_size=5, gamma=0.5)
    lossf = nn.CrossEntropyLoss()
    best = 0.0
    nt = len(Xt)
    for ep in range(EPOCHS):
        net.train()
        idx = rng.permutation(nt)
        for i in range(0, nt, BATCH):
            b = idx[i:i + BATCH]
            xb = to_t(Xt[b]).to(dev)
            yb = torch.from_numpy(Yt[b]).to(dev)
            opt.zero_grad()
            loss = lossf(net(xb), yb)
            loss.backward()
            opt.step()
        sched.step()
        net.eval()
        with torch.no_grad():
            correct = 0
            for i in range(0, len(Xv), 4096):
                pr = net(Xv[i:i + 4096]).argmax(1)
                correct += (pr == Yv[i:i + 4096]).sum().item()
            acc = correct / len(Xv)
        best = max(best, acc)
        print(f"  epoch {ep+1:2d}/{EPOCHS}  val_acc={acc:.3f}", flush=True)
    return net, best


# ── int8 export ──────────────────────────────────────────────────────────────
def q_i8(name, t):
    a = t.detach().cpu().numpy().reshape(-1)
    scale = float(np.abs(a).max()) / 127.0 or 1.0
    q = np.clip(np.round(a / scale), -127, 127).astype(np.int8)
    body = ", ".join(str(int(v)) for v in q)
    rs = f"pub static {name}: [i8; {q.size}] = [{body}];\n"
    return rs, scale


def f32_vec(name, t):
    a = t.detach().cpu().numpy().reshape(-1)
    return f"pub static {name}: [f32; {a.size}] = [{', '.join(f'{v:.7}' for v in a)}];\n"


def emit_rust(net, acc):
    c1w, c1s = q_i8("C1_W", net.c1.weight)
    c2w, c2s = q_i8("C2_W", net.c2.weight)
    f1w, f1s = q_i8("F1_W", net.f1.weight)
    f2w, f2s = q_i8("F2_W", net.f2.weight)
    labels = CHARS.replace("\\", "\\\\").replace('"', '\\"')
    out = (
        "//! OCR classifier — a compact CNN trained OFFLINE (tools/train_ocr_cnn.py)\n"
        "//! on EMNIST (handwritten) + synthetic glyphs rendered from thousands of\n"
        "//! fonts (printed / punctuation / accented Latin). Weights are int8-\n"
        "//! quantized; the runtime forward pass is pure `std` (see ocr.rs). Re-run\n"
        "//! the trainer to improve precision without touching any runtime code.\n"
        f"//! Input {SIZE}x{SIZE} (ink=1), conv {C1_OUT}->{C2_OUT}, fc {FC1}, "
        f"{N_CLASSES} classes, val_acc {acc:.3f}.\n"
        "#![allow(clippy::excessive_precision, clippy::unreadable_literal)]\n\n"
        f"pub const SIZE: usize = {SIZE};\n"
        f"pub const INK_BOX: usize = {INK_BOX};\n"
        f"pub const CLASSES: usize = {N_CLASSES};\n"
        f"pub const C1_OUT: usize = {C1_OUT};\n"
        f"pub const C2_OUT: usize = {C2_OUT};\n"
        f"pub const KERNEL: usize = {KERNEL};\n"
        f"pub const FLAT: usize = {FLAT};\n"
        f"pub const FC1: usize = {FC1};\n"
        f"pub const C1_SCALE: f32 = {c1s:.8};\n"
        f"pub const C2_SCALE: f32 = {c2s:.8};\n"
        f"pub const F1_SCALE: f32 = {f1s:.8};\n"
        f"pub const F2_SCALE: f32 = {f2s:.8};\n"
        f'/// Class index -> character (UTF-8).\npub static LABELS: &str = "{labels}";\n\n'
        + c1w + f32_vec("C1_B", net.c1.bias)
        + c2w + f32_vec("C2_B", net.c2.bias)
        + f1w + f32_vec("F1_B", net.f1.bias)
        + f2w + f32_vec("F2_B", net.f2.bias)
    )
    dest = os.path.join(ROOT, "crates/core/src/raster/ocr_model.rs")
    open(dest, "w").write(out)
    print("wrote", dest, f"({os.path.getsize(dest)//1024} KB source)")


def main():
    print(f"classes={N_CLASSES} size={SIZE} flat={FLAT}")
    Xu, Y = build_dataset()
    print(f"total={len(Xu)}")
    net, acc = train(Xu, Y)
    emit_rust(net, acc)


if __name__ == "__main__":
    sys.exit(main())
