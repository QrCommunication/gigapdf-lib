
static NOINLINE void
splat_dc(pixel *dst, const ptrdiff_t stride,
         const int width, const int height, const int dc HIGHBD_DECL_SUFFIX)
{
#if BITDEPTH == 8
    assert(dc <= 0xff);
    if (width > 4) {
        const uint64_t dcN = dc * 0x0101010101010101ULL;
        for (int y = 0; y < height; y++) {
            for (int x = 0; x < width; x += sizeof(dcN))
                *((uint64_t *) &dst[x]) = dcN;
            dst += PXSTRIDE(stride);
        }
    } else {
        const unsigned dcN = dc * 0x01010101U;
        for (int y = 0; y < height; y++) {
            for (int x = 0; x < width; x += sizeof(dcN))
                *((unsigned *) &dst[x]) = dcN;
            dst += PXSTRIDE(stride);
        }
    }
#else
    assert(dc <= bitdepth_max);
    const uint64_t dcN = dc * 0x0001000100010001ULL;
    for (int y = 0; y < height; y++) {
        for (int x = 0; x < width; x += sizeof(dcN) >> 1)
            *((uint64_t *) &dst[x]) = dcN;
        dst += PXSTRIDE(stride);
    }
#endif
}

static NOINLINE void
cfl_pred(pixel *dst, const ptrdiff_t stride,
         const int width, const int height, const int dc,
         const int16_t *ac, const int alpha HIGHBD_DECL_SUFFIX)
{
    for (int y = 0; y < height; y++) {
        for (int x = 0; x < width; x++) {
            const int diff = alpha * ac[x];
            dst[x] = iclip_pixel(dc + apply_sign((abs(diff) + 32) >> 6, diff));
        }
        ac += width;
        dst += PXSTRIDE(stride);
    }
}

static unsigned dc_gen_top(const pixel *const topleft, const int width) {
    unsigned dc = width >> 1;
    for (int i = 0; i < width; i++)
       dc += topleft[1 + i];
    return dc >> ctz(width);
}

static void ipred_dc_top_c(pixel *dst, const ptrdiff_t stride,
                           const pixel *const topleft,
                           const int width, const int height, const int a,
                           const int max_width, const int max_height
                           HIGHBD_DECL_SUFFIX)
{
    splat_dc(dst, stride, width, height, dc_gen_top(topleft, width)
             HIGHBD_TAIL_SUFFIX);
}

static void ipred_cfl_top_c(pixel *dst, const ptrdiff_t stride,
                            const pixel *const topleft,
                            const int width, const int height,
                            const int16_t *ac, const int alpha
                            HIGHBD_DECL_SUFFIX)
{
    cfl_pred(dst, stride, width, height, dc_gen_top(topleft, width), ac, alpha
             HIGHBD_TAIL_SUFFIX);
}

static unsigned dc_gen_left(const pixel *const topleft, const int height) {
    unsigned dc = height >> 1;
    for (int i = 0; i < height; i++)
       dc += topleft[-(1 + i)];
    return dc >> ctz(height);
}

static void ipred_dc_left_c(pixel *dst, const ptrdiff_t stride,
                            const pixel *const topleft,
                            const int width, const int height, const int a,
                            const int max_width, const int max_height
                            HIGHBD_DECL_SUFFIX)
{
    splat_dc(dst, stride, width, height, dc_gen_left(topleft, height)
             HIGHBD_TAIL_SUFFIX);
}

static void ipred_cfl_left_c(pixel *dst, const ptrdiff_t stride,
                             const pixel *const topleft,
                             const int width, const int height,
                             const int16_t *ac, const int alpha
                             HIGHBD_DECL_SUFFIX)
{
    const unsigned dc = dc_gen_left(topleft, height);
    cfl_pred(dst, stride, width, height, dc, ac, alpha HIGHBD_TAIL_SUFFIX);
}

#if BITDEPTH == 8
#define MULTIPLIER_1x2 0x5556
#define MULTIPLIER_1x4 0x3334
#define BASE_SHIFT 16
#else
#define MULTIPLIER_1x2 0xAAAB
#define MULTIPLIER_1x4 0x6667
#define BASE_SHIFT 17
#endif

static unsigned dc_gen(const pixel *const topleft,
                       const int width, const int height)
{
    unsigned dc = (width + height) >> 1;
    for (int i = 0; i < width; i++)
       dc += topleft[i + 1];
    for (int i = 0; i < height; i++)
       dc += topleft[-(i + 1)];
    dc >>= ctz(width + height);

    if (width != height) {
        dc *= (width > height * 2 || height > width * 2) ? MULTIPLIER_1x4 :
                                                           MULTIPLIER_1x2;
        dc >>= BASE_SHIFT;
    }
    return dc;
}

static void ipred_dc_c(pixel *dst, const ptrdiff_t stride,
                       const pixel *const topleft,
                       const int width, const int height, const int a,
                       const int max_width, const int max_height
                       HIGHBD_DECL_SUFFIX)
{
    splat_dc(dst, stride, width, height, dc_gen(topleft, width, height)
             HIGHBD_TAIL_SUFFIX);
}

