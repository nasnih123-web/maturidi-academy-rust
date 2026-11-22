//! Voice intent classification + ayah-reference extraction — a
//! faithful port of `app/modules/voice/intents.py` (pure, no I/O).
//!
//! Deterministic and data-driven (no LLM): pattern priority is
//! quran, then fatwa, then a surah-name mention, otherwise general.
//! The Rust `regex` crate has no lookahead/lookbehind, so the
//! word-boundary check (`_word_boundary_name`) is implemented by
//! hand over `char_indices` — same rule: the char before/after the
//! name must not be an Arabic letter.
//!
//! Dead code is allowed here on purpose: no native route calls this
//! module yet (the ask route stays proxied until an STT sidecar
//! exists, ADR-046/ADR-047). It is exercised by unit tests and is
//! the library the future native ask will call.
#![allow(dead_code)]

use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

use crate::quran_data::SURAHS;

pub const QURAN: &str = "quran";
pub const FATWA: &str = "fatwa";
pub const GENERAL: &str = "general";
pub const INTENTS: &[&str] = &[QURAN, FATWA, GENERAL];
pub const SUPPORTED_LANGS: &[&str] = &["fa", "ar"];

// --- language detection ---------------------------------------------------

const FA_CHARS: &str = "گچپژ";
const AR_CHARS: &str = "ةك"; // ك = U+0643 (Arabic) vs ک = U+06A9 (Persian)
const FA_YEH: char = 'ی'; // U+06CC
const AR_YEH: char = 'ي'; // U+064A
const FA_KAF: char = 'ک'; // U+06A9

const FA_MARKERS: &[&str] = &[
    "است",
    "این",
    "آن",
    "چیست",
    "چگونه",
    "چطور",
    "چرا",
    "بخوان",
    "بگو",
    "درباره",
    "دربارهٔ",
    "برای",
    "آیا",
    "جایز",
    "حکم",
    "نماز",
    "روزه",
    "می‌شود",
    "میشود",
    "نمی",
    "ها",
    "های",
    "تر",
    "ترین",
];
const AR_MARKERS: &[&str] = &[
    "هل",
    "يجوز",
    "حكم",
    "الحكم",
    "اقرأ",
    "اقرا",
    "آية",
    "آيه",
    "سورة",
    "سوره",
    "عن",
    "من",
    "في",
    "الذي",
    "التي",
    "كيف",
    "لماذا",
    "ما هو",
];

/// `intents.detect_language` — "fa" | "ar" | "unknown".
pub fn detect_language(text: &str) -> &'static str {
    if text.trim().is_empty() {
        return "unknown";
    }
    let mut fa_score: i64 = 0;
    let mut ar_score: i64 = 0;
    for ch in text.chars() {
        if FA_CHARS.contains(ch) {
            fa_score += 3;
        } else if AR_CHARS.contains(ch) {
            ar_score += 2;
        } else if ch == FA_YEH {
            fa_score += 1;
        } else if ch == AR_YEH {
            ar_score += 1;
        } else if ch == FA_KAF {
            fa_score += 1;
        }
    }
    let lowered = format!(" {text} ");
    for w in FA_MARKERS {
        if lowered.contains(w) {
            fa_score += 2;
        }
    }
    for w in AR_MARKERS {
        if lowered.contains(w) {
            ar_score += 2;
        }
    }
    if fa_score == 0 && ar_score == 0 {
        return "unknown";
    }
    if fa_score >= ar_score {
        "fa"
    } else {
        "ar"
    }
}

// --- intent patterns --------------------------------------------------------

