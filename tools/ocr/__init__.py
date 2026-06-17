"""gigapdf OCR training toolkit (build-time only — the runtime stays zero-dep).

Modules:
  scripts       per-script character sets / unicharsets (Latin, Cyrillic, Greek,
                CJK, Arabic, Hebrew, Indic) and the script-group registry.
  fonts         download Noto + Google fonts per script for synthetic rendering.
  corpora       fetch + sample text corpora (Tesseract langdata / Leipzig / Wiki).
  render_lines  render whole text LINES (corpus × fonts) → (image, transcription).
  eval          CER/WER scoring + an optional Tesseract baseline runner.

These feed the offline trainers (tools/train_ocr_cnn.py mono-glyph,
tools/train_ocr_crnn.py line-level). See docs/OCR_TRAINING_DATA.md and
docs/OCR_ARCHITECTURE.md.
"""
