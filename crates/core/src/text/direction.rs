//! Unicode text-direction and script detection (no third-party library).
//!
//! Pure `std`, codepoint-based. A run's direction is decided by counting
//! **strong** scalars — those with an inherent left-to-right or right-to-left
//! reading order — and ignoring neutrals (digits, punctuation, spaces,
//! symbols). This mirrors the bidi heuristic readers use to lay out a run, and
//! gives the SDK a reusable language/direction capability for any consumer
//! (editor, viewer, export), not just the PDF overlay editor.

/// The reading direction of a text run, by its strong characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Left-to-right (Latin, Greek, Cyrillic, CJK, …) dominates.
    Ltr,
    /// Right-to-left (Arabic, Hebrew, …) dominates.
    Rtl,
    /// No strong characters at all (only digits/punctuation/spaces/symbols).
    Neutral,
}

/// The dominant writing system of a body of text. `Other` covers scripts the
/// engine does not classify individually (it still contributes to the
/// LTR/RTL/neutral direction tally where it has a strong direction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    /// Arabic and Arabic-derived (Urdu, Persian…), incl. presentation forms.
    Arabic,
    /// Hebrew, incl. the Alphabetic Presentation Forms block.
    Hebrew,
    /// Latin (Basic, Latin-1, Extended-A/B and the Latin Supplement letters).
    Latin,
    /// Greek and Coptic.
    Greek,
    /// Cyrillic.
    Cyrillic,
    /// Unified CJK ideographs, Hiragana, Katakana and Hangul.
    Cjk,
    /// Any other script (or none) — not classified individually.
    Other,
}

/// A body of text's aggregate language signal: its dominant [`Direction`] and
/// [`Script`], plus a best-effort ISO-639-1 language guess (`None` when the
/// script does not pin a single language, e.g. plain Latin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocLanguage {
    /// Dominant reading direction across all the supplied text.
    pub direction: Direction,
    /// Dominant writing system across all the supplied text.
    pub script: Script,
    /// Best-effort ISO-639-1 code (`"ar"`, `"he"`, `"zh"`/`"ja"`…), or `None`.
    pub lang: Option<String>,
}

/// True for a scalar whose inherent reading order is right-to-left — Arabic
/// (incl. Supplement, Extended-A/B and both Presentation Forms blocks) and
/// Hebrew (incl. its Alphabetic Presentation Forms).
fn is_strong_rtl(c: char) -> bool {
    matches!(c as u32,
        0x0590..=0x05FF // Hebrew
        | 0x0600..=0x06FF // Arabic
        | 0x0750..=0x077F // Arabic Supplement
        | 0x08A0..=0x08FF // Arabic Extended-A
        | 0xFB1D..=0xFB4F // Hebrew presentation forms (Alphabetic Presentation Forms)
        | 0xFB50..=0xFDFF // Arabic Presentation Forms-A
        | 0xFE70..=0xFEFF // Arabic Presentation Forms-B
    )
}

/// True for a scalar whose inherent reading order is left-to-right and that we
/// count as a "strong LTR letter": Latin, Greek, Cyrillic and CJK letters.
/// Digits, punctuation, spaces and symbols are deliberately excluded (neutral).
fn is_strong_ltr(c: char) -> bool {
    classify_ltr_script(c).is_some()
}

/// If `c` is a strong left-to-right letter, the [`Script`] it belongs to;
/// `None` for RTL scalars, digits, punctuation, whitespace and symbols.
fn classify_ltr_script(c: char) -> Option<Script> {
    let u = c as u32;
    // Latin: Basic Latin letters, Latin-1 letters, Latin Extended-A/B and the
    // additional supplement letters — but NOT digits or ASCII punctuation.
    if c.is_ascii_alphabetic() {
        return Some(Script::Latin);
    }
    match u {
        0x00C0..=0x024F => Some(Script::Latin), // Latin-1 Supplement letters + Extended-A/B
        0x0370..=0x03FF | 0x1F00..=0x1FFF => Some(Script::Greek), // Greek & Coptic + Greek Extended
        0x0400..=0x052F => Some(Script::Cyrillic), // Cyrillic + Cyrillic Supplement
        0x3040..=0x30FF // Hiragana + Katakana
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xAC00..=0xD7AF // Hangul Syllables
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        => Some(Script::Cjk),
        _ => None,
    }
}