static void ipred_cfl_c(pixel *dst, const ptrdiff_t stride,
                        const pixel *const topleft,
                        const int width, const int height,
                        const int16_t *ac, const int alpha
                        HIGHBD_DECL_SUFFIX)
{
    unsigned dc = dc_gen(topleft, width, height);
    cfl_pred(dst, stride, width, height, dc, ac, alpha HIGHBD_TAIL_SUFFIX);
}

#undef MULTIPLIER_1x2
#undef MULTIPLIER_1x4
#undef BASE_SHIFT

static void ipred_dc_128_c(pixel *dst, const ptrdiff_t stride,
                           const pixel *const topleft,
                           const int width, const int height, const int a,
                           const int max_width, const int max_height
                           HIGHBD_DECL_SUFFIX)
{
#if BITDEPTH == 16
    const int dc = (bitdepth_max + 1) >> 1;
#else
    const int dc = 128;
#endif
    splat_dc(dst, stride, width, height, dc HIGHBD_TAIL_SUFFIX);
}

static void ipred_cfl_128_c(pixel *dst, const ptrdiff_t stride,
                            const pixel *const topleft,
                            const int width, const int height,
                            const int16_t *ac, const int alpha
                            HIGHBD_DECL_SUFFIX)
{
#if BITDEPTH == 16
    const int dc = (bitdepth_max + 1) >> 1;
#else
    const int dc = 128;
#endif
    cfl_pred(dst, stride, width, height, dc, ac, alpha HIGHBD_TAIL_SUFFIX);
}

static void ipred_v_c(pixel *dst, const ptrdiff_t stride,
                      const pixel *const topleft,
                      const int width, const int height, const int a,
                      const int max_width, const int max_height
                      HIGHBD_DECL_SUFFIX)
{
    for (int y = 0; y < height; y++) {
        pixel_copy(dst, topleft + 1, width);
        dst += PXSTRIDE(stride);
    }
}

static void ipred_h_c(pixel *dst, const ptrdiff_t stride,
                      const pixel *const topleft,
                      const int width, const int height, const int a,
                      const int max_width, const int max_height
                      HIGHBD_DECL_SUFFIX)
{
    for (int y = 0; y < height; y++) {
        pixel_set(dst, topleft[-(1 + y)], width);
        dst += PXSTRIDE(stride);
    }
}

static void ipred_paeth_c(pixel *dst, const ptrdiff_t stride,
                          const pixel *const tl_ptr,
                          const int width, const int height, const int a,
                          const int max_width, const int max_height
                          HIGHBD_DECL_SUFFIX)
{
    const int topleft = tl_ptr[0];
    for (int y = 0; y < height; y++) {
        const int left = tl_ptr[-(y + 1)];
        for (int x = 0; x < width; x++) {
            const int top = tl_ptr[1 + x];
            const int base = left + top - topleft;
            const int ldiff = abs(left - base);
            const int tdiff = abs(top - base);
            const int tldiff = abs(topleft - base);

            dst[x] = ldiff <= tdiff && ldiff <= tldiff ? left :
                     tdiff <= tldiff ? top : topleft;
        }
        dst += PXSTRIDE(stride);
    }
}

static void ipred_smooth_c(pixel *dst, const ptrdiff_t stride,
                           const pixel *const topleft,
                           const int width, const int height, const int a,
                           const int max_width, const int max_height
                           HIGHBD_DECL_SUFFIX)
{
    const uint8_t *const weights_hor = &dav1d_sm_weights[width];
    const uint8_t *const weights_ver = &dav1d_sm_weights[height];
    const int right = topleft[width], bottom = topleft[-height];

    for (int y = 0; y < height; y++) {
        for (int x = 0; x < width; x++) {
            const int pred = weights_ver[y]  * topleft[1 + x] +
                      (256 - weights_ver[y]) * bottom +
                             weights_hor[x]  * topleft[-(1 + y)] +
                      (256 - weights_hor[x]) * right;
            dst[x] = (pred + 256) >> 9;
        }
        dst += PXSTRIDE(stride);
    }
}

static void ipred_smooth_v_c(pixel *dst, const ptrdiff_t stride,
                             const pixel *const topleft,
                             const int width, const int height, const int a,
                             const int max_width, const int max_height
                             HIGHBD_DECL_SUFFIX)
{
    const uint8_t *const weights_ver = &dav1d_sm_weights[height];
    const int bottom = topleft[-height];

    for (int y = 0; y < height; y++) {
        for (int x = 0; x < width; x++) {
            const int pred = weights_ver[y]  * topleft[1 + x] +
                      (256 - weights_ver[y]) * bottom;
            dst[x] = (pred + 128) >> 8;
        }
        dst += PXSTRIDE(stride);
    }
}

