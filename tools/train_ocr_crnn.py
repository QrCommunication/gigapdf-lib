#!/usr/bin/env python3
"""Offline CRNN + CTC line-recognizer trainer (build-time only — runtime stays zero-dep).

Trains, per **script group** (tools/ocr/scripts.py), a compact CRNN on synthetic text
LINES (corpus × fonts, tools/ocr/render_lines.py) and emits int8-quantized weights to
`crates/core/src/raster/ocr_model_<group>.rs`, which the engine reads with the pure-`std`
forward pass in `ocr_crnn.rs` — no ML dependency ships.

Architecture (must mirror ocr_crnn.rs exactly):
    input 1×H×W (H=32, ink=1)
    conv1 1->16 3x3 pad1 ReLU → maxpool2          → 16×16×(W/2)
    conv2 16->32 3x3 pad1 ReLU → maxpool2          → 32×8×(W/4)
    mean over the 8 rows                            → sequence T=W/4, dim 32
    bidirectional GRU (hidden HID each direction)   → T×(2·HID)
    linear 2·HID -> K+1                             → per-step logits (blank = K)
    CTC

CRITICAL — the GRU matches ocr_crnn.rs's formulation (reset gate applied to the hidden
state BEFORE the recurrent matmul), NOT torch.nn.GRU's (which applies it after). We use
a hand-written cell. Quantization mirrors train_ocr_cnn.py (symmetric int8, per-tensor
for conv/fc; one shared scale for a GRU direction's input weights and one for its
recurrent weights — matching GruSpec's `w_scale`/`u_scale`).

The torch-dependent half lives inside `build_and_train` so the module (and its Rust
emitter) import without PyTorch; only training needs it.

Run:  /tmp/ocrvenv/bin/python tools/train_ocr_crnn.py <group> [epochs]
      (needs: pip install torch ; fonts via tools/ocr/fonts.py ; corpora auto-fetched)
"""
from __future__ import annotations

import os
import sys

import numpy as np

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "ocr"))
import gpocr  # noqa: E402
from scripts import SCRIPTS, alphabet_for, is_rtl  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
H = 32                   # must equal ocr_crnn::STRIP_H and render_lines.STRIP_H
# Backbone size — env-overridable so a larger model can be trained for hard scripts
# (CJK, dense Indic). The .gpocr format and the Rust runtime read every conv/GRU
# dimension from the blob, so a bigger model host-loads with no runtime change.
C1_OUT = int(os.environ.get("GIGA_OCR_C1", 16))
C2_OUT = int(os.environ.get("GIGA_OCR_C2", 32))
HID = int(os.environ.get("GIGA_OCR_HID", 64))    # GRU hidden units per direction
N_LINES = 60_000         # synthetic lines sampled from the corpus
BATCH = 64


# ── int8 export (torch-free; mirrors ConvSpec/GruSpec/Crnn in ocr_crnn.rs) ─────
# These operate on anything exposing `.detach().cpu().numpy()` (a torch tensor, or a
# numpy shim) so the emitter can be unit-checked without PyTorch installed.
def _q(arr: np.ndarray, scale: float) -> str:
    q = np.clip(np.round(arr.reshape(-1) / scale), -127, 127).astype(np.int8)
    return ", ".join(str(int(v)) for v in q)


def _i8(name: str, t, scale: float | None = None):
    a = t.detach().cpu().numpy()
    scale = scale or (float(np.abs(a).max()) / 127.0 or 1.0)
    return f"pub static {name}: [i8; {a.size}] = [{_q(a, scale)}];\n", scale


def _f32(name: str, t) -> str:
    a = t.detach().cpu().numpy().reshape(-1)
    return f"pub static {name}: [f32; {a.size}] = [{', '.join(f'{v:.7}' for v in a)}];\n"


def _wmax(*mats) -> float:
    return max(float(np.abs(m.weight.detach().cpu().numpy()).max()) for m in mats) / 127.0 or 1.0


