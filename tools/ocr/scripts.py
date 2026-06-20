#!/usr/bin/env python3
"""Per-script character sets and the script-group registry for OCR training.

Class sets are composed from **Unicode ranges** (not hand-typed glyphs) so they are
typo-free and self-documenting. A *script group* is the unit a single CRNN+CTC model
covers (see docs/OCR_ARCHITECTURE.md): segmentable alphabets share one model;
CJK / Arabic-Hebrew / each Indic script get their own.

The CTC blank is implicit (index == len(chars)); it is never part of a class set.

Self-test:  python3 tools/ocr/scripts.py
"""
from __future__ import annotations

import os


def _range(lo: int, hi: int, exclude: set[int] | None = None) -> str:
    """All code points in [lo, hi] as a string, minus `exclude`."""
    ex = exclude or set()
    return "".join(chr(c) for c in range(lo, hi + 1) if c not in ex)


def _dedup(s: str) -> str:
    """Order-preserving de-duplication; drops whitespace and control chars."""
    seen: set[str] = set()
    out: list[str] = []
    for ch in s:
        if ch.isspace() or ord(ch) < 0x20:
            continue
        if ch not in seen:
            seen.add(ch)
            out.append(ch)
    return "".join(out)


# ── shared building blocks ───────────────────────────────────────────────────
DIGITS = "0123456789"
LATIN_UP = "ABCDEFGHIJKLMNOPQRSTUVWXYZ"
LATIN_LO = "abcdefghijklmnopqrstuvwxyz"
# A pragmatic shared punctuation set (printable, single-width).
PUNCT = ".,;:!?'\"()[]{}-/\\&%@#$+=*<>°|~^_`§«»…—–’‘“”€£"

# Latin-1 Supplement letters (À-ÿ) minus the × and ÷ math signs.
_LATIN1 = _range(0x00C0, 0x00FF, exclude={0x00D7, 0x00F7})
# Latin Extended-A (Polish, Czech, Croatian, Baltic, Turkish, Romanian, …).
_LATIN_A = _range(0x0100, 0x017F)
# Vietnamese precomposed vowels (Latin Extended Additional).
_LATIN_VIET = _range(0x1EA0, 0x1EF9)
# Romanian comma-below + a few Extended-B letters used in European orthographies.
_LATIN_B = "ăĂơƠưƯșȘțȚ"
LATIN_EXT = _dedup(_LATIN1 + _LATIN_A + _LATIN_VIET + _LATIN_B)

# Cyrillic: Russian core + Ukrainian/Belarusian/Serbian/Macedonian additions.
CYRILLIC = _dedup(
    _range(0x0410, 0x044F)  # А-я
    + "ЁёЂђЃѓЄєЅѕІіЇїЈјЉљЊњЋћЌќЎўЏџҐґ"
)

# Greek (modern): mono/poly-tonic vowels + the base alphabet (skip reserved U+03A2).
GREEK = _dedup(
    _range(0x0391, 0x03A9, exclude={0x03A2})  # Α-Ω
    + _range(0x03B1, 0x03C9)  # α-ω
    + "άέήίόύώϊϋΐΰΆΈΉΊΌΎΏ"
)

# Arabic letters + Eastern-Arabic digits + Persian/Urdu extensions.
ARABIC = _dedup(
    _range(0x0621, 0x064A)  # hamza … yeh (base letters, all contextual forms shape from these)
    + _range(0x0660, 0x0669)  # ٠-٩
    + "پچژگکیۀ"  # Persian/Urdu
)

# Hebrew letters (final forms included in the block range).
HEBREW = _dedup(_range(0x05D0, 0x05EA))

# Devanagari: independent vowels, consonants, matras, virama, anusvara, digits.
DEVANAGARI = _dedup(
    "ँंः"
    + _range(0x0905, 0x0939)  # अ … ह
    + _range(0x093E, 0x094D)  # matras + virama
    + _range(0x0966, 0x096F)  # ०-९
)

