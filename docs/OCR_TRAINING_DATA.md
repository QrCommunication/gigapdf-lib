# OCR training data — multi-script sources catalogue

> **Goal.** Take gigapdf's built-in OCR from *Latin-only, single-glyph, ~61 % per-glyph*
> to **Tesseract-level accuracy across Latin, Cyrillic, Greek, CJK, Arabic, Hebrew and
> Indic scripts** — while keeping the runtime **zero-dependency / pure-`std`** (training is
> offline; only int8 weights ship). See [`OCR_ARCHITECTURE.md`](./OCR_ARCHITECTURE.md) for
> the recognizer redesign (mono-glyph → line-level CRNN+CTC) that consumes this data.

## How real OCR engines are actually trained

Tesseract, PaddleOCR and EasyOCR are trained **mostly on *synthetic* data**: a text
**corpus** (real word/n-gram frequencies) is rendered with many **fonts** onto line images,
then **augmented** to mimic scans (blur, noise, skew, ink bleed). Real labelled datasets are
comparatively tiny, often `⚠ licence`-restricted, and are best used for **validation** and
**fine-tuning** (handwriting, scene text) — not as the bulk of training.

gigapdf already does a *glyph-level* version of this (`tools/train_ocr.py`:
`render_glyph` + EMNIST). The upgrade is to render **whole lines** from **per-script
corpora + Noto fonts**, and add real datasets per script for robustness.

**Licence legend.** ✅ open (OFL / Apache / MIT / CC-BY[-SA] / CC0 / public domain) ·
🔶 free **for research**, registration/agreement required · ⛔ paid / membership (e.g. LDC).
Always re-verify the licence at the source before downloading or shipping anything derived.

---

## 1. Cross-script foundations (the scalable core)

### 1.1 Fonts — the rendering engine for synthetic data

| Source | Scripts | Licence | Note |
|---|---|---|---|
| **Google Noto** (`notofonts.github.io`, `github.com/notofonts`) | **Every Unicode script** (Sans/Serif) | ✅ OFL-1.1 | The multilingual font source. Pull per script via family naming (`Noto Sans <Script>`). |
| **Noto Sans/Serif CJK** (`github.com/googlefonts/noto-cjk`) | CJK (zh-Hans/Hant, ja, ko) | ✅ OFL-1.1 | Full CJK coverage; large files (handle as `.otf`/`.ttc`). |
| **Noto Naskh Arabic / Noto Nastaliq Urdu** | Arabic, Urdu | ✅ OFL-1.1 | Both styles matter (Naskh vs Nastaliq shaping). |
| **Source Han Sans/Serif** (Adobe) | CJK | ✅ OFL-1.1 | Second CJK family for font diversity. |
| **Google Fonts** (already wired: `tools/download_gfonts.py`, `google-fonts-metadata.json`) | Mostly Latin + many scripts via `subsets` | ✅ OFL/Apache | Reuse the `subsets` field to pick per-script families. |
| System fonts (`/usr/share/fonts/**`) | Host-dependent | mixed | Already harvested by `usable_fonts()`. |

> **Integration:** generalize `download_gfonts.py` → add a Noto downloader; select fonts per
> script (Google Fonts `subsets`: `cyrillic`, `greek`, `arabic`, `devanagari`, `bengali`,
> `tamil`, `hebrew`, `korean`, `japanese`, `chinese-*`). Diversity of fonts *is* the
> augmentation for printed text.

### 1.2 Text corpora — realistic character/word distributions

| Source | Coverage | Licence | Note |
|---|---|---|---|
| **Tesseract `langdata_lstm`** (`github.com/tesseract-ocr/langdata_lstm`) | 100+ langs: `*.training_text`, `*.wordlist`, `unicharset`, numbers/punc | ✅ Apache-2.0 | **Best starting point** — purpose-built for OCR training text; reuse unicharsets to define our per-script class sets. |
| **Leipzig Corpora Collection** (`wortschatz.uni-leipzig.de`) | 250+ langs, sentence-per-line files | ✅ CC-BY (per corpus) | Clean sentences, ideal to render lines. |
| **Wikipedia / Wikimedia dumps** (`dumps.wikimedia.org`) | 300+ langs | ✅ CC-BY-SA 3.0/4.0 | Strip wiki markup; huge, free, every script. |
| **OSCAR** (`oscar-project.org`) | 150+ langs (Common Crawl) | ✅ (metadata CC0; content CC caveats) | Web-scale; needs cleanup/dedup. |
| **CC-100** (`data.statmt.org/cc-100`) | 100+ langs | ✅ (Common Crawl terms) | Plain monolingual text, easy to sample. |

