#!/usr/bin/env python3
"""Download Google Fonts TTFs for OCR training (build-time only).

Reads family names from /tmp/gfonts_families.txt (extracted from the engine's
font catalog) and fetches one regular TTF per family into /tmp/gfonts/, using the
same flow the engine designs for the host: the CSS API with a legacy User-Agent
(so Google serves TTF, not WOFF2) → parse the gstatic URL → download. Parallel,
idempotent, fault-tolerant.

Run:  python3 tools/download_gfonts.py
"""
import concurrent.futures as cf
import os
import re
import urllib.request

OUT = "/tmp/gfonts"
FAMILIES = "/tmp/gfonts_families.txt"
# The bare "Mozilla/4.0" UA makes the v1 CSS API serve a clean static `.ttf`
# URL (`.../s/<face>/<ver>/<hash>.ttf`). A fuller IE6 UA instead returns an
# obfuscated `/l/font?kit=` blob (not a valid TTF), and css2/Firefox yields WOFF.
UA = "Mozilla/4.0"
TTF_RE = re.compile(r"url\((https://fonts\.gstatic\.com/[^)]+?\.ttf)\)")


def fetch(url: str, timeout=20) -> bytes:
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read()


def download_family(family: str) -> str:
    safe = re.sub(r"[^A-Za-z0-9]", "_", family)
    dest = os.path.join(OUT, f"{safe}.ttf")
    if os.path.exists(dest) and os.path.getsize(dest) > 2000:
        return "cached"
    fam = family.replace(" ", "+")
    # v1 CSS API first (serves TTF for the old UA); css2 as a fallback.
    for css in (
        f"https://fonts.googleapis.com/css?family={fam}",
        f"https://fonts.googleapis.com/css2?family={fam}:wght@400",
    ):
        try:
            css_text = fetch(css).decode("utf-8", "ignore")
        except Exception:
            continue
        m = TTF_RE.search(css_text)
        if not m:
            continue
        try:
            ttf = fetch(m.group(1))
        except Exception:
            continue
        if len(ttf) > 2000 and ttf[:4] in (b"\x00\x01\x00\x00", b"true", b"OTTO", b"ttcf"):
            with open(dest, "wb") as f:
                f.write(ttf)
            return "ok"
    return "fail"


def main():
    os.makedirs(OUT, exist_ok=True)
    families = [l.strip() for l in open(FAMILIES) if l.strip()]
    print(f"downloading {len(families)} Google families → {OUT}")
    counts = {"ok": 0, "cached": 0, "fail": 0}
    with cf.ThreadPoolExecutor(max_workers=24) as ex:
        for i, status in enumerate(ex.map(download_family, families), 1):
            counts[status] += 1
            if i % 200 == 0:
                print(f"  {i}/{len(families)}  ok={counts['ok']} cached={counts['cached']} fail={counts['fail']}", flush=True)
    total = len([f for f in os.listdir(OUT) if f.endswith(".ttf")])
    print(f"done: {counts}  → {total} TTFs on disk")


if __name__ == "__main__":
    main()
