# gigapdf-ocr-rten — server-side OCR via PaddleOCR + RTen (pure-Rust)

State-of-the-art multilingual OCR using **PaddleOCR PP-OCR** models run through
**RTen** (pure-Rust ONNX engine, no C++ dependency — the engine behind `ocrs`).
Replaces the legacy hand-trained CRNN. Heavy ML deps live here, host-side; the
lean pure-std `core`/`wasm` crates stay dependency-free and call this via an endpoint.

## Models (not committed — fetch + convert)
1. Download PP-OCR ONNX (det + rec) from RapidOCR:
   `https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv4/ch_PP-OCRv4_{rec,det}_infer.onnx`
2. Convert ONNX → `.rten`:  `pip install rten-convert && rten-convert model.onnx`
3. Dict: `ppocr_keys_v1.txt` (PaddleOCR repo) — CTC charlist = `[blank] + dict + [space]`.

## Probe (validated)
`cargo run -p gigapdf-ocr-rten --bin rec_probe -- <rec.rten> <dict.txt> <line.png>`
→ decodes a cropped text line. Verified: `深度学习模型测试2026` decoded 100% correct.
