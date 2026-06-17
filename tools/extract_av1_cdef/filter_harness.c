/* Reference generator for the dav1d AV1 CDEF filter block (BSD-2-Clause).
 * Runs the real `cdef_filter_block_c` on deterministic blocks (every direction,
 * pri-only / sec-only / both) and dumps the filtered output as Rust literals to
 * validate the Rust transcription bit-for-bit. dav1d src/cdef_tmpl.c. Dev-only. */
#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

typedef uint8_t pixel;
#define PXSTRIDE(s) (s)
#define bitdepth_max 255
static int bitdepth_from_max(int m) { (void) m; return 8; }
static inline int imin(int a, int b) { return a < b ? a : b; }
static inline int imax(int a, int b) { return a > b ? a : b; }
static inline unsigned umin(unsigned a, unsigned b) { return a < b ? a : b; }
static inline int iclip(int v, int lo, int hi) { return v < lo ? lo : (v > hi ? hi : v); }
static inline int abs_i(int v) { return v < 0 ? -v : v; }
#define abs abs_i
static inline int apply_sign(int a, int s) { return s < 0 ? -a : a; }
static inline int ulog2(unsigned x) { return 31 ^ __builtin_clz(x); }

enum CdefEdgeFlags {
    CDEF_HAVE_LEFT = 1 << 0, CDEF_HAVE_RIGHT = 1 << 1,
    CDEF_HAVE_TOP = 1 << 2, CDEF_HAVE_BOTTOM = 1 << 3,
};

/* verbatim from dav1d src/tables.c */
static const int8_t dav1d_cdef_directions[2 + 8 + 2][2] = {
    {  1 * 12 + 0,  2 * 12 + 0 }, // 6
    {  1 * 12 + 0,  2 * 12 - 1 }, // 7
    { -1 * 12 + 1, -2 * 12 + 2 }, // 0
    {  0 * 12 + 1, -1 * 12 + 2 }, // 1
    {  0 * 12 + 1,  0 * 12 + 2 }, // 2
    {  0 * 12 + 1,  1 * 12 + 2 }, // 3
    {  1 * 12 + 1,  2 * 12 + 2 }, // 4
    {  1 * 12 + 0,  2 * 12 + 1 }, // 5
    {  1 * 12 + 0,  2 * 12 + 0 }, // 6
    {  1 * 12 + 0,  2 * 12 - 1 }, // 7
    { -1 * 12 + 1, -2 * 12 + 2 }, // 0
    {  0 * 12 + 1, -1 * 12 + 2 }, // 1
};