> **Integration:** `tools/ocr/corpora.py` fetches+caches a corpus per language and samples
> lines weighted by real frequency; feed those strings to the line renderer.

### 1.3 Synthetic line generators

| Tool | Licence | Note |
|---|---|---|
| **TextRecognitionDataGenerator (TRDG)** — `github.com/Belval/TextRecognitionDataGenerator` | ✅ MIT | The workhorse: any language/font → word/line images, with skew/distortion/blur/backgrounds. CLI `trdg`. |
| **SynthTIGER** — `github.com/clovaai/synthtiger` (Naver Clova) | ✅ MIT | SOTA-grade synthetic text; richer layout/effects. |
| **Tesseract `text2image`** | ✅ Apache-2.0 | Official: emits `.tif`+`.box`+`.gt.txt` from training text + a font. |
| **Albumentations** / **imgaug** | ✅ MIT | Scan-like degradations: JPEG noise, blur, ink bleed, paper texture, rotation, perspective. |

### 1.4 Reference recipes & teacher models (what works per language)

| Source | Licence | Use |
|---|---|---|
| **PaddleOCR PP-OCRv4/v5** (`github.com/PaddlePaddle/PaddleOCR`) | ✅ Apache-2.0 | Per-language dataset lists + dict files (80+ langs) — a ready map of "which data per language". |
| **EasyOCR** (`github.com/JaidedAI/EasyOCR`) | ✅ Apache-2.0 | 80+ langs; documents its training sources. |
| **Tesseract `tessdata_best`** (`github.com/tesseract-ocr/tessdata_best`) | ✅ Apache-2.0 | The benchmark **and** a usable **teacher**: run it over unlabeled scans to auto-label (image, text) pairs for semi-supervised data (distillation). |

---

## 2. Latin (printed + handwritten + scene)

| Dataset | Type | Licence | Size | Note |
|---|---|---|---|---|
| **EMNIST** (NIST SD19) | handwritten chars | ✅ public domain | ~700k | **Already used** (`load_emnist`). Keep for handwriting. |
| **IAM Handwriting DB** (`fki.tic.heia-fr.ch`) | handwritten lines/words (EN) | 🔶 research | ~13k lines | The standard line-level handwriting benchmark. |
| **MJSynth / Synth90k** (VGG Oxford) | synthetic word images | 🔶 research | ~9M | Massive printed-word recognizer pretraining. |
| **SynthText in the Wild** | synthetic scene text | 🔶 research | ~800k imgs / 8M words | Scene-text robustness. |
| **ICDAR2013 / 2015 Robust Reading** (`rrc.cvc.uab.es`) | scene text | 🔶 research | small | Focused + incidental scene text benchmarks. |
| **COCO-Text** | scene text on COCO | 🔶 research | ~63k imgs | Real-world clutter. |
| **TextOCR** (`textvqa.org/textocr`) | scene text | ✅ CC-BY-4.0 | ~28k imgs / 900k words | Large, openly licensed. |
| **GT4HistOCR** (`zenodo.org/record/1344132`) | historical printed lines | ✅ CC-BY-4.0 | ~313k lines | Fraktur/antiqua → degraded/old print. |
| **IMPACT** | historical European print | 🔶 research | large | Old documents across European languages. |

---

## 3. Cyrillic + Greek (segmentable → fast wins)

| Dataset | Type | Licence | Note |
|---|---|---|---|
| Synthetic (Noto + corpora) | printed lines | ✅ | ru/uk/bg/sr/mk/el from Wikipedia + Leipzig — **primary source**. |
| **Cyrillic Handwriting Dataset** (Kaggle: `constantinwerner/cyrillic-handwriting-dataset`) | handwritten (RU) | ✅ open | ~73k word images. |
| **HKR Dataset** (`github.com/abdoelsayed2016/HKR_Dataset`) | handwritten (RU/KK) | 🔶 research | ~64k; Cyrillic lines. |
| **GRPOLY-DB** | polytonic Greek (historical) | 🔶 research | Greek diacritics / historical print. |

---

## 4. CJK (Chinese · Japanese · Korean)

