#!/usr/bin/env bash
# Regenerate crates/core/src/raster/avif/cdf.rs from dav1d's default CDF tables.
#
# Method: compile-and-dump. We assemble a small C program from dav1d's own
# headers/sources (BSD-2-Clause, © VideoLAN / Two Orioles) — the enum
# definitions, the CDF expansion macros, the CdfContext struct layout, and the
# `default_cdf` / `default_coef_cdf` initializers — then a generated `main()`
# (extract_main.c) walks each field and prints it as a Rust nested-array literal.
#
# Compiling the *real* struct lets the C compiler resolve designated
# initializers, enum indices, and implicit zero-fill EXACTLY as dav1d does at
# runtime — no hand-parsing, no fabricated values. The values are stored inverse
# Q15 (`32768 - cdf`) with a trailing adaptation counter + SIMD padding, which is
# precisely the layout `Msac::symbol_adapt` consumes.
#
# This tool runs at dev time only; the AV1 decoder ships zero runtime deps.
set -euo pipefail
cd "$(dirname "$0")"

REV="${DAV1D_REV:-master}"
BASE="https://raw.githubusercontent.com/videolan/dav1d/${REV}/src"
OUT="../../crates/core/src/raster/avif/cdf.rs"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT

for f in cdf.c cdf.h levels.h; do
  curl -fsSL "${BASE}/${f}" -o "${T}/${f}"
done

# Enum blocks only (skip the partial structs interleaved in levels.h).
awk '/^enum [A-Za-z0-9_]+ \{/{p=1} p{print} /^\};/{if(p){p=0}}' "${T}/levels.h" > "${T}/enums.c"

# Boundaries (robust to minor revision drift): CDF macros, the three struct
# typedefs, and the two initializer tables.
macro_lo=$(grep -n '^#define CDF1(' "${T}/cdf.c" | head -1 | cut -d: -f1)
def_lo=$(grep -n '^typedef struct CdfDefaultContext {' "${T}/cdf.c" | head -1 | cut -d: -f1)
coef_close=$(awk 'NR>'"$(grep -n 'default_coef_cdf\[4\]' "${T}/cdf.c" | head -1 | cut -d: -f1)"' && /^[[:space:]]*};[[:space:]]*$/{print NR; exit}' "${T}/cdf.c")

{
  printf '#include <stdint.h>\n#include <stdio.h>\n'
  printf '#define ALIGN(x, n) x\n#define DAV1D_MAX_SEGMENTS 8\n#define DAV1D_N_SWITCHABLE_FILTERS 3\n'
  cat "${T}/enums.c"
  sed -n "${macro_lo},$((def_lo-1))p" "${T}/cdf.c"
  sed -n '/^typedef struct CdfModeContext {/,/^} CdfModeContext;/p' "${T}/cdf.h"
  sed -n '/^typedef struct CdfCoefContext {/,/^} CdfCoefContext;/p' "${T}/cdf.h"
  sed -n '/^typedef struct CdfMvComponent {/,/^} CdfMvComponent;/p' "${T}/cdf.h"
  sed -n "${def_lo},${coef_close}p" "${T}/cdf.c"
  cat extract_main.c
} > "${T}/extract.c"

cc -O1 -w "${T}/extract.c" -o "${T}/extract"
"${T}/extract" > "${OUT}"
echo "Wrote ${OUT} ($(grep -c 'pub(crate) static' "${OUT}") tables, $(wc -c < "${OUT}") bytes)"