def _gru(prefix: str, g):
    """Emit one direction. Shared `w_scale` over Wz/Wr/Wn, shared `u_scale` over Uz/Ur/Un."""
    ws = _wmax(g.wz, g.wr, g.wn)
    us = _wmax(g.uz, g.ur, g.un)
    out = ""
    for nm, m in (("WZ", g.wz), ("WR", g.wr), ("WN", g.wn)):
        out += _i8(f"{prefix}_{nm}", m.weight, ws)[0]
    for nm, m in (("UZ", g.uz), ("UR", g.ur), ("UN", g.un)):
        out += _i8(f"{prefix}_{nm}", m.weight, us)[0]
    out += f"pub const {prefix}_W_SCALE: f32 = {ws:.8};\n"
    out += f"pub const {prefix}_U_SCALE: f32 = {us:.8};\n"
    out += _f32(f"{prefix}_BZ", g.wz.bias) + _f32(f"{prefix}_BR", g.wr.bias) + _f32(f"{prefix}_BN", g.wn.bias)
    return out


def _gru_ctor(prefix: str) -> str:
    return (
        f"GruSpec {{ wz: &{prefix}_WZ, wr: &{prefix}_WR, wn: &{prefix}_WN, "
        f"uz: &{prefix}_UZ, ur: &{prefix}_UR, un: &{prefix}_UN, "
        f"w_scale: {prefix}_W_SCALE, u_scale: {prefix}_U_SCALE, "
        f"bz: &{prefix}_BZ, br: &{prefix}_BR, bn: &{prefix}_BN }}"
    )


def _out_group(group: str) -> str:
    """Output-file group name: `<group>` normally, `<group>_<variant>` when GIGA_OCR_VARIANT
    is set (e.g. 'photo'). Lets a degraded/photo variant train without clobbering the primary
    `ocr_<group>` model — the alphabet/RTL/scripts logic keeps using the real `group`."""
    v = os.environ.get("GIGA_OCR_VARIANT", "").strip()
    return f"{group}_{v}" if v else group


def emit_rust(net, alphabet: str, group: str, cer: float, out_group: str | None = None):
    k = len(alphabet)
    c1w, c1s = _i8("C1_W", net.c1.weight)
    c2w, c2s = _i8("C2_W", net.c2.weight)
    fcw, fcs = _i8("FC_W", net.fc.weight)
    labels = alphabet.replace("\\", "\\\\").replace('"', '\\"')
    body = (
        f"//! AUTO-GENERATED by tools/train_ocr_crnn.py — CRNN+CTC line model, group '{group}'.\n"
        f"//! Trained on synthetic lines (corpus × fonts); int8-quantized. Inference is the\n"
        f"//! pure-`std` forward pass in ocr_crnn.rs. {k} classes (+1 CTC blank), val_CER {cer:.4f}.\n"
        "//! DO NOT EDIT — re-run the trainer to refresh.\n"
        "#![allow(clippy::excessive_precision, clippy::unreadable_literal)]\n\n"
        "use super::ocr_crnn::{ConvSpec, Crnn, GruSpec};\n\n"
        f"pub const H: usize = {H};\n"
        f"pub const GRU_IN: usize = {C2_OUT};\n"
        f"pub const GRU_HID: usize = {HID};\n"
        f"pub const RTL: bool = {str(is_rtl(group)).lower()};\n"
        f'pub static ALPHABET: &str = "{labels}";\n\n'
        + c1w + _f32("C1_B", net.c1.bias) + f"pub const C1_SCALE: f32 = {c1s:.8};\n"
        + c2w + _f32("C2_B", net.c2.bias) + f"pub const C2_SCALE: f32 = {c2s:.8};\n\n"
        + _gru("FWD", net.fwd) + "\n" + _gru("BWD", net.bwd) + "\n"
        + fcw + _f32("FC_B", net.fc.bias) + f"pub const FC_SCALE: f32 = {fcs:.8};\n\n"
        "static CONV: [ConvSpec; 2] = [\n"
        f"    ConvSpec {{ w: &C1_W, scale: C1_SCALE, b: &C1_B, in_ch: 1, out_ch: {C1_OUT} }},\n"
        f"    ConvSpec {{ w: &C2_W, scale: C2_SCALE, b: &C2_B, in_ch: {C1_OUT}, out_ch: {C2_OUT} }},\n"
        "];\n\n"
        "/// The embedded model as a borrow the recognizer can run.\n"
        "pub(crate) fn model() -> Crnn<'static> {\n"
        "    Crnn {\n"
        "        h: H, conv: &CONV, gru_in: GRU_IN, gru_hid: GRU_HID,\n"
        f"        fwd: {_gru_ctor('FWD')},\n"
        f"        bwd: {_gru_ctor('BWD')},\n"
        "        fc_w: &FC_W, fc_scale: FC_SCALE, fc_b: &FC_B, alphabet: ALPHABET, rtl: RTL,\n"
        "    }\n"
        "}\n"
    )
    dest = os.path.join(ROOT, f"crates/core/src/raster/ocr_model_{out_group or group}.rs")
    with open(dest, "w") as f:
        f.write(body)
    print(f"wrote {dest} ({os.path.getsize(dest) // 1024} KB)")
    print(
        f"  → wire it: add `pub mod ocr_model_{group};` to raster/mod.rs (behind a Cargo\n"
        f"    feature `ocr-{group}`), then pass `&ocr_model_{group}::model()` to ocr_crnn::recognize."
    )


