#!/usr/bin/env python3
"""Convert an embedded `ocr_model_<group>.rs` (int8 statics emitted by
train_ocr_crnn.py) into a runtime-loadable `.gpocr` blob — without retraining. Lets an
existing trained model be served via the host-loaded path (`gp_ocr_load_model`).

Run: python3 tools/ocr/rs_to_gpocr.py crates/core/src/raster/ocr_model_alpha.rs out.gpocr
"""
from __future__ import annotations

import re
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import gpocr


def _unescape(t: str) -> str:
    out, i = [], 0
    while i < len(t):
        if t[i] == "\\" and i + 1 < len(t):
            out.append(t[i + 1])
            i += 2
        else:
            out.append(t[i])
            i += 1
    return "".join(out)


def parse_rs(path: str) -> dict:
    s = open(path, encoding="utf-8").read()

    def cint(name):
        return int(re.search(rf"pub const {name}: usize = (\d+);", s).group(1))

    def cf32(name):
        return float(re.search(rf"pub const {name}: f32 = ([0-9.eE+\-]+);", s).group(1))

    def i8(name):
        body = re.search(rf"pub static {name}: \[i8; \d+\] = \[([^\]]*)\];", s).group(1).strip()
        return [int(x) for x in body.split(",")] if body else []

    def f32(name):
        body = re.search(rf"pub static {name}: \[f32; \d+\] = \[([^\]]*)\];", s).group(1).strip()
        return [float(x) for x in body.split(",")] if body else []

    rtl = re.search(r"pub const RTL: bool = (true|false);", s).group(1) == "true"
    alphabet = _unescape(re.search(r'pub static ALPHABET: &str = "(.*)";', s).group(1))
    c1w, c1b, c2w, c2b = i8("C1_W"), f32("C1_B"), i8("C2_W"), f32("C2_B")
    out1, out2 = len(c1b), len(c2b)
    conv = [
        (len(c1w) // (out1 * 9), out1, cf32("C1_SCALE"), c1w, c1b),
        (len(c2w) // (out2 * 9), out2, cf32("C2_SCALE"), c2w, c2b),
    ]

    def gru(p):
        return (
            cf32(f"{p}_W_SCALE"), cf32(f"{p}_U_SCALE"),
            [i8(f"{p}_WZ"), i8(f"{p}_WR"), i8(f"{p}_WN")],
            [i8(f"{p}_UZ"), i8(f"{p}_UR"), i8(f"{p}_UN")],
            [f32(f"{p}_BZ"), f32(f"{p}_BR"), f32(f"{p}_BN")],
        )

    return dict(
        rtl=rtl, h=cint("H"), gru_in=cint("GRU_IN"), gru_hid=cint("GRU_HID"),
        alphabet=alphabet, conv=conv, fwd=gru("FWD"), bwd=gru("BWD"),
        fc=(cf32("FC_SCALE"), i8("FC_W"), f32("FC_B")),
    )


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(f"usage: {argv[0]} <ocr_model_X.rs> <out.gpocr>", file=sys.stderr)
        return 2
    m = parse_rs(argv[1])
    blob = gpocr.serialize(**m)
    with open(argv[2], "wb") as f:
        f.write(blob)
    print(f"wrote {argv[2]} ({len(blob)} bytes) — {len(m['alphabet'])} classes, rtl={m['rtl']}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
