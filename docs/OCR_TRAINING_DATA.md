# OCR models & data

The OCR engine (`gigapdf-ocr-rten`) runs **pre-trained PaddleOCR PP-OCR** models — so for almost
every language there is **no training and no training data to manage**: the models are downloaded
and converted to RTen's `.rten` format at deploy time. The single exception is **Hebrew**, which
PaddleOCR/EasyOCR/MMOCR do not ship, so we train it ourselves on synthetic data.

> Historical note: the retired hand-trained CRNN engine needed a large multi-script training-data
> catalogue (EMNIST, IAM, CASIA, ICDAR/MLT, Leipzig/Wikipedia corpora, Noto fonts, TRDG/SynthTIGER,
> handwriting datasets, …). None of that is needed now — PaddleOCR already trained on it. The old
> catalogue is gone; this document covers only what the current engine uses.

## 1. Pre-trained PaddleOCR models (12 languages)

Source: **`deepghs/paddleocr`** on Hugging Face (PaddleOCR PP-OCRv3/v4 inference models exported to
ONNX). One shared **DBNet** detector + one **SVTR/CRNN+CTC** recognizer per language, each with its
own `dict.txt` character list.

Fetched + converted by **`crates/ocr-rten/tools/fetch_models.sh`**:

```
det  : ch_PP-OCRv4_det
rec  : arabic · ch_PP-OCRv4 · chinese_cht · cyrillic · devanagari · en_PP-OCRv4
       japan · ka (Kannada) · korean · latin · ta (Tamil) · te (Telugu)
```

ONNX → `.rten` conversion uses `rten-convert` (`pip install rten-convert`). PaddleOCR PP-OCRv4/v5
covers **100+ scripts** — to add one, drop its `model.rten` + `dict.txt` into the models dir and add
an entry to `REC_MODELS` in `crates/ocr-rten/src/lib.rs`.

## 2. Hebrew (the only newly-trained model)

PaddleOCR has no Hebrew model. Hebrew is a small, non-stacking alphabet (≈22 letters + 5 finals),
so a compact CRNN+CTC trains well on synthetic data. Trainer: **`crates/ocr-rten/tools/train_hebrew.py`**.

- **Fonts:** ~10 Hebrew typefaces (Noto Serif/Rashi Hebrew, David Libre, Frank Ruhl Libre, Heebo,
  Rubik, Assistant, Secular One, Suez One) — variety for generalization.
- **Text:** procedurally generated lines from a built-in list of common Hebrew words + digits/Latin
  for mixed content; light scan-like augmentation (rotation, gaussian noise).
- **RTL:** labels are in **visual** order via `python-bidi` `get_display` (logical → visual), so the
  CTC model learns the left-to-right glyph order; the engine reverses the output back to logical at
  inference (`rtl: true`). Digits/Latin embedded in RTL are ordered by the BiDi algorithm.
- **Charlist:** aligned exactly with `OcrEngine` (`[blank] + chars + [space]`); emitted as
  `ocr_hebrew.dict.txt`.
- **Output:** CRNN (conv → BiLSTM → CTC) exported to ONNX (legacy exporter, `dynamo=False`) →
  `rten-convert` → `.rten`. Same input convention as PaddleOCR recognizers (RGB, H=48,
  `(px/255−0.5)/0.5`, `[1,3,48,W]`) so it slots into the shared pipeline.
- **Run:** `python train_hebrew.py --fonts ~/hebrew_fonts --out models/ocr_hebrew --nlines 20000 --epochs 20`.
  Drop the resulting `.rten` + dict into `<models_dir>/hebrew/{model.rten,dict.txt}`.

## 3. Handwriting (`latin_hw`) — reused, not trained

PaddleOCR is printed-text only. For **handwriting** (Latin/Cyrillic/Greek) we **reuse** the retired
engine's already-trained CRNN — no new training, no data. `tools/convert_legacy_gpocr.py` parses
`ocr_alpha_hw.gpocr` (the old handwriting model that beat Tesseract on IAM, CER 0.309), rebuilds the
CRNN, loads its weights (GPO1 int8 × per-layer scale → f32), and exports ONNX → `.rten`. It keeps the
legacy input convention (grayscale H32, ink=1, fixed width, blank-last) — the `LegacyGray32` profile
— and is **opt-in** (`recognize_page_with(img, "latin_hw")`), since a handwriting model is
overconfident on printed/other-script input and must not enter auto script selection.

## 4. Adding a new language

1. **PaddleOCR already supports it** → add its `rec/<lang>` to `fetch_models.sh`, add an entry to
   `REC_MODELS`, redeploy.
2. **PaddleOCR doesn't support it** (like Hebrew) → adapt `train_hebrew.py` (charset + fonts +
   corpus + `rtl` flag), train, convert, drop into the models dir.

No data catalogue to maintain: pre-trained for the common case, one small synthetic trainer for the
gaps.
