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

- **Hebrew** (`hebrew/`) — our trained CRNN; PaddleOCR ships none. Pre-trained weights on Hugging Face:
  **[`ronylicha/gigapdf-ocr-hebrew`](https://huggingface.co/ronylicha/gigapdf-ocr-hebrew)** (`model.rten`
  + `dict.txt`), or retrain with `tools/train_hebrew.py` → ONNX → `rten-convert`.
- **Handwriting** (`latin_hw/`) — our trained CRNN (real IAM/RIMES/… via `hw_datasets` + synthetic;
  standard `nn.LSTM` → **dynamic-width** ONNX). Pre-trained weights on Hugging Face:
  **[`ronylicha/gigapdf-ocr-handwriting`](https://huggingface.co/ronylicha/gigapdf-ocr-handwriting)**
  (`model.rten` + `dict.txt`), or retrain with `tools/train_handwriting.py`. Grayscale H32 `Gray32`
  profile, **opt-in** via `recognize_page_handwriting` / `..._with(img, "latin_hw")`.

PaddleOCR PP-OCRv4/v5 covers 100+ scripts — add one by dropping `<subdir>/{model.rten,dict.txt}` in
the models dir + an entry in `REC_MODELS` (`src/lib.rs`).

## Validated
- `rec_probe`: Chinese line `深度学习模型测试2026` decoded **100% correct** (conf 0.999).
- `ocr_auto` (det + auto script selection, 13 printed recognizers): KR→`ko`, JA→`ja`, FR→`zh`,
  RU→`cyrillic` — all routed correctly; Korean & Latin perfect, Cyrillic ~90%.
## Probes / tools
`rec_probe` (single rec), `ocr_probe` (det+rec, one model), `ocr_auto` (load all + auto-select, or
`ocr_auto <dir> <png> latin_hw` to force the handwriting model);
`tools/{fetch_models,train_hebrew,train_handwriting}`.