static void ipred_smooth_h_c(pixel *dst, const ptrdiff_t stride,
                             const pixel *const topleft,
                             const int width, const int height, const int a,
                             const int max_width, const int max_height
                             HIGHBD_DECL_SUFFIX)
{
    const uint8_t *const weights_hor = &dav1d_sm_weights[width];
    const int right = topleft[width];

    for (int y = 0; y < height; y++) {
        for (int x = 0; x < width; x++) {
            const int pred = weights_hor[x]  * topleft[-(y + 1)] +
                      (256 - weights_hor[x]) * right;
            dst[x] = (pred + 128) >> 8;
        }
        dst += PXSTRIDE(stride);
    }
}

static NOINLINE int get_filter_strength(const int wh, const int angle,
                                        const int is_sm)
{
    if (is_sm) {
        if (wh <= 8) {
            if (angle >= 64) return 2;
            if (angle >= 40) return 1;
        } else if (wh <= 16) {
            if (angle >= 48) return 2;
            if (angle >= 20) return 1;
        } else if (wh <= 24) {
            if (angle >=  4) return 3;
        } else {
            return 3;
        }
    } else {
        if (wh <= 8) {
            if (angle >= 56) return 1;
        } else if (wh <= 16) {
            if (angle >= 40) return 1;
        } else if (wh <= 24) {
            if (angle >= 32) return 3;
            if (angle >= 16) return 2;
            if (angle >=  8) return 1;
        } else if (wh <= 32) {
            if (angle >= 32) return 3;
            if (angle >=  4) return 2;
            return 1;
        } else {
            return 3;
        }
    }
    return 0;
}

static NOINLINE void filter_edge(pixel *const out, const int sz,
                                 const int lim_from, const int lim_to,
                                 const pixel *const in, const int from,
                                 const int to, const int strength)
{
    static const uint8_t kernel[3][5] = {
        { 0, 4, 8, 4, 0 },
        { 0, 5, 6, 5, 0 },
        { 2, 4, 4, 4, 2 }
    };

    assert(strength > 0);
    int i = 0;
    for (; i < imin(sz, lim_from); i++)
        out[i] = in[iclip(i, from, to - 1)];
    for (; i < imin(lim_to, sz); i++) {
        int s = 0;
        for (int j = 0; j < 5; j++)
            s += in[iclip(i - 2 + j, from, to - 1)] * kernel[strength - 1][j];
        out[i] = (s + 8) >> 4;
    }
    for (; i < sz; i++)
        out[i] = in[iclip(i, from, to - 1)];
}

static inline int get_upsample(const int wh, const int angle, const int is_sm) {
    return angle < 40 && wh <= 16 >> is_sm;
}

static NOINLINE void upsample_edge(pixel *const out, const int hsz,
                                   const pixel *const in, const int from,
                                   const int to HIGHBD_DECL_SUFFIX)
{
    static const int8_t kernel[4] = { -1, 9, 9, -1 };
    int i;
    for (i = 0; i < hsz - 1; i++) {
        out[i * 2] = in[iclip(i, from, to - 1)];

        int s = 0;
        for (int j = 0; j < 4; j++)
            s += in[iclip(i + j - 1, from, to - 1)] * kernel[j];
        out[i * 2 + 1] = iclip_pixel((s + 8) >> 4);
    }
    out[i * 2] = in[iclip(i, from, to - 1)];
}

static void ipred_z1_c(pixel *dst, const ptrdiff_t stride,
                       const pixel *const topleft_in,
                       const int width, const int height, int angle,
                       const int max_width, const int max_height
                       HIGHBD_DECL_SUFFIX)
{
    const int is_sm = (angle >> 9) & 0x1;
    const int enable_intra_edge_filter = angle >> 10;
    angle &= 511;
    assert(angle < 90);
    int dx = dav1d_dr_intra_derivative[angle >> 1];
    pixel top_out[64 + 64];
    const pixel *top;
    int max_base_x;
    const int upsample_above = enable_intra_edge_filter ?
        get_upsample(width + height, 90 - angle, is_sm) : 0;
    if (upsample_above) {
        upsample_edge(top_out, width + height, &topleft_in[1], -1,
                      width + imin(width, height) HIGHBD_TAIL_SUFFIX);
        top = top_out;
        max_base_x = 2 * (width + height) - 2;
        dx <<= 1;
    } else {
        const int filter_strength = enable_intra_edge_filter ?
            get_filter_strength(width + height, 90 - angle, is_sm) : 0;
        if (filter_strength) {
            filter_edge(top_out, width + height, 0, width + height,
                        &topleft_in[1], -1, width + imin(width, height),
                        filter_strength);
            top = top_out;
            max_base_x = width + height - 1;
        } else {
            top = &topleft_in[1];
            max_base_x = width + imin(width, height) - 1;
        }
    }
    const int base_inc = 1 + upsample_above;
    for (int y = 0, xpos = dx; y < height;
         y++, dst += PXSTRIDE(stride), xpos += dx)
    {
        const int frac = xpos & 0x3E;

        for (int x = 0, base = xpos >> 6; x < width; x++, base += base_inc) {
            if (base < max_base_x) {
                const int v = top[base] * (64 - frac) + top[base + 1] * frac;
                dst[x] = (v + 32) >> 6;
            } else {
                pixel_set(&dst[x], top[max_base_x], width - x);
                break;
            }
        }
    }
}