# Bengali: vowels, consonants, signs, digits.
BENGALI = _dedup(
    _range(0x0985, 0x09B9, exclude={0x098D, 0x098E, 0x0991, 0x0992, 0x09A9, 0x09B1, 0x09B3, 0x09B4, 0x09B5})
    + _range(0x09BE, 0x09CC)
    + "ংঃঁ্"
    + _range(0x09E6, 0x09EF)
)

# Tamil: vowels, consonants, signs, digits.
TAMIL = _dedup(
    _range(0x0B85, 0x0B94)
    + _range(0x0B95, 0x0BB9, exclude={0x0B96, 0x0B97, 0x0B98, 0x0B9B, 0x0B9D, 0x0BA0, 0x0BA1, 0x0BA2, 0x0BA5, 0x0BA6, 0x0BA7, 0x0BAB, 0x0BAC, 0x0BAD})
    + _range(0x0BBE, 0x0BCD)
    + _range(0x0BE6, 0x0BEF)
)

# A small built-in CJK fallback (frequent Hanzi/Kana) so the registry is usable
# without a download; the full set is loaded from a frequency list via load_charset().
CJK_FALLBACK = _dedup(
    "的一是不了人我在有他这中大来上国个到说们为子和你地出道也时年得就那要下以生会自着去之过家学"
    + "あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほまみむめもやゆよらりるれろわをん"
    + "アイウエオカキクケコサシスセソタチツテトナニヌネノハヒフヘホマミムメモヤユヨラリルレロワヲン"
    + "가나다라마바사아자차카타파하"  # a few Hangul syllables
)


def load_charset(path: str) -> str:
    """Load a newline- or char-separated charset file (e.g. a CJK frequency list),
    de-duplicated and whitespace-stripped. Use for large scripts (CJK)."""
    with open(path, encoding="utf-8") as f:
        return _dedup(f.read())


# ── script-group registry ─────────────────────────────────────────────────────
# Each group is one CRNN+CTC model:
#   chars         the class set (CTC blank is implicit at index len(chars))
#   rtl           reading direction (reverse decoded order at runtime)
#   subsets       Google-Fonts `subset` tags → font selection (see fonts.py)
#   noto          Noto family stems to download (see fonts.py)
#   langs         corpus language codes to sample text from (see corpora.py)
SCRIPTS: dict[str, dict] = {
    # Segmentable LTR alphabets share one model (Latin-extended + Cyrillic + Greek).
    "alpha": {
        "chars": _dedup(DIGITS + LATIN_UP + LATIN_LO + LATIN_EXT + CYRILLIC + GREEK + PUNCT),
        "rtl": False,
        "subsets": ["latin", "latin-ext", "cyrillic", "cyrillic-ext", "greek", "greek-ext"],
        "noto": ["Noto Sans", "Noto Serif"],
        "langs": ["eng", "fra", "deu", "spa", "ita", "por", "pol", "ces", "tur", "vie", "rus", "ukr", "bul", "srp", "ell"],
    },
    "cjk": {
        "chars": CJK_FALLBACK,  # replace via load_charset() for the full set
        "rtl": False,
        "subsets": ["chinese-simplified", "chinese-traditional", "japanese", "korean"],
        "noto": ["Noto Sans SC", "Noto Sans TC", "Noto Sans JP", "Noto Sans KR"],
        "langs": ["chi_sim", "chi_tra", "jpn", "kor"],
    },
    # RTL, cursive: Arabic (+ Persian/Urdu) and Hebrew.
    "arabic": {
        "chars": _dedup(ARABIC + HEBREW + DIGITS + PUNCT),
        "rtl": True,
        "subsets": ["arabic", "hebrew"],
        "noto": ["Noto Naskh Arabic", "Noto Sans Arabic", "Noto Sans Hebrew"],
        "langs": ["ara", "fas", "urd", "heb"],
    },
    "deva": {
        "chars": _dedup(DEVANAGARI + DIGITS + LATIN_UP + LATIN_LO + PUNCT),
        "rtl": False,
        "subsets": ["devanagari"],
        "noto": ["Noto Sans Devanagari"],
        "langs": ["hin", "mar", "nep", "san"],
    },
    "beng": {
        "chars": _dedup(BENGALI + DIGITS + LATIN_UP + LATIN_LO + PUNCT),
        "rtl": False,
        "subsets": ["bengali"],
        "noto": ["Noto Sans Bengali"],
        "langs": ["ben", "asm"],
    },
    "taml": {
        "chars": _dedup(TAMIL + DIGITS + LATIN_UP + LATIN_LO + PUNCT),
        "rtl": False,
        "subsets": ["tamil"],
        "noto": ["Noto Sans Tamil"],
        "langs": ["tam"],
    },
    # Japanese (Hiragana/Katakana + common Kanji) and Korean (Hangul) get their own
    # models, like each Indic script — distinct from the Chinese-only `cjk` group. The
    # real class set is data-driven (top-frequency chars from the corpus): set
    # GIGA_OCR_CHARSET_JPN / GIGA_OCR_CHARSET_KOR to a build_cjk_charset.py output. The
    # built-in fallback only keeps the group usable without that file.
    "jpn": {
        "chars": CJK_FALLBACK,  # kana + a few Kanji; replace via GIGA_OCR_CHARSET_JPN
        "rtl": False,
        "subsets": ["japanese"],
        "noto": ["Noto Sans JP"],
        "langs": ["jpn"],
    },
    "kor": {
        "chars": _dedup("가나다라마바사아자차카타파하" + DIGITS + PUNCT),  # replace via GIGA_OCR_CHARSET_KOR
        "rtl": False,
        "subsets": ["korean"],
        "noto": ["Noto Sans KR"],
        "langs": ["kor"],
    },
}