> Thousands of classes → a **dedicated per-script model** (see architecture doc). Mostly
> block-segmentable, so synthetic line rendering from Noto CJK + corpora carries most of it.
>
> **Wired today:** Chinese = group `cjk` (trained, 2401-class, CER 0.206 on CASIA — see the
> training log). **Japanese (`jpn`) and Korean (`kor`) are their own groups** (kana+kanji /
> Hangul), each built data-driven by `build_cjk_charset.py` from the synthetic-OCR datasets in
> §8.2 (`japanese` 150k / `korean` 200k). Those corpora are **pure-script**, so the charset
> **force-includes full printable ASCII** and training mixes in **Latin synthetic lines**
> (`GIGA_OCR_LANGS=jpn,eng` / `kor,eng`) — otherwise the model could never read the
> alphanumerics (prices, dates, codes) that pepper real JP/KR documents.

| Dataset | Type | Licence | Note |
|---|---|---|---|
| Synthetic (Noto CJK / Source Han + corpora) | printed lines | ✅ | Primary; sample from common-char frequency lists (e.g. 通用规范汉字表, JIS, KS). |
| **Synthetic Chinese String Dataset** | synthetic lines (ZH) | ✅ open | ~3.6M line images, 5990-char set — strong ZH baseline. |
| **CASIA-HWDB / OLHWDB** (`nlpr.ia.ac.cn`) | handwritten (ZH) | 🔶 research agreement | ~3.9M samples, 7185 classes; offline + online. |
| **CTW — Chinese Text in the Wild** (`ctwdataset.github.io`) | scene text (ZH) | 🔶 research | ~32k imgs / ~1M chars. |
| **ICDAR2017-RCTW / 2019-ReCTS / 2019-LSVT / 2019-ArT** | scene text (ZH) | 🔶 research | rrc.cvc.uab.es competitions. |
| **ETL Character Database** (`etlcdb.db.aist.go.jp`) | handwritten/printed (JA) | 🔶 research | kana + kanji. |
| **Kuzushiji** — KMNIST / Kuzushiji-49 / Kuzushiji-Kanji (`github.com/rois-codh/kmnist`) | historical cursive (JA) | ✅ CC-BY-SA-4.0 | classical Japanese. |
| **AI-Hub Korean OCR** (`aihub.or.kr`) | printed/scene (KO) | 🔶 free w/ account | large Hangul corpora. |
| **PHD08** | handwritten Hangul | 🔶 research | Hangul syllable DB. |

---

## 5. Arabic + Hebrew (RTL, cursive → needs CRNN+CTC)

> Cursive/connected: **cannot** be done by the mono-glyph path; Arabic also has 4 contextual
> letter forms. These scripts are the clearest justification for the line-level recognizer.

| Dataset | Type | Licence | Note |
|---|---|---|---|
| Synthetic (Noto Naskh/Nastaliq/Hebrew + corpora) | printed lines | ✅ | Primary; render shaped text (use a shaping engine so contextual forms are correct). |
| **APTI — Arabic Printed Text Image** | synthetic printed (AR) | 🔶 research | ~45M word images, many fonts/sizes. |
| **ICDAR2017/2019 MLT** (Arabic subset) | scene text (AR) | 🔶 research | multilingual scene text. |
| **EvArEST** | scene text (AR) | ✅ open | Arabic-in-the-wild. |
| **KHATT** (`khatt.ideas2serve.net`) | handwritten (AR) | 🔶 research | lines/paragraphs. |
| **IFN/ENIT** (`ifnenit.com`) | handwritten (AR) | 🔶 research | ~26k Tunisian town names. |
| **MADCAT** (LDC2012T15…) | handwritten (AR) | ⛔ LDC | high quality, paid. |
| **AHCD** (Kaggle) | handwritten chars (AR) | ✅ open | ~16.8k isolated chars (warm-up only). |
| **VML-HD** | historical handwritten (HE) | 🔶 research | Hebrew manuscripts. |

---

## 6. Indic scripts (Devanagari, Bengali, Tamil, …)

> Conjuncts/ligatures and matras → also best served by the line-level recognizer.

| Dataset | Type | Licence | Note |
|---|---|---|---|
| Synthetic (Noto Devanagari/Bengali/Tamil/Telugu/… + corpora) | printed lines | ✅ | Primary; Wikipedia hi/bn/ta/te/kn/ml/gu/pa. |
| **DHCD — Devanagari Handwritten Character Dataset** (UCI ML) | handwritten chars | ✅ CC-BY-4.0 | ~92k images, 46 classes. |
| **IIIT-HW-Dev** / **IIIT-INDIC-HW-WORDS** (`cvit.iiit.ac.in`) | handwritten words (multi-Indic) | 🔶 research | word-level, several scripts. |
| **MILE** (`mile.ee.iisc.ac.in`) | printed/handwritten (Kannada, Tamil) | 🔶 research | South-Indian scripts. |
| **Bangla:** BanglaWriting (Mendeley), **Ekush** (`ekush.info`), **CMATERdb**, **BN-HTRd** (Mendeley) | handwritten (BN) | mix ✅/🔶 | strong Bengali coverage. |
| **ICDAR2019-MLT** (Bangla subset) | scene text (BN) | 🔶 research | multilingual scene. |