const QURAN_PATTERNS: &[&str] = &[
    r"آی[هة]\s*(الکرسی|الكرسي)",
    r"(بخوان|بگو|بیاور|نشان\s*بده|پخش\s*کن).{0,20}آی[هة]",
    r"آی[هة].{0,30}(بخوان|بگو|بیاور|بخوانید|بگویید)",
    r"تلاوت",
    r"(بخوان|پخش\s*کن).{0,20}سور[هة]",
    r"سور[هة].{0,30}(بخوان|بخوانید)",
    r"قرآن.{0,20}(بخوان|اقرأ)",
    r"(اقرأ|اِقرأ).{0,20}(آي[ةه]|سور[ةه])",
    r"(آي[ةه]|سور[ةه]).{0,30}(اقرأ|اِقرأ)",
    r"آي[ةه]\s*الكرسي",
    r"تلاو[ةه]",
    r"\d+\s*[:/]\s*\d+",
];

const FATWA_PATTERNS: &[&str] = &[
    r"آیا.{0,40}(جایز|حرام|حلال|واجب|مستحب|مکروه|صحیح|باطل)",
    r"حکم.{0,40}چیست",
    r"(جایز|حرام|حلال|واجب)\s*(است|هست|می‌شود|میشود)",
    r"هل.{0,40}(يجوز|حرام|حلال|واجب|مستحب|مكروه|صحيح|باطل)",
    r"ما\s+حكم",
    r"فتوا",
    r"فقه",
];

static QURAN_RES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    QURAN_PATTERNS
        .iter()
        .map(|p| Regex::new(p).expect("valid quran intent pattern"))
        .collect()
});
static FATWA_RES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    FATWA_PATTERNS
        .iter()
        .map(|p| Regex::new(p).expect("valid fatwa intent pattern"))
        .collect()
});

/// The Arabic-letter class used for word boundaries
/// (`[\u0621-\u064a\u066e-\u06d3]`).
fn is_arabic_letter(c: char) -> bool {
    ('\u{0621}'..='\u{064a}').contains(&c) || ('\u{066e}'..='\u{06d3}').contains(&c)
}

/// `intents._word_boundary_name` without lookarounds: `name` occurs
/// with no Arabic letter directly before or after it.
fn matches_word_bounded(haystack: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    for (start, _) in haystack.match_indices(name) {
        let before_ok = match haystack[..start].chars().next_back() {
            None => true,
            Some(c) => !is_arabic_letter(c),
        };
        if !before_ok {
            continue;
        }
        let after_ok = match haystack[start + name.len()..].chars().next() {
            None => true,
            Some(c) => !is_arabic_letter(c),
        };
        if after_ok {
            return true;
        }
    }
    false
}

// --- normalization ----------------------------------------------------------

fn is_tashkeel(c: char) -> bool {
    ('\u{064b}'..='\u{0652}').contains(&c) || c == '\u{0670}' || c == '\u{0640}'
}

/// `intents._normalize_ar` — NFKC, strip tashkeel, fa/ar digits →
/// ASCII, char replacements.
pub fn normalize_ar(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let mut out = String::with_capacity(nfkc.len());
    for c in nfkc.chars() {
        if is_tashkeel(c) {
            continue;
        }
        let mapped = match c {
            '۰'..='۹' => char::from_digit(c as u32 - '۰' as u32, 10).unwrap_or(c),
            '٠'..='٩' => char::from_digit(c as u32 - '٠' as u32, 10).unwrap_or(c),
            'أ' | 'إ' | 'آ' => 'ا',
            'ة' => 'ه',
            'ى' | 'ي' => 'ی',
            'ك' => 'ک',
            'ؤ' => 'و',
            'ئ' => 'ی',
            other => other,
        };
        out.push(mapped);
    }
    out
}

// --- surah names --------------------------------------------------------------