# Space is a real output class (word boundaries) for every script; appended after
# _dedup (which strips whitespace), so it lands as the last non-blank class index.
for _s in SCRIPTS.values():
    if " " not in _s["chars"]:
        _s["chars"] = _s["chars"] + " "


# Optional per-group charset override from a file (e.g. a real CJK frequency list built by
# tools/ocr/build_cjk_charset.py): set GIGA_OCR_CHARSET_<GROUP>=path (e.g. GIGA_OCR_CHARSET_CJK).
# Enables a real ~2–6k-class CJK model in place of the tiny built-in fallback. Space is kept
# as a class. Unknown/missing paths are ignored (falls back to the built-in set).
for _g in list(SCRIPTS):
    _p = os.environ.get(f"GIGA_OCR_CHARSET_{_g.upper()}")
    if _p and os.path.exists(_p):
        _cs = load_charset(_p)
        if _cs:
            SCRIPTS[_g]["chars"] = _cs if " " in _cs else _cs + " "


def alphabet_for(group: str) -> str:
    """The class-set string for a script group (raises KeyError if unknown)."""
    return SCRIPTS[group]["chars"]


def is_rtl(group: str) -> bool:
    return SCRIPTS[group]["rtl"]


# ── self-test ──────────────────────────────────────────────────────────────────
def _selftest() -> int:
    ok = True
    for name, spec in SCRIPTS.items():
        chars = spec["chars"]
        # No whitespace, no duplicates, non-empty.
        assert chars, f"{name}: empty"
        assert len(set(chars)) == len(chars), f"{name}: duplicate chars"
        # Exactly one whitespace class allowed: the space (word boundary).
        assert chars.count(" ") == 1 and not any(c.isspace() and c != " " for c in chars), (
            f"{name}: bad whitespace in class set"
        )
        print(f"  {name:8s} rtl={str(spec['rtl']):5s} classes={len(chars):4d}  (blank idx={len(chars)})")
    # Sanity: the shared alphabet must carry Latin, Cyrillic and Greek.
    a = alphabet_for("alpha")
    for probe in ("A", "é", "ł", "Я", "Ω", "ş"):
        assert probe in a, f"alpha missing {probe!r}"
    assert is_rtl("arabic") and not is_rtl("alpha")
    print("scripts.py self-test: OK" if ok else "FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    import sys

    sys.exit(_selftest())