---

## 7. Priority & roadmap (tied to the plan phases)

1. **Phase 1 (infra).** Noto downloader + corpora fetchers + `text2image`/TRDG line renderer +
   per-script unicharsets (seed from `langdata_lstm`). Build a labelled **eval set** per script
   and a **Tesseract baseline** (CER/WER).
2. **Phase 2 (fast wins, current mono-glyph model).** Add **Latin-extended + Cyrillic + Greek**
   classes; retrain `train_ocr_cnn.py`; measure CER gain. No runtime change.
3. **Phase 3 (CRNN+CTC).** Train the shared **alphabetic** line model (Latin+Cyrillic+Greek)
   on synthetic lines (+ IAM/handwriting fine-tune); validate vs Tesseract.
4. **Phase 4 (non-Latin).** Per-script line models: **CJK** (Synthetic Chinese String + CASIA +
   Noto CJK), **Arabic/Hebrew** (APTI + KHATT + shaped synthetic), **Indic** (synthetic + DHCD/
   IIIT/Bangla).
5. **Continuous.** Use **Tesseract `tessdata_best` as a teacher** to auto-label unlabeled scans
   (semi-supervised) where licences of real datasets are restrictive.

## 8. Implemented handwriting pipeline (what is actually wired)

Two ungated, dependency-light sources feed handwriting into training. The gated references
in §2–6 (official IAM, CASIA, KHATT, IIIT-HW…) need an HF token — see §8.3.

### 8.1 Synthetic handwriting fonts
`tools/ocr/fonts.py <group> --handwriting` downloads the Google-Fonts **Handwriting**
category filtered to the group's script subsets (`handwriting_fonts_for_group`). Rendering
corpus lines in these cursive/handprint faces is the TRDG/Tesseract recipe for handwriting
robustness; a cmap guard (`font_covers`) stops a Latin face from rendering tofu on
Cyrillic/Greek. Counts: Latin ~199 faces, other scripts far fewer (Cyrillic 15, Greek/Deva 4,
Arabic 1, Tamil 1, Bengali 0) — so real datasets matter more there. Trainer knob
**`GIGA_OCR_HW_FRAC`** = fraction of lines drawn in a covering handwriting face.

### 8.2 Real handwriting datasets (HF datasets-server REST)
`tools/ocr/hw_datasets.py` streams real (image, transcription) line pairs via the HuggingFace
**datasets-server** — no `datasets`/`pyarrow` dependency — normalised to the render_lines
strip convention (H=32, ink-high, float32) with a pickle-free `.npz` cache. Trainer knobs
**`GIGA_OCR_HW_REAL="iam,rimes,…"`** + `GIGA_OCR_HW_REAL_N`. Image fetching is **concurrent**
(`ThreadPoolExecutor`, `GIGA_OCR_DL_WORKERS`, default 16): the per-image HTTP round-trip is the
bottleneck, so a pool gives a ~16× speed-up — pairs with an **HF Pro token** (datasets-server
rate limits lifted; `_fetch` still honours 429/`Retry-After`). Ungated mirrors wired:

| Alias | HF dataset (ungated) | Script / lang | Group |
|-------|----------------------|---------------|-------|
| `iam` | Teklia/IAM-line | English HW | alpha |
| `rimes` | Teklia/RIMES-2011-line | French HW | alpha |
| `norhand` | Teklia/NorHand-v3-line | Norwegian HW | alpha |
| `newseye` | Teklia/NewsEye-Austrian-line | German HW | alpha |
| `belfort` | Teklia/Belfort-line | French HW | alpha |
| `esposalles` | Teklia/Esposalles-line | Catalan HW | alpha |
| `cyrillic` | deepcopy/synthetic-handwritten-cyrillic-180k | Cyrillic HW | alpha |
| `casia` | Teklia/CASIA-HWDB2-line | Chinese HW | cjk |
| `chinese` | priyank-m/chinese_text_recognition | Chinese printed/scene | cjk |
| `japanese` | deepcopy/japanese-synthetic-ocr-150k | Japanese (synthetic, 150k; text field `string`) | jpn |
| `korean` | Jiwon-Kang/OCR-Synthetic-Rendered-Korean-200K | Korean (synthetic, 200k; text field `render_text`) | kor |
| `iiit_hindi` | c3rl/IIIT-INDIC-HW-WORDS-Hindi | Devanagari **handwriting** words (~70k) | deva |
| `iiit_tamil` | c3rl/IIIT-INDIC-HW-WORDS-Tamil | Tamil **handwriting** words (~76k) | taml |