/// `intents.SURAH_FA_NAMES` — common Persian surah names → number
/// (reference data, not fiqh content).
pub const SURAH_FA_NAMES: &[(&str, i32)] = &[
    ("فاتحه", 1),
    ("بقره", 2),
    ("آل عمران", 3),
    ("نساء", 4),
    ("مائده", 5),
    ("انعام", 6),
    ("اعراف", 7),
    ("انفال", 8),
    ("توبه", 9),
    ("یونس", 10),
    ("هود", 11),
    ("یوسف", 12),
    ("رعد", 13),
    ("ابراهیم", 14),
    ("حجر", 15),
    ("نحل", 16),
    ("اسراء", 17),
    ("کهف", 18),
    ("مریم", 19),
    ("طه", 20),
    ("انبیاء", 21),
    ("حج", 22),
    ("مؤمنون", 23),
    ("نور", 24),
    ("فرقان", 25),
    ("شعراء", 26),
    ("نمل", 27),
    ("قصص", 28),
    ("عنکبوت", 29),
    ("روم", 30),
    ("لقمان", 31),
    ("سجده", 32),
    ("احزاب", 33),
    ("سبا", 34),
    ("فاطر", 35),
    ("یس", 36),
    ("صافات", 37),
    ("ص", 38),
    ("زمر", 39),
    ("غافر", 40),
    ("فصلت", 41),
    ("شوری", 42),
    ("زخرف", 43),
    ("دخان", 44),
    ("جاثیه", 45),
    ("احقاف", 46),
    ("محمد", 47),
    ("فتح", 48),
    ("حجرات", 49),
    ("ق", 50),
    ("ذاریات", 51),
    ("طور", 52),
    ("نجم", 53),
    ("قمر", 54),
    ("رحمن", 55),
    ("واقعه", 56),
    ("حدید", 57),
    ("مجادله", 58),
    ("حشر", 59),
    ("ممتحنه", 60),
    ("صف", 61),
    ("جمعه", 62),
    ("منافقون", 63),
    ("تغابن", 64),
    ("طلاق", 65),
    ("تحریم", 66),
    ("ملک", 67),
    ("قلم", 68),
    ("حاقه", 69),
    ("معارج", 70),
    ("نوح", 71),
    ("جن", 72),
    ("مزمل", 73),
    ("مدثر", 74),
    ("قیامت", 75),
    ("انسان", 76),
    ("مرسلات", 77),
    ("نبا", 78),
    ("نازعات", 79),
    ("عبس", 80),
    ("تکویر", 81),
    ("انفطار", 82),
    ("مطففین", 83),
    ("انشقاق", 84),
    ("بروج", 85),
    ("طارق", 86),
    ("اعلی", 87),
    ("غاشیه", 88),
    ("فجر", 89),
    ("بلد", 90),
    ("شمس", 91),
    ("لیل", 92),
    ("ضحی", 93),
    ("شرح", 94),
    ("تین", 95),
    ("علق", 96),
    ("قدر", 97),
    ("بینه", 98),
    ("زلزله", 99),
    ("عادیات", 100),
    ("قارعه", 101),
    ("تکاثر", 102),
    ("عصر", 103),
    ("همزه", 104),
    ("فیل", 105),
    ("قریش", 106),
    ("ماعون", 107),
    ("کوثر", 108),
    ("کافرون", 109),
    ("نصر", 110),
    ("مسد", 111),
    ("اخلاص", 112),
    ("فلق", 113),
    ("ناس", 114),
];

/// Persian surah name for a number (reverse of `SURAH_FA_NAMES`),
/// for the `surah_label` of the quran branch.
pub fn surah_fa_name(number: i32) -> Option<&'static str> {
    SURAH_FA_NAMES
        .iter()
        .find(|(_, n)| *n == number)
        .map(|(name, _)| *name)
}

/// `intents._surah_name_map` as an ordered list (insertion order
/// matters: on equal-length matches the first entry wins, like
/// Python's dict). Arabic names from Wave 4 `SURAHS`, then Persian.
static SURAH_MAP: LazyLock<Vec<(String, i32)>> = LazyLock::new(|| {
    let mut map: Vec<(String, i32)> = Vec::new();
    let insert = |name: String, num: i32, map: &mut Vec<(String, i32)>| {
        if !map.iter().any(|(n, _)| *n == name) {
            map.push((name, num));
        }
    };
    for s in SURAHS {
        let norm = normalize_ar(s.name_ar);
        insert(norm.clone(), s.number, &mut map);
        let no_al = norm.strip_prefix("ال").unwrap_or(&norm).to_string();
        insert(no_al, s.number, &mut map);
    }
    for (fa_name, num) in SURAH_FA_NAMES {
        insert(normalize_ar(fa_name), *num, &mut map);
    }
    map
});

