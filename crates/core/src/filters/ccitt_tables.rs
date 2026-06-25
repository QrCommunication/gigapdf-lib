// CCITT modified-Huffman (MH) run-length code tables (ITU-T T.4, Tables 1-3).
//
// Each entry is `(code_bits, code_length, run_length)`, with `code_bits` the
// value of the prefix code right-justified in `code_length` bits, read MSB-first
// from the stream. Terminating codes carry runs 0..=63; make-up codes carry runs
// that are multiples of 64 (64..=1728 per colour, plus the shared 1792..=2560
// codes that apply to both colours, in `SHARED_MAKEUP`).
//
// These tables are sorted so that no listed code is a prefix of a later-tried
// code of greater length, but because the decoder peeks an exact bit count for
// each entry and the MH codes form a complete prefix code, ordering only affects
// which equal-length code matches first (and equal-length codes are mutually
// exclusive). The values are transcribed directly from the standard.

/// White run terminating + make-up codes (T.4 Tables 1 & 3, white column).
#[rustfmt::skip]
const WHITE_CODES: &[(u32, u32, u32)] = &[
    // (bits, len, run) — terminating codes (run 0..=63)
    (0x35, 8, 0),   (0x07, 6, 1),   (0x07, 4, 2),   (0x08, 4, 3),
    (0x0B, 4, 4),   (0x0C, 4, 5),   (0x0E, 4, 6),   (0x0F, 4, 7),
    (0x13, 5, 8),   (0x14, 5, 9),   (0x07, 5, 10),  (0x08, 5, 11),
    (0x08, 6, 12),  (0x03, 6, 13),  (0x34, 6, 14),  (0x35, 6, 15),
    (0x2A, 6, 16),  (0x2B, 6, 17),  (0x27, 7, 18),  (0x0C, 7, 19),
    (0x08, 7, 20),  (0x17, 7, 21),  (0x03, 7, 22),  (0x04, 7, 23),
    (0x28, 7, 24),  (0x2B, 7, 25),  (0x13, 7, 26),  (0x24, 7, 27),
    (0x18, 7, 28),  (0x02, 8, 29),  (0x03, 8, 30),  (0x1A, 8, 31),
    (0x1B, 8, 32),  (0x12, 8, 33),  (0x13, 8, 34),  (0x14, 8, 35),
    (0x15, 8, 36),  (0x16, 8, 37),  (0x17, 8, 38),  (0x28, 8, 39),
    (0x29, 8, 40),  (0x2A, 8, 41),  (0x2B, 8, 42),  (0x2C, 8, 43),
    (0x2D, 8, 44),  (0x04, 8, 45),  (0x05, 8, 46),  (0x0A, 8, 47),
    (0x0B, 8, 48),  (0x52, 8, 49),  (0x53, 8, 50),  (0x54, 8, 51),
    (0x55, 8, 52),  (0x24, 8, 53),  (0x25, 8, 54),  (0x58, 8, 55),
    (0x59, 8, 56),  (0x5A, 8, 57),  (0x5B, 8, 58),  (0x4A, 8, 59),
    (0x4B, 8, 60),  (0x32, 8, 61),  (0x33, 8, 62),  (0x34, 8, 63),
    // make-up codes (run 64..=1728, multiples of 64), white column
    (0x1B, 5, 64),   (0x12, 5, 128),  (0x17, 6, 192),  (0x37, 7, 256),
    (0x36, 8, 320),  (0x37, 8, 384),  (0x64, 8, 448),  (0x65, 8, 512),
    (0x68, 8, 576),  (0x67, 8, 640),  (0xCC, 9, 704),  (0xCD, 9, 768),
    (0xD2, 9, 832),  (0xD3, 9, 896),  (0xD4, 9, 960),  (0xD5, 9, 1024),
    (0xD6, 9, 1088), (0xD7, 9, 1152), (0xD8, 9, 1216), (0xD9, 9, 1280),
    (0xDA, 9, 1344), (0xDB, 9, 1408), (0x98, 9, 1472), (0x99, 9, 1536),
    (0x9A, 9, 1600), (0x18, 6, 1664), (0x9B, 9, 1728),
];

