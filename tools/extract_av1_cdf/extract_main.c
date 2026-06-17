/* EMIT/DECL: print a C array field as a Rust nested array literal. */
#define EMIT1(e, n0) do { printf("["); for (int d = 0; d < (int)(n0); d++) printf("%u,", (unsigned)(e)[d]); printf("]"); } while (0)
#define EMIT2(e, n0, n1) do { printf("["); for (int c = 0; c < (int)(n0); c++) { EMIT1((e)[c], n1); printf(","); } printf("]"); } while (0)
#define EMIT3(e, n0, n1, n2) do { printf("["); for (int b = 0; b < (int)(n0); b++) { EMIT2((e)[b], n1, n2); printf(","); } printf("]"); } while (0)
#define EMIT4(e, n0, n1, n2, n3) do { printf("["); for (int a = 0; a < (int)(n0); a++) { EMIT3((e)[a], n1, n2, n3); printf(","); } printf("]"); } while (0)

#define DECL1(rn, e, n0) do { printf("pub(crate) static %s: [u16; %d] = ", rn, (int)(n0)); EMIT1(e, n0); printf(";\n"); } while (0)
#define DECL2(rn, e, n0, n1) do { printf("pub(crate) static %s: [[u16; %d]; %d] = ", rn, (int)(n1), (int)(n0)); EMIT2(e, n0, n1); printf(";\n"); } while (0)
#define DECL3(rn, e, n0, n1, n2) do { printf("pub(crate) static %s: [[[u16; %d]; %d]; %d] = ", rn, (int)(n2), (int)(n1), (int)(n0)); EMIT3(e, n0, n1, n2); printf(";\n"); } while (0)
#define DECL4(rn, e, n0, n1, n2, n3) do { printf("pub(crate) static %s: [[[[u16; %d]; %d]; %d]; %d] = ", rn, (int)(n3), (int)(n2), (int)(n1), (int)(n0)); EMIT4(e, n0, n1, n2, n3); printf(";\n"); } while (0)

/* Coef CDFs are indexed by qcat (4 sets). Wrap each field in an outer [4]. */
#define DECLQ2(rn, f, n0, n1) do { printf("pub(crate) static %s: [[[u16; %d]; %d]; 4] = [", rn, (int)(n1), (int)(n0)); for (int q = 0; q < 4; q++) { EMIT2(default_coef_cdf[q].f, n0, n1); printf(","); } printf("];\n"); } while (0)
#define DECLQ3(rn, f, n0, n1, n2) do { printf("pub(crate) static %s: [[[[u16; %d]; %d]; %d]; 4] = [", rn, (int)(n2), (int)(n1), (int)(n0)); for (int q = 0; q < 4; q++) { EMIT3(default_coef_cdf[q].f, n0, n1, n2); printf(","); } printf("];\n"); } while (0)
#define DECLQ4(rn, f, n0, n1, n2, n3) do { printf("pub(crate) static %s: [[[[[u16; %d]; %d]; %d]; %d]; 4] = [", rn, (int)(n3), (int)(n2), (int)(n1), (int)(n0)); for (int q = 0; q < 4; q++) { EMIT4(default_coef_cdf[q].f, n0, n1, n2, n3); printf(","); } printf("];\n"); } while (0)

int main(void) {
    printf("//! AUTO-GENERATED from dav1d `src/cdf.c` (BSD-2-Clause, (c) VideoLAN /\n");
    printf("//! Two Orioles). Default AV1 CDF tables, inverse Q15 (`32768 - cdf`) with a\n");
    printf("//! trailing adaptation counter + SIMD padding per dav1d's layout. DO NOT EDIT\n");
    printf("//! by hand; regenerate via tools/extract_av1_cdf (compile-and-dump).\n");
    printf("#![allow(dead_code)]\n");
    printf("#![allow(clippy::all)]\n\n");
    printf("// qcat selector: q = (q>20) + (q>60) + (q>120). Index the *_Q tables by it.\n\n");

    /* ── Mode context (intra-keyframe subset) ── */
    DECL3("PARTITION", default_cdf.m.partition, N_BL_LEVELS, 4, N_PARTITIONS + 6);
    DECL3("KF_Y_MODE", default_cdf.kfym, 5, 5, N_INTRA_PRED_MODES + 3);
    DECL3("UV_MODE", default_cdf.m.uv_mode, 2, N_INTRA_PRED_MODES, N_UV_INTRA_PRED_MODES + 2);
    DECL2("ANGLE_DELTA", default_cdf.m.angle_delta, 8, 8);
    DECL1("FILTER_INTRA_MODE", default_cdf.m.filter_intra, 5 + 3);
    DECL2("USE_FILTER_INTRA", default_cdf.m.use_filter_intra, N_BS_SIZES, 2);
    DECL3("TX_SIZE", default_cdf.m.txsz, N_TX_SIZES - 1, 3, 4);
    DECL3("TXTP_INTRA1", default_cdf.m.txtp_intra1, 2, N_INTRA_PRED_MODES, 7 + 1);
    DECL3("TXTP_INTRA2", default_cdf.m.txtp_intra2, 3, N_INTRA_PRED_MODES, 5 + 3);
    DECL1("CFL_SIGN", default_cdf.m.cfl_sign, 8);
    DECL2("CFL_ALPHA", default_cdf.m.cfl_alpha, 6, 16);
    DECL2("SKIP", default_cdf.m.skip, 3, 2);
    DECL1("DELTA_Q", default_cdf.m.delta_q, 4);
    DECL2("DELTA_LF", default_cdf.m.delta_lf, 5, 4);
    DECL1("INTRABC", default_cdf.m.intrabc, 2);
    DECL3("PAL_Y_MODE", default_cdf.m.pal_y, 7, 3, 2);
    DECL2("PAL_UV_MODE", default_cdf.m.pal_uv, 2, 2);
    DECL3("PAL_SZ", default_cdf.m.pal_sz, 2, 7, 7 + 1);
    DECL4("COLOR_MAP", default_cdf.m.color_map, 2, 7, 5, 8);

    /* ── Coefficient context (per-qcat, all sets) ── */
    DECLQ3("COEF_SKIP_Q", skip, N_TX_SIZES, 13, 2);
    DECLQ3("DC_SIGN_Q", dc_sign, 2, 3, 2);
    DECLQ3("EOB_BIN_16_Q", eob_bin_16, 2, 2, 5 + 3);
    DECLQ3("EOB_BIN_32_Q", eob_bin_32, 2, 2, 6 + 2);
    DECLQ3("EOB_BIN_64_Q", eob_bin_64, 2, 2, 7 + 1);
    DECLQ3("EOB_BIN_128_Q", eob_bin_128, 2, 2, 8 + 0);
    DECLQ3("EOB_BIN_256_Q", eob_bin_256, 2, 2, 9 + 7);
    DECLQ2("EOB_BIN_512_Q", eob_bin_512, 2, 10 + 6);
    DECLQ2("EOB_BIN_1024_Q", eob_bin_1024, 2, 11 + 5);
    DECLQ4("EOB_HI_BIT_Q", eob_hi_bit, N_TX_SIZES, 2, 9, 2);
    DECLQ4("EOB_BASE_TOK_Q", eob_base_tok, N_TX_SIZES, 2, 4, 4);
    DECLQ4("COEFF_BASE_Q", base_tok, N_TX_SIZES, 2, 41, 4);
    DECLQ4("COEFF_BR_Q", br_tok, 4, 2, 21, 4);

    return 0;
}
