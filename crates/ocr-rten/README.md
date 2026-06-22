# gigapdf-ocr-rten — server-side OCR via PaddleOCR + RTen (pure-Rust)

State-of-the-art multilingual OCR using **PaddleOCR PP-OCR** models run through
**RTen** (pure-Rust ONNX engine, no C++ dependency — the engine behind `ocrs`).
Replaces the legacy hand-trained CRNN. Heavy ML deps live here, host-side; the
lean pure-std `core`/`wasm` crates stay dependency-free and call this via an endpoint.

See [`../../docs/OCR_ARCHITECTURE.md`](../../docs/OCR_ARCHITECTURE.md) for the full design (pipeline,
input profiles, CTC, RTL, auto script selection, handwriting opt-in, API).

## Models (not committed — fetched/converted at deploy)
Run **`tools/fetch_models.sh [out_dir]`** to download the shared **DBNet detector** + 12 PaddleOCR
recognizers from `deepghs/paddleocr` (Hugging Face) and convert each ONNX → `.rten`
(`pip install rten-convert`). Two models we provide ourselves:

- **Hebrew** (`hebrew/`) — our trained CRNN (`tools/train_hebrew.py` → ONNX → `rten-convert`);
  PaddleOCR ships none.
- **Handwriting** (`latin_hw/`) — the **reused** legacy CRNN, re-exported from `ocr_alpha_hw.gpocr`
  via `tools/convert_legacy_gpocr.py` (no retraining; beat Tesseract on IAM 0.309). Grayscale H32
  `LegacyGray32` profile, **opt-in** via `recognize_page_with(img, "latin_hw")`.

PaddleOCR PP-OCRv4/v5 covers 100+ scripts — add one by dropping `<subdir>/{model.rten,dict.txt}` in
the models dir + an entry in `REC_MODELS` (`src/lib.rs`).

## Validated
- `rec_probe`: Chinese line `深度学习模型测试2026` decoded **100% correct** (conf 0.999).
- `ocr_auto` (det + auto script selection, 13 printed recognizers): KR→`ko`, JA→`ja`, FR→`zh`,
  RU→`cyrillic` — all routed correctly; Korean & Latin perfect, Cyrillic ~90%.
- Reused HW model (`validate_legacy_hw.py`): reads `Bonjour le monde` / `facture 2026` 100%.

## Probes / tools
`rec_probe` (single rec), `ocr_probe` (det+rec, one model), `ocr_auto` (load all + auto-select);
`tools/{fetch_models,train_hebrew,convert_legacy_gpocr,validate_legacy_hw}`.