/// Black run terminating + make-up codes (T.4 Tables 2 & 3, black column).
#[rustfmt::skip]
const BLACK_CODES: &[(u32, u32, u32)] = &[
    // terminating codes (run 0..=63)
    (0x37, 10, 0),  (0x02, 3, 1),   (0x03, 2, 2),   (0x02, 2, 3),
    (0x03, 3, 4),   (0x03, 4, 5),   (0x02, 4, 6),   (0x03, 5, 7),
    (0x05, 6, 8),   (0x04, 6, 9),   (0x04, 7, 10),  (0x05, 7, 11),
    (0x07, 7, 12),  (0x04, 8, 13),  (0x07, 8, 14),  (0x18, 9, 15),
    (0x17, 10, 16), (0x18, 10, 17), (0x08, 10, 18), (0x67, 11, 19),
    (0x68, 11, 20), (0x6C, 11, 21), (0x37, 11, 22), (0x28, 11, 23),
    (0x17, 11, 24), (0x18, 11, 25), (0xCA, 12, 26), (0xCB, 12, 27),
    (0xCC, 12, 28), (0xCD, 12, 29), (0x68, 12, 30), (0x69, 12, 31),
    (0x6A, 12, 32), (0x6B, 12, 33), (0xD2, 12, 34), (0xD3, 12, 35),
    (0xD4, 12, 36), (0xD5, 12, 37), (0xD6, 12, 38), (0xD7, 12, 39),
    (0x6C, 12, 40), (0x6D, 12, 41), (0xDA, 12, 42), (0xDB, 12, 43),
    (0x54, 12, 44), (0x55, 12, 45), (0x56, 12, 46), (0x57, 12, 47),
    (0x64, 12, 48), (0x65, 12, 49), (0x52, 12, 50), (0x53, 12, 51),
    (0x24, 12, 52), (0x37, 12, 53), (0x38, 12, 54), (0x27, 12, 55),
    (0x28, 12, 56), (0x58, 12, 57), (0x59, 12, 58), (0x2B, 12, 59),
    (0x2C, 12, 60), (0x5A, 12, 61), (0x66, 12, 62), (0x67, 12, 63),
    // make-up codes (run 64..=1728, multiples of 64), black column
    (0x0F, 10, 64),   (0xC8, 12, 128),  (0xC9, 12, 192),  (0x5B, 12, 256),
    (0x33, 12, 320),  (0x34, 12, 384),  (0x35, 12, 448),  (0x6C, 13, 512),
    (0x6D, 13, 576),  (0x4A, 13, 640),  (0x4B, 13, 704),  (0x4C, 13, 768),
    (0x4D, 13, 832),  (0x72, 13, 896),  (0x73, 13, 960),  (0x74, 13, 1024),
    (0x75, 13, 1088), (0x76, 13, 1152), (0x77, 13, 1216), (0x52, 13, 1280),
    (0x53, 13, 1344), (0x54, 13, 1408), (0x55, 13, 1472), (0x5A, 13, 1536),
    (0x5B, 13, 1600), (0x64, 13, 1664), (0x65, 13, 1728),
];

/// Shared (colour-independent) make-up codes for runs 1792..=2560 (T.4 Table 3,
/// "Make-up codes" common section). These apply to both white and black runs.
#[rustfmt::skip]
const SHARED_MAKEUP: &[(u32, u32, u32)] = &[
    (0x08, 11, 1792), (0x0C, 11, 1856), (0x0D, 11, 1920), (0x12, 12, 1984),
    (0x13, 12, 2048), (0x14, 12, 2112), (0x15, 12, 2176), (0x16, 12, 2240),
    (0x17, 12, 2304), (0x1C, 12, 2368), (0x1D, 12, 2432), (0x1E, 12, 2496),
    (0x1F, 12, 2560),
];
