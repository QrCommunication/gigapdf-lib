
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
