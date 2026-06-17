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
import json
import os
import re
import sys
import urllib.request
from functools import lru_cache

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


# ── handwriting fonts (synthetic cursive/handprint, TRDG/Tesseract-style) ───────
_META_URL = "https://fonts.google.com/metadata/fonts"
_META_CACHE = os.path.join(OUT_ROOT, "_gfonts_metadata.json")


def _gfonts_metadata() -> list[dict]:
    """Google-Fonts family metadata (cached to /tmp). Each entry carries `family`,
    `category` ('Handwriting', 'Serif', …) and `subsets` (['latin', 'cyrillic', …])."""
    try:
        if os.path.getsize(_META_CACHE) > 1000:
            with open(_META_CACHE, encoding="utf-8") as f:
                return json.load(f)
    except OSError:
        pass
    req = urllib.request.Request(_META_URL, headers={"User-Agent": "Mozilla/5.0"})
    with urllib.request.urlopen(req, timeout=25) as r:
        raw = r.read().decode("utf-8", "ignore").lstrip(")]}'\n ")  # strip XSSI guard
    lst = json.loads(raw).get("familyMetadataList", [])
    try:
        os.makedirs(OUT_ROOT, exist_ok=True)
        with open(_META_CACHE, "w", encoding="utf-8") as f:
            json.dump(lst, f)
    except OSError:
        pass
    return lst


def handwriting_families_for_group(group: str, limit: int = 200, seed: int = 7) -> list[str]:
    """Google-Fonts *Handwriting*-category families whose `subsets` intersect the
    group's subsets — so they actually render the script (never tofu). Shuffled for
    diversity, capped at `limit`. Latin is richly covered (~250 families); other
    scripts have few, where real handwriting datasets matter more."""
    import random as _r

    want = set(SCRIPTS[group].get("subsets", []))
    fams = [
        f["family"]
        for f in _gfonts_metadata()
        if f.get("category") == "Handwriting" and want & set(f.get("subsets") or [])
    ]
    _r.Random(seed).shuffle(fams)
    return fams[:limit]


def handwriting_fonts_for_group(
    group: str, out_dir: str | None = None, limit: int = 200, seed: int = 7
) -> list[str]:
    """Download Handwriting-category TTFs covering the group's script → local paths.
    A synthetic-handwriting source that complements the printed Noto/system fonts:
    rendering corpus lines in cursive/handprint faces trains the CRNN toward real
    handwriting (the TRDG/Tesseract recipe), with zero runtime change."""
    out_dir = out_dir or os.path.join(OUT_ROOT, f"{group}_hw")
    paths: list[str] = []
    for family in handwriting_families_for_group(group, limit=limit, seed=seed):
        got = download_family(family, out_dir)
        if got:
            paths.extend(got)
            print(f"  {family:28s} → {len(got)} ttf", flush=True)
    return paths


def local_handwriting_fonts(group: str, out_dir: str | None = None) -> list[str]:
    """Already-downloaded handwriting TTFs for a group (see `handwriting_fonts_for_group`)."""
    out_dir = out_dir or os.path.join(OUT_ROOT, f"{group}_hw")
    return sorted(glob.glob(os.path.join(out_dir, "*.ttf")))


@lru_cache(maxsize=8192)
def _font_cmap(path: str) -> frozenset[int]:
    """Cached set of code points a font can render (best cmap)."""
    try:
        from fontTools.ttLib import TTFont

        return frozenset(TTFont(path, fontNumber=0, lazy=True).getBestCmap().keys())
    except Exception:
        return frozenset()


def font_covers(path: str, text: str, min_frac: float = 0.999) -> bool:
    """True if the font's cmap covers (nearly) every non-space char in `text`. Guards
    a handwriting face declared for one subset (e.g. latin) from rendering tofu when it
    meets another script (Cyrillic/Greek) in a shared-model group like `alpha`."""
    cps = [ord(c) for c in text if not c.isspace()]
    if not cps:
        return False
    cmap = _font_cmap(path)
    return bool(cmap) and sum(1 for cp in cps if cp in cmap) >= min_frac * len(cps)


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
    flags = {a for a in argv[1:] if a.startswith("-")}
    pos = [a for a in argv[1:] if not a.startswith("-")]
    if len(pos) != 1 or pos[0] not in SCRIPTS:
        print(f"usage: {argv[0]} <{'|'.join(SCRIPTS)}> [--handwriting]", file=sys.stderr)
        return 2
    group = pos[0]
    if "--handwriting" in flags:
        print(f"downloading handwriting fonts for group '{group}' → {OUT_ROOT}/{group}_hw")
        paths = handwriting_fonts_for_group(group)
        print(f"done: {len(paths)} handwriting TTF files")
        return 0 if paths else 1
    print(f"downloading Noto fonts for group '{group}' → {OUT_ROOT}/{group}")
    paths = fonts_for_group(group)
    print(f"done: {len(paths)} TTF files")
    return 0 if paths else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
