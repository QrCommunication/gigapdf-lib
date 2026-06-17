#!/usr/bin/env python3
"""Download Noto + Google fonts per script group for synthetic OCR rendering.

Reuses the engine's own font-fetch flow (the v1 CSS API with a legacy User-Agent →
parse the gstatic `.ttf` URL → download), the same as tools/download_gfonts.py, but
selects families by **script group** (see scripts.py `SCRIPTS[group]["noto"]`).

Notes:
  * Latin/Cyrillic/Greek/Arabic/Hebrew/Indic Noto families serve a single clean TTF.
  * CJK families (`Noto Sans SC/TC/JP/KR`) are *subsetted* by the CSS API into many
    unicode-range slices; all slices are downloaded, but for full coverage prefer the
    monolithic Noto Sans CJK / Source Han release (see docs/OCR_TRAINING_DATA.md).

Run:  python3 tools/ocr/fonts.py <group>      # e.g. alpha | arabic | deva | cjk
"""
from __future__ import annotations

import glob
import os
import re
import sys
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from scripts import SCRIPTS  # noqa: E402

OUT_ROOT = "/tmp/ocr_fonts"
# Legacy UA → v1 CSS API serves a static `.ttf` URL (matches download_gfonts.py).
UA = "Mozilla/4.0"
TTF_RE = re.compile(r"url\((https://fonts\.gstatic\.com/[^)]+?\.ttf)\)")
_TTF_MAGIC = (b"\x00\x01\x00\x00", b"true", b"OTTO", b"ttcf")


def _fetch(url: str, timeout: int = 25) -> bytes:
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read()


def download_family(family: str, out_dir: str) -> list[str]:
    """Download every TTF slice the CSS API serves for `family` into `out_dir`.
    Returns the local paths (≥1 for most scripts, many slices for CJK)."""
    os.makedirs(out_dir, exist_ok=True)
    safe = re.sub(r"[^A-Za-z0-9]", "_", family)
    fam = family.replace(" ", "+")
    urls: list[str] = []
    for css in (
        f"https://fonts.googleapis.com/css?family={fam}",
        f"https://fonts.googleapis.com/css2?family={fam}:wght@400",
    ):
        try:
            css_text = _fetch(css).decode("utf-8", "ignore")
        except Exception:
            continue
        urls = TTF_RE.findall(css_text)
        if urls:
            break
    paths: list[str] = []
    for i, url in enumerate(dict.fromkeys(urls)):  # de-dup, keep order
        dest = os.path.join(out_dir, f"{safe}_{i:03d}.ttf")
        if os.path.exists(dest) and os.path.getsize(dest) > 2000:
            paths.append(dest)
            continue
        try:
            ttf = _fetch(url)
        except Exception:
            continue
        if len(ttf) > 2000 and ttf[:4] in _TTF_MAGIC:
            with open(dest, "wb") as f:
                f.write(ttf)
            paths.append(dest)
    return paths


def fonts_for_group(group: str, out_dir: str | None = None) -> list[str]:
    """Download all Noto families for a script group; return local TTF paths."""
    spec = SCRIPTS[group]
    out_dir = out_dir or os.path.join(OUT_ROOT, group)
    paths: list[str] = []
    for family in spec["noto"]:
        got = download_family(family, out_dir)
        print(f"  {family:24s} → {len(got)} ttf", flush=True)
        paths.extend(got)
    return paths


def _probe_cps(group: str) -> list[int]:
    """~10 script-defining codepoints sampled across a group's non-ASCII alphabet."""
    chars = [c for c in SCRIPTS[group]["chars"] if ord(c) > 0x7F]
    if not chars:
        return [ord("A")]
    step = max(1, len(chars) // 10)
    return [ord(c) for c in chars[::step]][:10]


def system_fonts_for_group(group: str, limit: int = 60, min_cov: float = 0.8, seed: int = 7) -> list[str]:
    """Installed TTFs whose cmap covers the group's script (via fontTools), so we
    never render tofu. Returns up to `limit` paths (shuffled for diversity). Empty if
    fontTools is missing or no font covers the script — callers fall back to Noto."""
    try:
        from fontTools.ttLib import TTFont
    except Exception:
        return []
    import random as _r

    cps = _probe_cps(group)
    need = max(1, int(len(cps) * min_cov))
    paths = sorted(glob.glob("/usr/share/fonts/**/*.ttf", recursive=True))
    _r.Random(seed).shuffle(paths)
    out: list[str] = []
    for p in paths:
        try:
            cmap = TTFont(p, fontNumber=0, lazy=True).getBestCmap()
        except Exception:
            continue
        if sum(1 for cp in cps if cp in cmap) >= need:
            out.append(p)
            if len(out) >= limit:
                break
    return out


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[1] not in SCRIPTS:
        print(f"usage: {argv[0]} <{'|'.join(SCRIPTS)}>", file=sys.stderr)
        return 2
    group = argv[1]
    print(f"downloading Noto fonts for group '{group}' → {OUT_ROOT}/{group}")
    paths = fonts_for_group(group)
    print(f"done: {len(paths)} TTF files")
    return 0 if paths else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