fn mentions_surah(text: &str) -> bool {
    let norm = normalize_ar(text);
    SURAH_MAP
        .iter()
        .any(|(name, _)| name.chars().count() >= 2 && matches_word_bounded(&norm, name))
}

/// `intents.classify_intent` — quran → fatwa → surah mention → general.
pub fn classify_intent(text: &str) -> &'static str {
    for rx in QURAN_RES.iter() {
        if rx.is_match(text) {
            return QURAN;
        }
    }
    for rx in FATWA_RES.iter() {
        if rx.is_match(text) {
            return FATWA;
        }
    }
    if mentions_surah(text) {
        return QURAN;
    }
    GENERAL
}

// --- ayah references ----------------------------------------------------------

/// `intents.TOPIC_AYAH` — topic keyword → famous ayah (reference
/// index; the ayah text itself always comes from the Wave 4 Quran
/// provider, never from here).
pub const TOPIC_AYAH: &[(&str, (i32, i32))] = &[
    ("صبر", (2, 153)),
    ("الصبر", (2, 153)),
    ("شکر", (14, 7)),
    ("الشکر", (14, 7)),
    ("توکل", (65, 3)),
    ("التوکل", (65, 3)),
    ("دعا", (40, 60)),
    ("الدعاء", (40, 60)),
    ("رحمت", (39, 53)),
    ("الرحمه", (39, 53)),
    ("الرحمة", (39, 53)),
    ("والدین", (17, 23)),
    ("الوالدین", (17, 23)),
    ("پدر و مادر", (17, 23)),
    ("علم", (20, 114)),
    ("العلم", (20, 114)),
    ("عدالت", (16, 90)),
    ("العدل", (16, 90)),
    ("یتیم", (93, 9)),
    ("الیتیم", (93, 9)),
    ("نماز", (29, 45)),
    ("الصلاه", (29, 45)),
    ("الصلاة", (29, 45)),
    ("روزه", (2, 183)),
    ("الصوم", (2, 183)),
    ("الصیام", (2, 183)),
    ("کرسی", (2, 255)),
    ("الکرسی", (2, 255)),
    ("نور", (24, 35)),
    ("النور", (24, 35)),
];

static TOPIC_NORM: LazyLock<Vec<(String, (i32, i32))>> = LazyLock::new(|| {
    TOPIC_AYAH
        .iter()
        .map(|(k, v)| (normalize_ar(k), *v))
        .collect()
});

static EXPLICIT_REF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d{1,3})\s*[:/]\s*(\d{1,3})").expect("valid ref pattern"));
static AYAH_NUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:ایه|الایه)\s*(\d{1,3})").expect("valid ayah pattern"));
static SURAH_NUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:سوره|السوره)\s*(\d{1,3})").expect("valid surah pattern"));

