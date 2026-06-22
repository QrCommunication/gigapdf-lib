#!/usr/bin/env python3
"""Validate a reused legacy `.gpocr` model decodes text correctly (before engine integration).
Rebuilds the net + loads f32 weights, renders test lines with the ORIGINAL render_lines convention
(grayscale H32, ink=1), runs the net, CTC-greedy-decodes with blank = K (last). Prints ref vs pred.

Usage: PYTHONPATH=<legacy tools/ocr> validate_legacy_hw.py <in.gpocr> ["text1" "text2" ...]
"""
import sys
import numpy as np
import torch

import gpocr_to_rs as gp
import render_lines as rl
from convert_legacy_gpocr import Net


def main():
    d = gp.parse(open(sys.argv[1], "rb").read())
    alpha, hid, gru_in, k = d["alphabet"], d["gru_hid"], d["gru_in"], len(d["alphabet"])
    c1, c2 = d["convs"][0][1], d["convs"][1][1]
    net = Net(c1, c2, hid, k).eval()
    def t(arr, scale=1.0):
        return torch.tensor(arr, dtype=torch.float32) * scale  # int8(GPO1)×scale or f32(GPO2)×1.0
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
        net.fc.weight.copy_(t(d["fc"][1], d["fc"][0]).reshape(k + 1, 2 * hid))
        net.fc.bias.copy_(t(d["fc"][2]))

    texts = sys.argv[2:] or ["Bonjour le monde", "handwriting test 123", "The quick brown fox"]
    font = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
    for text in texts:
        res = rl.render_line(text, font, augment=False)
        if res is None:
            print(f"  ref: {text!r}\n  got: <render failed>\n")
            continue
        arr, _ = res
        x = torch.tensor(np.asarray(arr, np.float32)).unsqueeze(0).unsqueeze(0)  # [1,1,32,W]
        with torch.no_grad():
            logits = net(x)[0]  # [T, K+1]
        idxs = logits.argmax(1).tolist()
        blank, prev, out = k, k, []
        for i in idxs:
            if i != prev and i != blank:
                out.append(alpha[i] if i < k else "")
            prev = i
        print(f"  ref: {text!r}\n  got: {''.join(out)!r}\n")


if __name__ == "__main__":
    main()
