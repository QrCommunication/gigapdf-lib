# gigapdf-ocr-rten — server-side OCR via PaddleOCR + RTen (pure-Rust)

State-of-the-art multilingual OCR using **PaddleOCR PP-OCR** models run through
**RTen** (pure-Rust ONNX engine, no C++ dependency — the engine behind `ocrs`).
Replaces the legacy hand-trained CRNN. Heavy ML deps live here, host-side; the
lean pure-std `core`/`wasm` crates stay dependency-free and call this via an endpoint.

## Models (not committed — fetch + convert)
1. **Detection** (shared, language-agnostic) + Chinese/English **rec** ONNX from RapidOCR:
   `https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv4/ch_PP-OCRv4_{rec,det}_infer.onnx`
2. **Multilingual rec** (Korean, Japanese, Latin, Arabic, Cyrillic, Devanagari, Tamil, Telugu,
   Kannada, Thai … — PP-OCRv5 covers 100+ languages): `deepghs/paddleocr` or
   `monkt/paddleocr-onnx` on Hugging Face. Each language = its own rec model + dict; **detection
   is shared**, the pipeline is identical — only the model+dict swap.
3. Convert ONNX → `.rten`:  `pip install rten-convert && rten-convert model.onnx`
4. Dict per language ships with PaddleOCR (`ppocr/utils/dict/<lang>_dict.txt`); CTC charlist =
   `[blank] + dict + [space]`.

## Validated (Phase 1)
- `rec_probe`: Chinese line `深度学习模型测试2026` decoded **100% correct** (conf 0.999).
- `ocr_probe` (full det+rec pipeline): 3-line CJK page → **all 3 detected, ~95% accuracy**.
- Multilingual = same architecture (SVTR rec + DBNet det) → proven by transitivity.
