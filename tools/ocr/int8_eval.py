#!/usr/bin/env python3
"""Validate the **int8** `.gpocr` blob the trainer actually ships — not just the float net.

`train_ocr_crnn.py` reports `val_CER` on the **float** PyTorch model, but the runtime loads the
**int8-quantized** blob. Those can diverge badly (recurrent GRU rounding error compounds over a
line), and nothing caught it — shipping non-Latin models that decoded to garbage despite a great
float val. This is a torch-free numpy re-implementation of `ocr_crnn::recognize_line` driven by a
parsed `.gpocr`, so the trainer can print `int8_CER` next to `val_CER` and CI can gate on the gap.

Pure numpy; mirrors the Rust forward exactly (conv 3×3 pad1 + ReLU → 2×2 max-pool ×N → mean over
rows → bi-GRU with reset-before-recurrent-matmul → FC → CTC greedy).
"""
from __future__ import annotations

import numpy as np

import gpocr_to_rs as _g


def _conv_relu(x, in_ch, out_ch, scale, w, b, ih, iw):
    w = np.asarray(w, np.float32).reshape(out_ch, in_ch, 3, 3) * scale
    b = np.asarray(b, np.float32)
    xp = np.zeros((in_ch, ih + 2, iw + 2), np.float32)
    xp[:, 1:-1, 1:-1] = x
    out = np.empty((out_ch, ih, iw), np.float32)
    for oc in range(out_ch):
        acc = np.full((ih, iw), b[oc], np.float32)
        for ic in range(in_ch):
            for ky in range(3):
                for kx in range(3):
                    acc += w[oc, ic, ky, kx] * xp[ic, ky : ky + ih, kx : kx + iw]
        out[oc] = np.maximum(acc, 0.0)
    return out


def _maxpool2(x, ch, ih, iw):
    oh, ow = ih // 2, iw // 2
    if oh == 0 or ow == 0:
        return x[:, :0, :0], 0, 0
    return x[:, : oh * 2, : ow * 2].reshape(ch, oh, 2, ow, 2).max(axis=(2, 4)), oh, ow


def _sig(z):
    return 1.0 / (1.0 + np.exp(-np.clip(z, -40, 40)))


def _gru_dir(seq, gt, hid, reverse):
    w_scale, u_scale, mats, bv = gt
    wz, wr, wn, uz, ur, un = (np.asarray(m, np.float32).reshape(hid, -1) for m in mats)
    bz, br, bn = (np.asarray(v, np.float32) for v in bv)
    t_len = seq.shape[0]
    h = np.zeros(hid, np.float32)
    outs = [None] * t_len
    for i in range(t_len - 1, -1, -1) if reverse else range(t_len):
        x = seq[i]
        z = _sig(wz @ x * w_scale + uz @ h * u_scale + bz)
        r = _sig(wr @ x * w_scale + ur @ h * u_scale + br)
        n = np.tanh(wn @ x * w_scale + un @ (r * h) * u_scale + bn)
        h = (1.0 - z) * n + z * h
        outs[i] = h.copy()
    return np.asarray(outs, np.float32)


def forward(model: dict, strip: np.ndarray) -> str:
    """Run the parsed int8 model on a `STRIP_H×W` ink strip → decoded text (CTC greedy)."""
    alpha, hid = model["alphabet"], model["gru_hid"]
    k = len(alpha)
    x = strip[None].astype(np.float32)
    ih, iw = x.shape[1], x.shape[2]
    for in_ch, out_ch, scale, w, b in model["convs"]:
        x = _conv_relu(x, in_ch, out_ch, scale, w, b, ih, iw)
        x, ih, iw = _maxpool2(x, out_ch, ih, iw)
        if iw == 0:
            return ""
    seq = x.mean(axis=1).T  # (T, C2_OUT)
    ctx = np.concatenate([_gru_dir(seq, model["grus"][0], hid, False),
                          _gru_dir(seq, model["grus"][1], hid, True)], axis=1)
    fc_scale, fc_w, fc_b = model["fc"]
    w = np.asarray(fc_w, np.float32).reshape(k + 1, 2 * hid) * fc_scale
    logits = ctx @ w.T + np.asarray(fc_b, np.float32)
    prev, out = k, []
    rev = model["rtl"]
    for a in logits.argmax(1).tolist():
        if a != prev and a != k:
            out.append(alpha[a])
        prev = a
    s = "".join(out)
    return s[::-1] if rev else s


def int8_cer(gpocr_path: str, pairs) -> float:
    """`pairs`: iterable of (strip float32 STRIP_H×W, reference text). Returns micro CER of the
    int8 blob's decode vs reference (same Levenshtein metric as eval.corpus_cer)."""
    import eval as ev  # local import: torch-free

    model = _g.parse(open(gpocr_path, "rb").read())
    decoded = [(ref, forward(model, strip)) for strip, ref in pairs]
    return ev.corpus_cer(decoded)