/// Detect the reading [`Direction`] of a single run.
///
/// Counts strong-RTL scalars against strong-LTR letters: [`Direction::Rtl`]
/// when RTL strictly dominates, [`Direction::Ltr`] when LTR strictly dominates,
/// and [`Direction::Neutral`] when there are no strong characters at all. Ties
/// (equal non-zero counts) resolve to [`Direction::Ltr`].
pub fn run_direction(text: &str) -> Direction {
    let mut rtl = 0usize;
    let mut ltr = 0usize;
    for c in text.chars() {
        if is_strong_rtl(c) {
            rtl += 1;
        } else if is_strong_ltr(c) {
            ltr += 1;
        }
    }
    if rtl == 0 && ltr == 0 {
        Direction::Neutral
    } else if rtl > ltr {
        Direction::Rtl
    } else {
        Direction::Ltr
    }
}

/// Per-script strong-character tallies accumulated across a body of text.
#[derive(Default)]
struct ScriptCounts {
    arabic: usize,
    hebrew: usize,
    latin: usize,
    greek: usize,
    cyrillic: usize,
    cjk: usize,
}

impl ScriptCounts {
    fn add(&mut self, c: char) {
        let u = c as u32;
        if is_strong_rtl(c) {
            // Hebrew lives in 0x0590..=0x05FF and 0xFB1D..=0xFB4F; everything
            // else in the strong-RTL set is Arabic.
            if matches!(u, 0x0590..=0x05FF | 0xFB1D..=0xFB4F) {
                self.hebrew += 1;
            } else {
                self.arabic += 1;
            }
            return;
        }
        match classify_ltr_script(c) {
            Some(Script::Latin) => self.latin += 1,
            Some(Script::Greek) => self.greek += 1,
            Some(Script::Cyrillic) => self.cyrillic += 1,
            Some(Script::Cjk) => self.cjk += 1,
            _ => {}
        }
    }

    /// The script with the most strong characters (`Other` if none seen). Ties
    /// resolve in a fixed, deterministic order.
    fn dominant(&self) -> Script {
        let ranked = [
            (self.arabic, Script::Arabic),
            (self.hebrew, Script::Hebrew),
            (self.cjk, Script::Cjk),
            (self.cyrillic, Script::Cyrillic),
            (self.greek, Script::Greek),
            (self.latin, Script::Latin),
        ];
        let mut best = Script::Other;
        let mut best_count = 0usize;
        for (count, script) in ranked {
            if count > best_count {
                best_count = count;
                best = script;
            }
        }
        best
    }
}

/// Aggregate the language signal of a sequence of text runs.
///
/// Sums strong-character counts across every `&str` and returns the dominant
/// [`Direction`] and [`Script`], plus a best-effort ISO-639-1 guess: Arabic →
/// `"ar"`, Hebrew → `"he"`, CJK → `"zh"`/`"ja"` (Kana present ⇒ Japanese), and
/// `None` otherwise (Latin/Greek/Cyrillic do not pin a single language).
pub fn document_language<'a>(texts: impl Iterator<Item = &'a str>) -> DocLanguage {
    let mut rtl = 0usize;
    let mut ltr = 0usize;
    let mut counts = ScriptCounts::default();
    let mut has_kana = false;
    for text in texts {
        for c in text.chars() {
            if is_strong_rtl(c) {
                rtl += 1;
            } else if is_strong_ltr(c) {
                ltr += 1;
            }
            counts.add(c);
            if matches!(c as u32, 0x3040..=0x30FF) {
                has_kana = true;
            }
        }
    }

    let direction = if rtl == 0 && ltr == 0 {
        Direction::Neutral
    } else if rtl > ltr {
        Direction::Rtl
    } else {
        Direction::Ltr
    };

    let script = counts.dominant();
    let lang = match script {
        Script::Arabic => Some("ar".to_string()),
        Script::Hebrew => Some("he".to_string()),
        // CJK ideographs are shared; presence of Kana disambiguates Japanese,
        // otherwise default to Chinese.
        Script::Cjk if has_kana => Some("ja".to_string()),
        Script::Cjk => Some("zh".to_string()),
        _ => None,
    };

    DocLanguage {
        direction,
        script,
        lang,
    }
}

/// The wire token (`"ltr"`/`"rtl"`/`"neutral"`) for a [`Direction`] — used by
/// the ABI/JSON layer so the SDK speaks the same vocabulary.
pub fn direction_str(direction: Direction) -> &'static str {
    match direction {
        Direction::Ltr => "ltr",
        Direction::Rtl => "rtl",
        Direction::Neutral => "neutral",
    }
}