static inline int constrain(const int diff, const int threshold, const int shift) {
    const int adiff = abs(diff);
    return apply_sign(imin(adiff, imax(0, threshold - (adiff >> shift))), diff);
}
static inline void fill(int16_t *tmp, const ptrdiff_t stride, const int w, const int h) {
    for (int y = 0; y < h; y++) { for (int x = 0; x < w; x++) tmp[x] = INT16_MIN; tmp += stride; }
}
/* verbatim from dav1d src/cdef_tmpl.c */
static void padding(int16_t *tmp, const ptrdiff_t tmp_stride,
                    const pixel *src, const ptrdiff_t src_stride,
                    const pixel (*left)[2], const pixel *top, const pixel *bottom,
                    const int w, const int h, const enum CdefEdgeFlags edges) {
    int x_start = -2, x_end = w + 2, y_start = -2, y_end = h + 2;
    if (!(edges & CDEF_HAVE_TOP)) { fill(tmp - 2 - 2 * tmp_stride, tmp_stride, w + 4, 2); y_start = 0; }
    if (!(edges & CDEF_HAVE_BOTTOM)) { fill(tmp + h * tmp_stride - 2, tmp_stride, w + 4, 2); y_end -= 2; }
    if (!(edges & CDEF_HAVE_LEFT)) { fill(tmp + y_start * tmp_stride - 2, tmp_stride, 2, y_end - y_start); x_start = 0; }
    if (!(edges & CDEF_HAVE_RIGHT)) { fill(tmp + y_start * tmp_stride + w, tmp_stride, 2, y_end - y_start); x_end -= 2; }
    for (int y = y_start; y < 0; y++) { for (int x = x_start; x < x_end; x++) tmp[x + y * tmp_stride] = top[x]; top += PXSTRIDE(src_stride); }
    for (int y = 0; y < h; y++) for (int x = x_start; x < 0; x++) tmp[x + y * tmp_stride] = left[y][2 + x];
    for (int y = 0; y < h; y++) { for (int x = (y < h) ? 0 : x_start; x < x_end; x++) tmp[x] = src[x]; src += PXSTRIDE(src_stride); tmp += tmp_stride; }
    for (int y = h; y < y_end; y++) { for (int x = x_start; x < x_end; x++) tmp[x] = bottom[x]; bottom += PXSTRIDE(src_stride); tmp += tmp_stride; }
}
/* verbatim from dav1d src/cdef_tmpl.c */
static void cdef_filter_block_c(pixel *dst, const ptrdiff_t dst_stride,
                    const pixel (*left)[2], const pixel *const top, const pixel *const bottom,
                    const int pri_strength, const int sec_strength,
                    const int dir, const int damping, const int w, int h,
                    const enum CdefEdgeFlags edges) {
    const ptrdiff_t tmp_stride = 12;
    assert((w == 4 || w == 8) && (h == 4 || h == 8));
    int16_t tmp_buf[144];
    int16_t *tmp = tmp_buf + 2 * tmp_stride + 2;
    padding(tmp, tmp_stride, dst, dst_stride, left, top, bottom, w, h, edges);
    if (pri_strength) {
        const int bitdepth_min_8 = bitdepth_from_max(bitdepth_max) - 8;
        const int pri_tap = 4 - ((pri_strength >> bitdepth_min_8) & 1);
        const int pri_shift = imax(0, damping - ulog2(pri_strength));
        if (sec_strength) {
            const int sec_shift = damping - ulog2(sec_strength);
            do {
                for (int x = 0; x < w; x++) {
                    const int px = dst[x];
                    int sum = 0, max = px, min = px, pri_tap_k = pri_tap;
                    for (int k = 0; k < 2; k++) {
                        const int off1 = dav1d_cdef_directions[dir + 2][k];
                        const int p0 = tmp[x + off1], p1 = tmp[x - off1];
                        sum += pri_tap_k * constrain(p0 - px, pri_strength, pri_shift);
                        sum += pri_tap_k * constrain(p1 - px, pri_strength, pri_shift);
                        pri_tap_k = (pri_tap_k & 3) | 2;
                        min = umin(p0, min); max = imax(p0, max);
                        min = umin(p1, min); max = imax(p1, max);
                        const int off2 = dav1d_cdef_directions[dir + 4][k];
                        const int off3 = dav1d_cdef_directions[dir + 0][k];
                        const int s0 = tmp[x + off2], s1 = tmp[x - off2];
                        const int s2 = tmp[x + off3], s3 = tmp[x - off3];
                        const int sec_tap = 2 - k;
                        sum += sec_tap * constrain(s0 - px, sec_strength, sec_shift);
                        sum += sec_tap * constrain(s1 - px, sec_strength, sec_shift);
                        sum += sec_tap * constrain(s2 - px, sec_strength, sec_shift);
                        sum += sec_tap * constrain(s3 - px, sec_strength, sec_shift);
                        min = umin(s0, min); max = imax(s0, max);
                        min = umin(s1, min); max = imax(s1, max);
                        min = umin(s2, min); max = imax(s2, max);
                        min = umin(s3, min); max = imax(s3, max);
                    }
                    dst[x] = iclip(px + ((sum - (sum < 0) + 8) >> 4), min, max);
                }
                dst += PXSTRIDE(dst_stride); tmp += tmp_stride;
            } while (--h);
        } else {
            do {
                for (int x = 0; x < w; x++) {
                    const int px = dst[x];
                    int sum = 0, pri_tap_k = pri_tap;
                    for (int k = 0; k < 2; k++) {
                        const int off = dav1d_cdef_directions[dir + 2][k];
                        const int p0 = tmp[x + off], p1 = tmp[x - off];
                        sum += pri_tap_k * constrain(p0 - px, pri_strength, pri_shift);
                        sum += pri_tap_k * constrain(p1 - px, pri_strength, pri_shift);
                        pri_tap_k = (pri_tap_k & 3) | 2;
                    }
                    dst[x] = px + ((sum - (sum < 0) + 8) >> 4);
                }
                dst += PXSTRIDE(dst_stride); tmp += tmp_stride;
            } while (--h);
        }
    } else {
        assert(sec_strength);
        const int sec_shift = damping - ulog2(sec_strength);
        do {
            for (int x = 0; x < w; x++) {
                const int px = dst[x];
                int sum = 0;
                for (int k = 0; k < 2; k++) {
                    const int off1 = dav1d_cdef_directions[dir + 4][k];
                    const int off2 = dav1d_cdef_directions[dir + 0][k];
                    const int s0 = tmp[x + off1], s1 = tmp[x - off1];
                    const int s2 = tmp[x + off2], s3 = tmp[x - off2];
                    const int sec_tap = 2 - k;
                    sum += sec_tap * constrain(s0 - px, sec_strength, sec_shift);
                    sum += sec_tap * constrain(s1 - px, sec_strength, sec_shift);
                    sum += sec_tap * constrain(s2 - px, sec_strength, sec_shift);
                    sum += sec_tap * constrain(s3 - px, sec_strength, sec_shift);
                }
                dst[x] = px + ((sum - (sum < 0) + 8) >> 4);
            }
            dst += PXSTRIDE(dst_stride); tmp += tmp_stride;
        } while (--h);
    }
}

