#!/usr/bin/env python3
"""Deterministically extract VP8 constant tables from RFC 6386 → vp8_tables.rs.

Big tables (default_coeff_probs, coeff_update_probs, kf_bmode_prob) span several
RFC pages, so per-page headers/footers (which contain stray digits like
"[Page 73]" / "November 2011") are filtered before integer extraction. Small
tables already hand-written in vp8.rs are re-extracted here only to VERIFY they
match the RFC bit-for-bit.
"""
import re
import sys

RFC = "/tmp/rfc6386.txt"
OUT = "/home/rony/Projets/gigapdf-lib/crates/core/src/raster/vp8_tables.rs"

with open(RFC, encoding="utf-8") as f:
    LINES = f.read().split("\n")

PAGE_BOUNDARY = re.compile(r"Bankoski|Informational|RFC 6386|\[Page|November 2011")


def extract_ints(start_line_1based):
    """From the first '{' at/after start line, return all ints that live INSIDE
    the brace-balanced block (depth >= 1), skipping page-boundary lines and C
    comments. Collecting only at depth >= 1 excludes array dimensions such as
    `[16]` / `[num_ymodes - 1]` that share the declaration line with the `{`."""
    i = start_line_1based - 1
    while i < len(LINES) and "{" not in LINES[i]:
        i += 1
    depth = 0
    started = False
    nums = []
    buf = ""

    def flush():
        nonlocal buf
        if buf and depth >= 1:
            nums.append(int(buf))
        buf = ""

    while i < len(LINES):
        raw = LINES[i]
        i += 1
        if PAGE_BOUNDARY.search(raw) or "\f" in raw:
            continue
        line = re.sub(r"//.*$", "", raw)
        line = re.sub(r"/\*.*?\*/", "", line)
        for ch in line:
            if ch.isdigit():
                buf += ch
            else:
                flush()
                if ch == "{":
                    depth += 1
                    started = True
                elif ch == "}":
                    depth -= 1
        flush()
        if started and depth == 0:
            break
    return nums


def find_decl(pattern):
    rx = re.compile(pattern)
    for n, ln in enumerate(LINES, 1):
        if rx.search(ln):
            return n
    raise SystemExit(f"decl not found: {pattern}")


def reshape(flat, dims):
    if len(dims) == 1:
        assert len(flat) == dims[0], (len(flat), dims)
        return list(flat)
    step = 1
    for d in dims[1:]:
        step *= d
    return [reshape(flat[i * step:(i + 1) * step], dims[1:]) for i in range(dims[0])]


def rust_lit(nested, ty="u8"):
    if isinstance(nested[0], int):
        return "[" + ", ".join(str(x) for x in nested) + "]"
    return "[\n" + ",\n".join("    " + rust_lit(x, ty).replace("\n", "\n    ") for x in nested) + ",\n]"


# ── big tables ────────────────────────────────────────────────────────────────
default_coeff = extract_ints(find_decl(r"const Prob default_coeff_probs \[4\]"))
coeff_update = extract_ints(find_decl(r"const Prob coeff_update_probs \[4\]"))
# kf_bmode_prob: the DATA block (with '=') is the 2nd occurrence (line ~2607).
kf_decls = [n for n, ln in enumerate(LINES, 1) if "kf_bmode_prob [num_intra_bmodes]" in ln]
kf_bmode = None
for d in kf_decls:
    cand = extract_ints(d)
    if len(cand) == 900:
        kf_bmode = cand
        break

assert len(default_coeff) == 4 * 8 * 3 * 11, f"default_coeff={len(default_coeff)}"
assert len(coeff_update) == 4 * 8 * 3 * 11, f"coeff_update={len(coeff_update)}"
assert kf_bmode is not None and len(kf_bmode) == 10 * 10 * 9, f"kf_bmode={kf_bmode and len(kf_bmode)}"
assert all(0 <= x <= 255 for x in default_coeff + coeff_update + kf_bmode), "out-of-range prob"