# ── training (torch-only; defined here so the module imports without PyTorch) ──
def build_and_train(group: str, epochs: int):
    import importlib

    import torch
    import torch.nn as nn
    import torch.nn.functional as F

    import corpora
    import fonts as fontmod
    import hw_datasets
    import render_lines as rl

    ev = importlib.import_module("eval")
    assert rl.STRIP_H == H, "STRIP_H mismatch between render_lines and trainer"

    class Gru(nn.Module):
        """`h' = (1−z)⊙n + z⊙h`, `n = tanh(Wn x + Un(r⊙h))` — reset BEFORE the
        recurrent matmul (matches ocr_crnn.rs). Bias on input linears only."""

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
        def __init__(self, k):
            super().__init__()
            self.c1 = nn.Conv2d(1, C1_OUT, 3, padding=1)
            self.c2 = nn.Conv2d(C1_OUT, C2_OUT, 3, padding=1)
            self.fwd, self.bwd = Gru(C2_OUT, HID), Gru(C2_OUT, HID)
            self.fc = nn.Linear(2 * HID, k + 1)

        def forward(self, x):
            x = F.max_pool2d(F.relu(self.c1(x)), 2)
            x = F.max_pool2d(F.relu(self.c2(x)), 2)
            seq = x.mean(dim=2).permute(0, 2, 1)
            ctx = torch.cat([self.fwd(seq), self.bwd(seq, reverse=True)], dim=2)
            return self.fc(ctx)

    import random

    # CPU-tunable knobs (defaults = full run); override via env for quick runs.
    n_lines = int(os.environ.get("GIGA_OCR_NLINES", N_LINES))
    max_chars = int(os.environ.get("GIGA_OCR_MAXCHARS", 48))
    if os.environ.get("GIGA_OCR_LANGS"):
        SCRIPTS[group]["langs"] = os.environ["GIGA_OCR_LANGS"].split(",")

    alphabet = alphabet_for(group)
    idx = {c: i for i, c in enumerate(alphabet)}
    rtl = is_rtl(group)

    def encode(text: str) -> list[int]:
        # Class-index target. For RTL scripts (Arabic/Hebrew), raqm renders glyphs in visual
        # order (first logical char on the right), but CTC is monotonic over the L→R pixel
        # sequence — so the target must be in VISUAL order too. We reverse here; the runtime
        # reverses the decode back to logical (ocr_crnn::ctc_greedy_decode honours `rtl`). The
        # trainer's val metric uses these same visual-order targets, so it stays consistent.
        tgt = [idx[c] for c in text if c in idx]
        return tgt[::-1] if rtl else tgt
    # Prefer coverage-filtered SYSTEM fonts (correct glyphs, high diversity, no
    # network/subset tofu); fall back to the Google/Noto API downloader.
    fontlimit = int(os.environ.get("GIGA_OCR_FONTLIMIT", 60))
    fonts = fontmod.system_fonts_for_group(group, limit=fontlimit) or fontmod.fonts_for_group(group)
    if not fonts:
        sys.exit(f"no fonts for '{group}' — check network / fonts.py")
    rsel = random.Random(7)
    # Handwriting mix: with probability GIGA_OCR_HW_FRAC, render a line in a
    # cursive/handprint face (downloaded via `fonts.py <group> --handwriting`) instead
    # of a printed one — trains robustness to real handwriting. Default 0 = unchanged.
    hw_frac = float(os.environ.get("GIGA_OCR_HW_FRAC", 0))
    hw_fonts = fontmod.local_handwriting_fonts(group) if hw_frac > 0 else []
    lines = corpora.sample_lines(group, n_lines, seed=7, max_chars=max_chars)
    print(f"  corpus lines: {len(lines)}  (max_chars={max_chars}, fonts={len(fonts)}, "
          f"hw_fonts={len(hw_fonts)} @ frac={hw_frac}, langs={SCRIPTS[group]['langs']})")

    def pick_font(text: str) -> str:
        # cmap-guarded handwriting pick (no tofu when a Latin face meets Cyrillic/Greek);
        # falls back to a printed font if no handwriting face covers the line.
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
        tgt = encode(t)
        if tgt and arr.shape[1] >= 8:
            samples.append((arr.astype(np.float32), tgt))
    print(f"  usable line samples: {len(samples)} (synthetic)")

    # Real handwriting lines (IAM/RIMES via HF datasets-server, see hw_datasets.py) —
    # genuine cursive, normalised to the same strip convention and filtered to the
    # alphabet. Opt-in via GIGA_OCR_HW_REAL="iam,rimes" (Latin → only the alpha group).
    hw_real = os.environ.get("GIGA_OCR_HW_REAL", "").strip()
    if hw_real:
        per = int(os.environ.get("GIGA_OCR_HW_REAL_N", 4000))
        added = 0
        for ds in (d.strip() for d in hw_real.split(",") if d.strip()):
            for arr, t in hw_datasets.fetch_lines(ds, per):
                tgt = encode(t)
                if tgt and arr.shape[1] >= 8:
                    samples.append((arr.astype(np.float32), tgt))
                    added += 1
        print(f"  + real handwriting samples: {added} (from {hw_real})")

    def collate(batch):
        maxw = max(a.shape[1] for a, _ in batch)
        x = np.zeros((len(batch), 1, H, maxw), np.float32)
        widths, targets, tlens = [], [], []
        for i, (a, t) in enumerate(batch):
            x[i, 0, :, : a.shape[1]] = a
            widths.append(a.shape[1] // 4)
            targets.extend(t)
            tlens.append(len(t))
        return (
            torch.from_numpy(x),
            torch.tensor(widths),
            torch.tensor(targets),
            torch.tensor(tlens),
        )

    def greedy(logits):
        blank, prev, out = len(alphabet), len(alphabet), []
        for i in logits.argmax(1).tolist():
            if i != prev and i != blank:
                out.append(alphabet[i])
            prev = i
        return "".join(out)

    rng = np.random.default_rng(7)
    rng.shuffle(samples)
    nval = max(1, len(samples) // 12)
    val, tr = samples[:nval], samples[nval:]
    net = Net(len(alphabet))
    lr = float(os.environ.get("GIGA_OCR_LR", 3e-3))
    opt = torch.optim.Adam(net.parameters(), lr=lr)
    # Step decay speeds the escape from the CTC all-blank basin then stabilizes.
    sched = torch.optim.lr_scheduler.StepLR(opt, step_size=max(1, epochs // 3), gamma=0.5)
    ctc = nn.CTCLoss(blank=len(alphabet), zero_infinity=True)
    best = 2.0  # > 1.0 so the first epoch always writes an initial checkpoint
    for ep in range(epochs):
        net.train()
        rng.shuffle(tr)
        for i in range(0, len(tr), BATCH):
            x, widths, targets, tlens = collate(tr[i : i + BATCH])
            logp = net(x).log_softmax(2).permute(1, 0, 2)
            loss = ctc(logp, targets, widths.clamp(min=1), tlens)
            opt.zero_grad()
            loss.backward()
            opt.step()
        sched.step()
        net.eval()
        with torch.no_grad():
            pairs = [("".join(alphabet[i] for i in t), greedy(net(torch.from_numpy(a[None, None]))[0])) for a, t in val[:1000]]
        cer = ev.corpus_cer(pairs)
        if cer < best:  # checkpoint the best model so far (robust to a long run dying)
            best = cer
            emit_rust(net, alphabet, group, cer, out_group=_out_group(group))  # baked Cargo feature
            emit_gpocr(net, alphabet, group, out_group=_out_group(group))  # runtime host-load blob
        print(f"  epoch {ep + 1:2d}/{epochs}  val_CER={cer:.4f}  best={best:.4f}", flush=True)
    return net, alphabet, best


def emit_gpocr(net, alphabet: str, group: str, out_group: str | None = None) -> str:
    """Write the runtime-loadable `.gpocr` blob (same int8 quantization as emit_rust,
    serialized for `gp_ocr_load_model`) to models/ocr_<group>.gpocr."""
    def ql(t, scale=None):
        a = t.detach().cpu().numpy().reshape(-1)
        scale = scale or (float(np.abs(a).max()) / 127.0 or 1.0)
        q = np.clip(np.round(a / scale), -127, 127).astype(np.int8)
        return [int(x) for x in q], scale

    def fl(t):
        return [float(x) for x in t.detach().cpu().numpy().reshape(-1)]

    c1w, c1s = ql(net.c1.weight)
    c2w, c2s = ql(net.c2.weight)
    conv = [
        (1, C1_OUT, c1s, c1w, fl(net.c1.bias)),
        (C1_OUT, C2_OUT, c2s, c2w, fl(net.c2.bias)),
    ]

    def gdir(g):
        ws = max(float(np.abs(m.weight.detach().cpu().numpy()).max()) for m in (g.wz, g.wr, g.wn)) / 127.0 or 1.0
        us = max(float(np.abs(m.weight.detach().cpu().numpy()).max()) for m in (g.uz, g.ur, g.un)) / 127.0 or 1.0
        wmats = [ql(m.weight, ws)[0] for m in (g.wz, g.wr, g.wn)]
        umats = [ql(m.weight, us)[0] for m in (g.uz, g.ur, g.un)]
        bvecs = [fl(g.wz.bias), fl(g.wr.bias), fl(g.wn.bias)]
        return (ws, us, wmats, umats, bvecs)

    fcw, fcs = ql(net.fc.weight)
    blob = gpocr.serialize(
        rtl=is_rtl(group), h=H, gru_in=C2_OUT, gru_hid=HID, alphabet=alphabet,
        conv=conv, fwd=gdir(net.fwd), bwd=gdir(net.bwd), fc=(fcs, fcw, fl(net.fc.bias)),
    )
    out_dir = os.path.join(ROOT, "models")
    os.makedirs(out_dir, exist_ok=True)
    dest = os.path.join(out_dir, f"ocr_{out_group or group}.gpocr")
    with open(dest, "wb") as f:
        f.write(blob)
    return dest


def main(argv: list[str]) -> int:
    if len(argv) < 2 or argv[1] not in SCRIPTS:
        print(f"usage: {argv[0]} <{'|'.join(SCRIPTS)}> [epochs]", file=sys.stderr)
        return 2
    group = argv[1]
    epochs = int(argv[2]) if len(argv) > 2 else 12
    print(f"group={group} classes={len(alphabet_for(group))} H={H} hid={HID} epochs={epochs}")
    net, alphabet, cer = build_and_train(group, epochs)
    print(f"done: best val_CER={cer:.4f} (best checkpoint already written by the loop)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
