#!/usr/bin/env python3
"""`.gpocr` binary model serializer — mirrors `LoadedModel::from_bytes` in
`crates/core/src/raster/ocr_crnn.rs` byte-for-byte. The host loads the blob at runtime
via `gp_ocr_load_model`, so the engine embeds no weights and stays lean.

Layout (little-endian): magic `GPO1`, u8 rtl, u16 h/gru_in/gru_hid, u32 alphabet_len +
UTF-8, u8 n_conv, per-conv {u16 in_ch, u16 out_ch, f32 scale, i8[out·in·9] w, f32[out] b},
two GRU dirs {f32 w_scale, f32 u_scale, i8 wz/wr/wn[hid·in], i8 uz/ur/un[hid·hid],
f32 bz/br/bn[hid]}, fc {f32 scale, i8[(K+1)·2hid] w, f32[K+1] b}.
"""
from __future__ import annotations

import struct

MAGIC = b"GPO1"


def serialize(rtl, h, gru_in, gru_hid, alphabet, conv, fwd, bwd, fc) -> bytes:
    """`conv`: list of (in_ch, out_ch, scale, w_i8[], b_f32[]). `fwd`/`bwd`:
    (w_scale, u_scale, [wz,wr,wn], [uz,ur,un], [bz,br,bn]). `fc`: (scale, w_i8[], b_f32[])."""
    o = bytearray(MAGIC)
    o += struct.pack("<B", 1 if rtl else 0)
    o += struct.pack("<HHH", h, gru_in, gru_hid)
    ab = alphabet.encode("utf-8")
    o += struct.pack("<I", len(ab)) + ab
    o += struct.pack("<B", len(conv))
    for in_ch, out_ch, scale, w, b in conv:
        o += struct.pack("<HHf", in_ch, out_ch, scale)
        o += struct.pack("<%db" % len(w), *w)
        o += struct.pack("<%df" % len(b), *b)
    for w_scale, u_scale, wmats, umats, bvecs in (fwd, bwd):
        o += struct.pack("<ff", w_scale, u_scale)
        for m in (*wmats, *umats):
            o += struct.pack("<%db" % len(m), *m)
        for v in bvecs:
            o += struct.pack("<%df" % len(v), *v)
    scale, w, b = fc
    o += struct.pack("<f", scale)
    o += struct.pack("<%db" % len(w), *w)
    o += struct.pack("<%df" % len(b), *b)
    return bytes(o)