/// `intents.extract_ayah_reference` — explicit `N:M` → «سوره X آیه Y»
/// → surah name alone (ayah 1) → topic keyword.
pub fn extract_ayah_reference(text: &str) -> Option<(i32, i32)> {
    let norm = normalize_ar(text);

    if let Some(caps) = EXPLICIT_REF_RE.captures(&norm) {
        let surah: i32 = caps[1].parse().ok()?;
        let ayah: i32 = caps[2].parse().ok()?;
        return Some((surah, ayah));
    }

    let mut ayah_num: Option<i32> = None;
    if let Some(caps) = AYAH_NUM_RE.captures(&norm) {
        ayah_num = caps[1].parse().ok();
    }

    let mut surah_num: Option<i32> = None;
    let mut surah_name_hit: Option<String> = None;
    if let Some(caps) = SURAH_NUM_RE.captures(&norm) {
        surah_num = caps[1].parse().ok();
    } else {
        // Longest matching name, so «نور» inside «النور» resolves right.
        let mut best: Option<(usize, i32, String)> = None;
        for (name, num) in SURAH_MAP.iter() {
            if name.chars().count() < 2 {
                continue;
            }
            if matches_word_bounded(&norm, name)
                && (best.is_none() || name.chars().count() > best.as_ref().unwrap().0)
            {
                best = Some((name.chars().count(), *num, name.clone()));
            }
        }
        if let Some((_, num, name)) = best {
            surah_num = Some(num);
            surah_name_hit = Some(name);
        }
    }

    if let Some(surah) = surah_num {
        if ayah_num.is_none() {
            if let Some(hit) = &surah_name_hit {
                // «بقره ۱۵۳»: the number after the surah name is the ayah.
                let rx = Regex::new(&format!("{}\\s*(\\d{{1,3}})", regex::escape(hit)))
                    .expect("valid name ref pattern");
                if let Some(caps) = rx.captures(&norm) {
                    ayah_num = caps[1].parse().ok();
                }
            }
        }
        return Some((surah, ayah_num.unwrap_or(1)));
    }

    let mut best_topic: Option<(usize, (i32, i32))> = None;
    for (keyword, r) in TOPIC_NORM.iter() {
        if norm.contains(keyword)
            && (best_topic.is_none() || keyword.chars().count() > best_topic.as_ref().unwrap().0)
        {
            best_topic = Some((keyword.chars().count(), *r));
        }
    }
    best_topic.map(|(_, r)| r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_language_fa_ar_unknown() {
        assert_eq!(detect_language("سلام، چطورید؟"), "fa");
        assert_eq!(detect_language("هل يجوز الصيام؟"), "ar");
        assert_eq!(detect_language("hello world"), "unknown");
        assert_eq!(detect_language(""), "unknown");
        // Persian ی/ک count for fa even in an otherwise Arabic sentence.
        assert_eq!(detect_language("کتاب نماز"), "fa");
    }

    #[test]
    fn normalize_ar_digits_and_forms() {
        assert_eq!(normalize_ar("۲۵۵"), "255");
        assert_eq!(normalize_ar("آیةُ"), "ایه");
        assert_eq!(normalize_ar("يكي"), "یکی");
        assert_eq!(normalize_ar("كتاب"), "کتاب");
        assert_eq!(normalize_ar("بِسْمِ اللَّهِ"), "بسم الله");
    }

    #[test]
    fn classify_intent_patterns() {
        assert_eq!(classify_intent("آیه الکرسی را بخوان"), "quran");
        assert_eq!(classify_intent("سوره بقره را بخوان"), "quran");
        assert_eq!(classify_intent("سوره بقره"), "quran");
        assert_eq!(classify_intent("حکم زکات چیست؟"), "fatwa");
        assert_eq!(classify_intent("آیا نماز واجب است؟"), "fatwa");
        assert_eq!(classify_intent("سلام، امروز هوا خوب است"), "general");
        assert_eq!(classify_intent("2:255"), "quran");
    }

    #[test]
    fn extract_ayah_reference_forms() {
        assert_eq!(extract_ayah_reference("2:255"), Some((2, 255)));
        assert_eq!(extract_ayah_reference("سوره بقره آیه ۲۵۵"), Some((2, 255)));
        assert_eq!(extract_ayah_reference("آیه الکرسی"), Some((2, 255)));
        assert_eq!(extract_ayah_reference("سوره فاتحه"), Some((1, 1)));
        assert_eq!(extract_ayah_reference("سوره 2 آیه 5"), Some((2, 5)));
        assert_eq!(extract_ayah_reference("درباره صبر بگو"), Some((2, 153)));
        assert_eq!(extract_ayah_reference("سلام"), None);
    }

    #[test]
    fn surah_fa_names_cover_all_114() {
        assert_eq!(surah_fa_name(1), Some("فاتحه"));
        assert_eq!(surah_fa_name(114), Some("ناس"));
        assert_eq!(surah_fa_name(115), None);
        assert_eq!(SURAH_FA_NAMES.len(), 114);
    }
}