/// The wire token for a [`Script`] (lower-case, stable) — used by the ABI/JSON
/// layer.
pub fn script_str(script: Script) -> &'static str {
    match script {
        Script::Arabic => "arabic",
        Script::Hebrew => "hebrew",
        Script::Latin => "latin",
        Script::Greek => "greek",
        Script::Cyrillic => "cyrillic",
        Script::Cjk => "cjk",
        Script::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_direction_arabic_is_rtl() {
        // "Hello world" in Arabic.
        assert_eq!(run_direction("مرحبا بالعالم"), Direction::Rtl);
    }

    #[test]
    fn run_direction_latin_is_ltr() {
        assert_eq!(run_direction("hello world"), Direction::Ltr);
    }

    #[test]
    fn run_direction_digits_punct_is_neutral() {
        assert_eq!(run_direction("123 — ."), Direction::Neutral);
    }

    #[test]
    fn run_direction_hebrew_is_rtl() {
        // "Hello" in Hebrew.
        assert_eq!(run_direction("שלום"), Direction::Rtl);
    }

    #[test]
    fn run_direction_greek_and_cyrillic_are_ltr() {
        assert_eq!(run_direction("Ελληνικά"), Direction::Ltr);
        assert_eq!(run_direction("Привет"), Direction::Ltr);
    }

    #[test]
    fn run_direction_empty_is_neutral() {
        assert_eq!(run_direction(""), Direction::Neutral);
    }

    #[test]
    fn run_direction_mixed_rtl_dominant() {
        // Mostly Arabic with a couple of Latin letters → RTL.
        assert_eq!(run_direction("مرحبا ok بالعالم"), Direction::Rtl);
    }

    #[test]
    fn run_direction_mixed_ltr_dominant() {
        // Mostly Latin with one Arabic word → LTR.
        assert_eq!(run_direction("hello مرحبا there friend"), Direction::Ltr);
    }

    #[test]
    fn document_language_hebrew_dominant() {
        let texts = ["שלום עולם", "123", "ברוך הבא"];
        let dl = document_language(texts.iter().copied());
        assert_eq!(dl.direction, Direction::Rtl);
        assert_eq!(dl.script, Script::Hebrew);
        assert_eq!(dl.lang.as_deref(), Some("he"));
    }

    #[test]
    fn document_language_arabic_dominant() {
        let texts = ["مرحبا", "بالعالم"];
        let dl = document_language(texts.iter().copied());
        assert_eq!(dl.direction, Direction::Rtl);
        assert_eq!(dl.script, Script::Arabic);
        assert_eq!(dl.lang.as_deref(), Some("ar"));
    }

    #[test]
    fn document_language_mixed_ltr_dominant() {
        // Latin sentence dominates a sprinkle of RTL → LTR.
        let texts = ["The quick brown fox", "مرحبا", "jumps over"];
        let dl = document_language(texts.iter().copied());
        assert_eq!(dl.direction, Direction::Ltr);
        assert_eq!(dl.script, Script::Latin);
        assert_eq!(dl.lang, None);
    }

    #[test]
    fn document_language_cjk_kana_is_japanese() {
        let texts = ["日本語のテキスト"];
        let dl = document_language(texts.iter().copied());
        assert_eq!(dl.direction, Direction::Ltr);
        assert_eq!(dl.script, Script::Cjk);
        assert_eq!(dl.lang.as_deref(), Some("ja"));
    }

    #[test]
    fn document_language_cjk_without_kana_is_chinese() {
        let texts = ["中文文本内容"];
        let dl = document_language(texts.iter().copied());
        assert_eq!(dl.script, Script::Cjk);
        assert_eq!(dl.lang.as_deref(), Some("zh"));
    }

    #[test]
    fn document_language_empty_is_neutral_other() {
        let dl = document_language(std::iter::empty());
        assert_eq!(dl.direction, Direction::Neutral);
        assert_eq!(dl.script, Script::Other);
        assert_eq!(dl.lang, None);
    }

    #[test]
    fn direction_and_script_wire_tokens() {
        assert_eq!(direction_str(Direction::Ltr), "ltr");
        assert_eq!(direction_str(Direction::Rtl), "rtl");
        assert_eq!(direction_str(Direction::Neutral), "neutral");
        assert_eq!(script_str(Script::Arabic), "arabic");
        assert_eq!(script_str(Script::Cjk), "cjk");
        assert_eq!(script_str(Script::Other), "other");
    }
}
