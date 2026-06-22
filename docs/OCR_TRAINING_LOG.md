# OCR engine history — the retired CRNN/CTC engine

> This is an **archived summary**. The detailed per-run CRNN benchmark tables that used to live here
> described an engine that **no longer exists** and were removed to avoid presenting them as current.
> The OCR engine is now **PaddleOCR PP-OCR on RTen** — see [`OCR_ARCHITECTURE.md`](OCR_ARCHITECTURE.md)
> and [`../crates/ocr-rten/README.md`](../crates/ocr-rten/README.md).

## What it was

A from-scratch, zero-dependency line recognizer embedded in the pure-`std` core: Otsu/Sauvola
binarization → projection-profile line bands → CNN → bidirectional GRU → CTC, run as a hand-written
**int8** forward pass (no ML runtime). Per-script models (`alpha` Latin/Cyrillic/Greek, `arabic`
RTL, Devanagari, Bengali, Tamil, CJK, …) were trained offline in PyTorch (`tools/train_ocr_crnn.py`),
quantized, and emitted as `.gpocr` blobs host-loaded like fonts. It beat Tesseract on several
**synthetic clean-print** benchmarks (Latin/Cyrillic/Greek, Arabic, Tamil, Devanagari) and was the
first dependency-free engine to beat Tesseract on real handwriting (IAM CER 0.309 vs 0.353).

## Why it was retired

1. **int8 collapse on recurrent non-Latin recognizers.** Host-loaded `.gpocr` weights were
   per-tensor int8-quantized while only the float net was validated. For GRU recognizers the ~10%
   per-tensor rounding error compounds over a line → decode collapsed to ASCII garbage in production
   despite a good float validation CER. (A float `GPO2` format fixed fidelity, but…)
2. **Capacity ceiling on complex scripts.** The small CPU/int8 models plateaued and **lost to
   Tesseract** on dense conjunct scripts (Kannada 0.50 vs 0.05, Gujarati 0.75 vs 0.07, Telugu/Thai
   similar). Matching Tesseract's breadth would have needed far larger models + far more data +
   per-script training — months of effort to reach what PaddleOCR already ships.
3. **The pragmatic call.** Reuse **pre-trained PaddleOCR** (state of the art, 100+ scripts) via
   **RTen** (pure-Rust ONNX, no C++) instead of training our own. Same "no Tesseract, pure-Rust"
   spirit, vastly better accuracy, zero training — except **Hebrew** (PaddleOCR ships none), which we
   still train (small alphabet, easy): [`OCR_TRAINING_DATA.md`](OCR_TRAINING_DATA.md).

## Lessons carried forward

- Validate the **quantized/exported** model, not just the float net — a great float CER can hide a
  broken deployed model.
- For a small team, **reusing strong pre-trained models beats training breadth from scratch**;
  train only the gaps (e.g. Hebrew).
- Keep the heavy ML engine **host-side**; the lean pure-`std` core/WASM stays dependency-free.
