#define F(idx, f0, f1, f2, f3, f4, f5, f6) [1*idx+0]=f0,[1*idx+8]=f1,[1*idx+16]=f2,[1*idx+24]=f3,[1*idx+32]=f4,[1*idx+40]=f5,[1*idx+48]=f6
static const int8_t dav1d_filter_intra_taps[5][64] = {
    {
        F( 0,  -6, 10,  0,  0,  0, 12,  0 ),
        F( 1,  -5,  2, 10,  0,  0,  9,  0 ),
        F( 2,  -3,  1,  1, 10,  0,  7,  0 ),
        F( 3,  -3,  1,  1,  2, 10,  5,  0 ),
        F( 4,  -4,  6,  0,  0,  0,  2, 12 ),
        F( 5,  -3,  2,  6,  0,  0,  2,  9 ),
        F( 6,  -3,  2,  2,  6,  0,  2,  7 ),
        F( 7,  -3,  1,  2,  2,  6,  3,  5 ),
    }, {
        F( 0, -10, 16,  0,  0,  0, 10,  0 ),
        F( 1,  -6,  0, 16,  0,  0,  6,  0 ),
        F( 2,  -4,  0,  0, 16,  0,  4,  0 ),
        F( 3,  -2,  0,  0,  0, 16,  2,  0 ),
        F( 4, -10, 16,  0,  0,  0,  0, 10 ),
        F( 5,  -6,  0, 16,  0,  0,  0,  6 ),
        F( 6,  -4,  0,  0, 16,  0,  0,  4 ),
        F( 7,  -2,  0,  0,  0, 16,  0,  2 ),
    }, {
        F( 0,  -8,  8,  0,  0,  0, 16,  0 ),
        F( 1,  -8,  0,  8,  0,  0, 16,  0 ),
        F( 2,  -8,  0,  0,  8,  0, 16,  0 ),
        F( 3,  -8,  0,  0,  0,  8, 16,  0 ),
        F( 4,  -4,  4,  0,  0,  0,  0, 16 ),
        F( 5,  -4,  0,  4,  0,  0,  0, 16 ),
        F( 6,  -4,  0,  0,  4,  0,  0, 16 ),
        F( 7,  -4,  0,  0,  0,  4,  0, 16 ),
    }, {
        F( 0,  -2,  8,  0,  0,  0, 10,  0 ),
        F( 1,  -1,  3,  8,  0,  0,  6,  0 ),
        F( 2,  -1,  2,  3,  8,  0,  4,  0 ),
        F( 3,   0,  1,  2,  3,  8,  2,  0 ),
        F( 4,  -1,  4,  0,  0,  0,  3, 10 ),
        F( 5,  -1,  3,  4,  0,  0,  4,  6 ),
        F( 6,  -1,  2,  3,  4,  0,  4,  4 ),
        F( 7,  -1,  2,  2,  3,  4,  3,  3 ),
    }, {
        F( 0, -12, 14,  0,  0,  0, 14,  0 ),
        F( 1, -10,  0, 14,  0,  0, 12,  0 ),
        F( 2,  -9,  0,  0, 14,  0, 11,  0 ),
        F( 3,  -8,  0,  0,  0, 14, 10,  0 ),
        F( 4, -10, 12,  0,  0,  0,  0, 14 ),
        F( 5,  -9,  1, 12,  0,  0,  0, 12 ),
        F( 6,  -8,  0,  0, 12,  0,  1, 11 ),
        F( 7,  -7,  0,  0,  1, 12,  1,  9 ),
    }
};
#define FLT_INCR 1
#define FILTER(flt_ptr, p0,p1,p2,p3,p4,p5,p6) (flt_ptr[0]*p0 + flt_ptr[8]*p1 + flt_ptr[16]*p2 + flt_ptr[24]*p3 + flt_ptr[32]*p4 + flt_ptr[40]*p5 + flt_ptr[48]*p6)
static void ipred_filter_c(pixel *dst, const ptrdiff_t stride,
                           const pixel *const topleft_in,
                           const int width, const int height, int filt_idx,
                           const int max_width, const int max_height
                           HIGHBD_DECL_SUFFIX)
{
    filt_idx &= 511;
    assert(filt_idx < 5);

    const int8_t *const filter = dav1d_filter_intra_taps[filt_idx];
    const pixel *top = &topleft_in[1];
    for (int y = 0; y < height; y += 2) {
        const pixel *topleft = &topleft_in[-y];
        const pixel *left = &topleft[-1];
        ptrdiff_t left_stride = -1;
        for (int x = 0; x < width; x += 4) {
            const int p0 = *topleft;
            const int p1 = top[0], p2 = top[1], p3 = top[2], p4 = top[3];
            const int p5 = left[0 * left_stride], p6 = left[1 * left_stride];
            pixel *ptr = &dst[x];
            const int8_t *flt_ptr = filter;

            for (int yy = 0; yy < 2; yy++) {
                for (int xx = 0; xx < 4; xx++, flt_ptr += FLT_INCR) {
                    const int acc = FILTER(flt_ptr, p0, p1, p2, p3, p4, p5, p6);
                    ptr[xx] = iclip_pixel((acc + 8) >> 4);
                }
                ptr += PXSTRIDE(stride);
            }
            left = &dst[x + 4 - 1];
            left_stride = PXSTRIDE(stride);
            top += 4;
            topleft = &top[-1];
        }
        top = &dst[PXSTRIDE(stride)];
        dst = &dst[PXSTRIDE(stride) * 2];
    }
}