# ── verify the small tables hand-written in vp8.rs ────────────────────────────
HARDCODED = {
    "dc_q_lookup[128]": (r"static const int dc_q_lookup\[128\]", [
        4,5,6,7,8,9,10,10,11,12,13,14,15,16,17,17,18,19,20,20,21,21,22,22,23,23,24,25,25,26,27,28,
        29,30,31,32,33,34,35,36,37,37,38,39,40,41,42,43,44,45,46,46,47,48,49,50,51,52,53,54,55,56,
        57,58,59,60,61,62,63,64,65,66,67,68,69,70,71,72,73,74,75,76,76,77,78,79,80,81,82,83,84,85,
        86,87,88,89,91,93,95,96,98,100,101,102,104,106,108,110,112,114,116,118,122,124,126,128,130,
        132,134,136,138,140,143,145,148,151,154,157]),
    "ac_q_lookup[128]": (r"static const int ac_q_lookup\[128\]", [
        4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,
        36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,60,62,64,66,68,70,72,
        74,76,78,80,82,84,86,88,90,92,94,96,98,100,102,104,106,108,110,112,114,116,119,122,125,128,
        131,134,137,140,143,146,149,152,155,158,161,164,167,170,173,177,181,185,189,193,197,201,205,
        209,213,217,221,225,229,234,239,245,249,254,259,264,269,274,279,284]),
    "coeff_bands[16]": (r"const int coeff_bands \[16\]", [0,1,2,3,6,4,5,6,6,6,6,6,6,6,6,7]),
    "zigzag[16]": (r"static const unsigned int zigzag\[16\]", [0,1,4,8,5,2,3,6,9,12,13,10,7,11,14,15]),
    "kf_ymode_prob": (r"const Prob kf_ymode_prob", [145,156,163,128]),
    "kf_uv_mode_prob": (r"const Prob kf_uv_mode_prob", [142,114,183]),
}
print("── small-table verification (vp8.rs vs RFC) ──")
ok = True
for name, (pat, expected) in HARDCODED.items():
    got = extract_ints(find_decl(pat))[: len(expected)]
    match = got == expected
    ok = ok and match
    print(f"  {name:18} {'✓ match' if match else '✗ MISMATCH'}")
    if not match:
        print("    RFC:", got)
        print("    src:", expected)
if not ok:
    sys.exit("SMALL TABLE MISMATCH — fix vp8.rs before generating")

# ── emit vp8_tables.rs ────────────────────────────────────────────────────────
dc = reshape(default_coeff, [4, 8, 3, 11])
cu = reshape(coeff_update, [4, 8, 3, 11])
kf = reshape(kf_bmode, [10, 10, 9])

with open(OUT, "w", encoding="utf-8") as f:
    f.write(
        "// AUTO-GENERATED from RFC 6386 (§13.4 coeff_update_probs, §13.5\n"
        "// default_coeff_probs, §11.2 kf_bmode_prob) by tools/extract_vp8_tables.py.\n"
        "// Do not edit by hand — re-run the extractor to regenerate.\n\n"
    )
    f.write("/// Default DCT-token probabilities `[plane][band][ctx][token]` (RFC 6386 §13.5).\n")
    f.write("pub(super) const DEFAULT_COEFF_PROBS: [[[[u8; 11]; 3]; 8]; 4] = " + rust_lit(dc) + ";\n\n")
    f.write("/// Per-frame coeff-probability update flags `[plane][band][ctx][token]` (§13.4).\n")
    f.write("pub(super) const COEFF_UPDATE_PROBS: [[[[u8; 11]; 3]; 8]; 4] = " + rust_lit(cu) + ";\n\n")
    f.write("/// Keyframe 4×4 intra sub-block mode context probabilities `[above][left][mode]` (§11.2).\n")
    f.write("pub(super) const KF_BMODE_PROBS: [[[u8; 9]; 10]; 10] = " + rust_lit(kf) + ";\n")

print(f"\n✓ wrote {OUT}")
print(f"  DEFAULT_COEFF_PROBS = {len(default_coeff)} ints, COEFF_UPDATE_PROBS = {len(coeff_update)}, KF_BMODE_PROBS = {len(kf_bmode)}")