**Non-Latin handwriting variants** (`deploy/train_hw_nonlatin.sh`, output `ocr_<group>_hw.gpocr`):
Devanagari & Tamil have strong ungated real HW above (IIIT-INDIC-HW-WORDS). **Arabic, Hebrew and
Bengali have no ungated real HW line corpus** (KHATT mirrors ship empty transcriptions; Hebrew sets
are doc-/text-level) → those fall back to synthetic *Handwriting*-font lines only and stay weaker.
All HW variants are **mixed**: the deva/beng/taml charsets now include Latin + digits and training
renders Latin synthetic lines (`GIGA_OCR_LANGS=<lang>,eng`), so dates / numbers / codes are read.

(datasets-server serves only the *converted* row subset for some datasets, so a few cap low:
IAM ~800, Cyrillic ~1.1k observed. The JP/KR synthetic sets are pure-script, so their data-driven
charsets force-include full printable ASCII and training adds Latin synthetic lines — see §4.)

### 8.3 HuggingFace token (unlocks gated handwriting)
`hw_datasets._hf_token()` reads `HF_TOKEN`/`HUGGINGFACE_TOKEN`/`HUGGING_FACE_HUB_TOKEN` env or
`~/.huggingface/token` and sends `Authorization: Bearer` to huggingface.co. A token unlocks
gated corpora — most needed for **Arabic** and **Indic** (Devanagari/Bengali/Tamil) where
ungated line-level (image+text) mirrors are scarce (official IAM, CASIA-full, KHATT, IIIT-HW).
**Latin, Cyrillic and Chinese are already covered ungated** (table above).

## 9. Degraded / photographed documents (photo variant — tooling built)

Crumpled paper, phone photos, faded thermal receipts: the model must **see** such degradation at
train time (domain randomization), complemented by the planned `ocr.rs` restoration front-end
(see [`OCR_ARCHITECTURE.md`](./OCR_ARCHITECTURE.md) §6 "Degraded / photographed documents").

### 9.1 Degradation augmentation (built)
`render_lines.py::_degrade` (gated by `GIGA_OCR_DEGRADE=1`) applies, per line: paper-curl wave,
shear, Gaussian blur, uneven illumination + background haze/stains, low-resolution down/up-sample,
JPEG artefacts, sensor + salt-pepper noise, contrast/brightness jitter — all pure numpy/PIL,
preserving the H=32 strip. Off by default (clean training keeps a light aug).

### 9.2 Training the photo variant
```
GIGA_OCR_DEGRADE=1 GIGA_OCR_VARIANT=photo GIGA_OCR_HW_FRAC=0.3 bash deploy/train_vps.sh
```
`GIGA_OCR_VARIANT=photo` writes `models/ocr_alpha_photo.gpocr` + `ocr_model_alpha_photo.rs`
(no clobber of the clean primary `ocr_alpha`); the host picks it via `gp_ocr_load_model` for
noisy input. Runs detached (tmux `megatrain_photo`).

### 9.3 Real degraded / receipt corpora (to add)
| Dataset | Type | Licence | Note |
|---|---|---|---|
| **SROIE** (ICDAR2019) | scanned receipts | research | line/word boxes + text — receipt domain |
| **CORD** | receipt photos | CC-BY 4.0 ✅ | line-level, HF-hosted |
| **FUNSD** | scanned forms | research | noisy forms, word boxes |
| **RVL-CDIP** | document photos | research | degraded-doc imagery |

Wire as `hw_datasets` entries (image+text line mirrors) once the HF config is confirmed; mix at a
modest fraction so IAM/RIMES still anchor the clean-text signal.

## 10. Licensing cautions

- **Ship only ✅-licensed derived weights.** Models trained on 🔶/⛔ data may inherit usage
  restrictions — keep such data to *internal validation* unless the licence permits otherwise.
- **OFL fonts:** fine to render training images; do **not** redistribute the font files inside
  the repo unless OFL terms are followed (we download at build time, like Google Fonts today).
- **Wikipedia/Leipzig:** CC-BY[-SA] — attribution; rendered glyph images are generally fine.
- Record the exact source + licence of every dataset used in each model's `ocr_model_<script>.rs`
  header (as `train_ocr_cnn.py` already records `val_acc`).