/* Deterministic surrounding canvas pixel: pure function of (x, y) so the Rust
 * test can rebuild the identical scenario without sharing data. */
static pixel canvas_pix(int x, int y) {
    int v = (x * 29 + y * 43 + ((x ^ y) * 53) + 40);
    return (pixel)(v & 0xff);
}

struct Cfg { int w, h, dir, pri, sec, damping; };

int main(void) {
    /* All edges available; the (w+4)×(h+4) canvas surrounds the w×h block at
     * (2,2). dir spans 0..7; pri-only, sec-only and combined are exercised. */
    struct Cfg cfgs[] = {
        { 8, 8, 0, 8, 0, 4 }, { 8, 8, 2, 0, 4, 4 }, { 8, 8, 4, 8, 4, 4 },
        { 8, 8, 6, 15, 8, 5 }, { 8, 8, 1, 4, 2, 3 }, { 8, 8, 7, 12, 4, 4 },
        { 4, 4, 0, 12, 2, 4 }, { 4, 4, 3, 0, 8, 4 }, { 4, 8, 5, 8, 4, 4 },
    };
    int n = (int)(sizeof cfgs / sizeof cfgs[0]);
    printf("// AUTO-GENERATED by tools/extract_av1_cdef/filter_harness.c — dav1d reference.\n");
    printf("pub(super) static CDEF_FILTER_REF: &[CdefCase] = &[\n");
    for (int c = 0; c < n; c++) {
        const int w = cfgs[c].w, h = cfgs[c].h;
        const int cw = w + 4, ch = h + 4;
        pixel canvas[12 * 12];
        for (int y = 0; y < ch; y++) for (int x = 0; x < cw; x++) canvas[y * cw + x] = canvas_pix(x, y);
        pixel *dst = canvas + 2 * cw + 2;
        const pixel *top = canvas + 0 * cw + 2;       // 2 rows above dst
        const pixel *bottom = canvas + (2 + h) * cw + 2;
        pixel left[8][2];
        for (int y = 0; y < h; y++) { left[y][0] = canvas[(2 + y) * cw + 0]; left[y][1] = canvas[(2 + y) * cw + 1]; }
        cdef_filter_block_c(dst, cw, left, top, bottom, cfgs[c].pri, cfgs[c].sec,
                            cfgs[c].dir, cfgs[c].damping,
                            w, h, CDEF_HAVE_LEFT | CDEF_HAVE_RIGHT | CDEF_HAVE_TOP | CDEF_HAVE_BOTTOM);
        printf("    CdefCase { w: %d, h: %d, dir: %d, pri: %d, sec: %d, damping: %d, out: &[",
               w, h, cfgs[c].dir, cfgs[c].pri, cfgs[c].sec, cfgs[c].damping);
        for (int y = 0; y < h; y++) for (int x = 0; x < w; x++)
            printf("%d%s", dst[y * cw + x], (y == h - 1 && x == w - 1) ? "" : ", ");
        printf("] },\n");
    }
    printf("];\n");
    return 0;
}
