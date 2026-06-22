#!/usr/bin/env python3
"""Convert a legacy `.gpocr` (GPO2 full-precision f32) handwriting/print model to ONNX so it can run
on RTen alongside the PaddleOCR recognizers — REUSING the already-trained weights (no retraining).

The legacy CRNN (from the retired tools/train_ocr_crnn.py): input 1×32×W grayscale, ink=1 (dark
text → 1, background → 0); conv(1→C1)→maxpool2 → conv(C1→C2)→maxpool2 → mean over height → custom
bi-GRU (reset gate applied BEFORE the recurrent matmul) → fc → K+1 logits, **CTC blank = K (last)**.

The custom GRU's Python time-loop unrolls during ONNX tracing, so we export at a FIXED width
(default 800 → T=200); the engine pads grayscale strips to that width.

Usage: convert_legacy_gpocr.py <in.gpocr> <out.onnx> [width]   (also writes <out>.dict.txt)
Needs gpocr_to_rs.parse on PYTHONPATH (legacy tools/ocr/).
"""
import sys
import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F

import gpocr_to_rs as gp  # legacy parser (handles GPO1 int8 + GPO2 f32)


class Gru(nn.Module):
    """h' = (1−z)⊙n + z⊙h, n = tanh(Wn x + Un(r⊙h)) — reset BEFORE the recurrent matmul."""
    def __init__(self, inn, hid):
        super().__init__()
        self.hid = hid
        self.wz, self.wr, self.wn = (nn.Linear(inn, hid) for _ in range(3))
        self.uz, self.ur, self.un = (nn.Linear(hid, hid, bias=False) for _ in range(3))

    def step(self, x, h):
        z = torch.sigmoid(self.wz(x) + self.uz(h))
        r = torch.sigmoid(self.wr(x) + self.ur(h))
        n = torch.tanh(self.wn(x) + self.un(r * h))
        return (1.0 - z) * n + z * h

    def forward(self, seq, reverse=False):
        b, t, _ = seq.shape
        h = seq.new_zeros(b, self.hid)
        outs = [None] * t
        for i in range(t - 1, -1, -1) if reverse else range(t):
            h = self.step(seq[:, i, :], h)
            outs[i] = h
        return torch.stack(outs, dim=1)


class Net(nn.Module):
    def __init__(self, c1, c2, hid, k):
        super().__init__()
        self.c1 = nn.Conv2d(1, c1, 3, padding=1)
        self.c2 = nn.Conv2d(c1, c2, 3, padding=1)
        self.fwd, self.bwd = Gru(c2, hid), Gru(c2, hid)
        self.fc = nn.Linear(2 * hid, k + 1)

    def forward(self, x):
        x = F.max_pool2d(F.relu(self.c1(x)), 2)
        x = F.max_pool2d(F.relu(self.c2(x)), 2)
        seq = x.mean(dim=2).permute(0, 2, 1)
        ctx = torch.cat([self.fwd(seq), self.bwd(seq, reverse=True)], dim=2)
        return self.fc(ctx)


def main():
    blob = open(sys.argv[1], "rb").read()
    out = sys.argv[2]
    width = int(sys.argv[3]) if len(sys.argv) > 3 else 800
    d = gp.parse(blob)
    alpha, hid, gru_in = d["alphabet"], d["gru_hid"], d["gru_in"]
    k = len(alpha)
    c1_out = d["convs"][0][1]
    c2_out = d["convs"][1][1]
    assert gru_in == c2_out, f"gru_in {gru_in} != c2_out {c2_out}"
    print(f"alphabet={k} chars  c1={c1_out} c2={c2_out} hid={hid}  blank=K={k}  rtl={d['rtl']}", flush=True)

    net = Net(c1_out, c2_out, hid, k).eval()
    # Weights are int8 (GPO1) or f32 (GPO2); multiply by the per-layer scale to recover the true
    # float weight (GPO2 stores scale=1.0, so this is correct for both). Biases are always f32.
    def t(arr, scale=1.0):
        return torch.tensor(arr, dtype=torch.float32) * scale
    with torch.no_grad():
        for layer, (inn, o, s, w, b) in zip((net.c1, net.c2), d["convs"]):
            layer.weight.copy_(t(w, s).reshape(o, inn, 3, 3))
            layer.bias.copy_(t(b))
        for gru, (ws, us, mats, bvecs) in zip((net.fwd, net.bwd), d["grus"]):
            for lin, m, bv in zip((gru.wz, gru.wr, gru.wn), mats[:3], bvecs):
                lin.weight.copy_(t(m, ws).reshape(hid, gru_in))
                lin.bias.copy_(t(bv))
            for lin, m in zip((gru.uz, gru.ur, gru.un), mats[3:]):
                lin.weight.copy_(t(m, us).reshape(hid, hid))
        fc_scale, fc_w, fc_b = d["fc"]
        net.fc.weight.copy_(t(fc_w, fc_scale).reshape(k + 1, 2 * hid))
        net.fc.bias.copy_(t(fc_b))

    dummy = torch.zeros(1, 1, 32, width)
    torch.onnx.export(net, dummy, out, input_names=["x"], output_names=["logits"],
                      opset_version=17, dynamo=False)
    # dict.txt: alphabet chars one per line; the engine maps output idx → char, blank = K (last).
    with open(out.rsplit(".", 1)[0] + ".dict.txt", "w", encoding="utf-8") as f:
        f.write("\n".join(alpha))
    print(f"exported {out} (fixed W={width}, T={width // 4})  +  dict ({k} chars)", flush=True)


if __name__ == "__main__":
    main()
