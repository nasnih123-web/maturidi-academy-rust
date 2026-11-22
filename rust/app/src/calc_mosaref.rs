use serde_json::{json, Map, Value};

use super::calc_decimal::CalcError;

const FATHA: char = '\u{064e}';
const DAMMA: char = '\u{064f}';
const KASRA: char = '\u{0650}';
const SUKOON: char = '\u{0652}';
const SHADDA: char = '\u{0651}';

const WEAK: &[char] = &['ا', 'و', 'ي', 'ى', 'ی'];
const HAMZA_SEATS: &[char] = &['ء', 'أ', 'إ', 'ؤ', 'ئ', 'آ'];

const MUJARRAD_BABS: &[&str] = &["faala", "faala2", "faala3", "faala4", "faala5", "faala6"];
const KNOWN_BABS: &[&str] = &[
    "faala",
    "faala2",
    "faala3",
    "faala4",
    "faala5",
    "faala6",
    "afala",
    "faala_m",
    "faaala",
    "tafa''ala",
    "tafaaala",
    "infaala",
    "iftaala",
    "istafala",
    "rubaai_mujarrad",
    "rubaai_tafa'lala",
];
const RARE_BABS: &[&str] = &[
    "if'alla",
    "if'aalla",
    "if'aw'ala",
    "if'awwala",
    "rubaai_if'anlala",
    "rubaai_if'allalla",
];
const INHERENTLY_LAZIM_BABS: &[&str] = &["faala5", "infaala", "rubaai_tafa'lala"];
const AMR_NAHI_ORDER: [usize; 14] = [6, 7, 8, 9, 10, 11, 0, 1, 2, 3, 4, 5, 12, 13];

fn mosaref_err(code: &'static str) -> CalcError {
    CalcError::new(code, format!("خطای صرف فعل ({code})"))
}

// --- char helpers (Python string slicing, by code point) ---

fn ch(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn from_chars(c: &[char]) -> String {
    c.iter().collect()
}

/// `s[:-n]` — drop the last `n` chars.
fn drop_last_n(s: &str, n: usize) -> String {
    let c = ch(s);
    let keep = c.len().saturating_sub(n);
    from_chars(&c[..keep])
}

/// `s[:-1]`.
fn drop_last(s: &str) -> String {
    drop_last_n(s, 1)
}

/// `s[-1:]` — the last char, or `""`.
fn last_char(s: &str) -> String {
    ch(s).last().map(|c| c.to_string()).unwrap_or_default()
}

/// `s[n:]` — skip the first `n` chars.
fn skip_chars(s: &str, n: usize) -> String {
    let c = ch(s);
    from_chars(&c[n.min(c.len())..])
}

/// `re.sub("^PREFIX", "", s)` — strip a literal prefix.
fn strip_prefix(s: &str, prefix: &str) -> String {
    s.strip_prefix(prefix).unwrap_or(s).to_string()
}

fn is_pure_weak(c: char) -> bool {
    WEAK.contains(&c)
}

fn is_hamza_seat(c: char) -> bool {
    HAMZA_SEATS.contains(&c)
}

// ---------------------------------------------------------------- hamza ---

fn hamza_seat_for(own_mark: char, prev_char: char, prev_mark: char) -> char {
    if (prev_char == 'ا' || prev_char == 'و' || prev_char == 'ي') && prev_mark == '\0' {
        if own_mark == KASRA {
            return 'ئ';
        }
        if own_mark == DAMMA {
            return 'ؤ';
        }
        return 'ء';
    }
    let strength = |m: char| match m {
        KASRA => 3,
        DAMMA => 2,
        FATHA => 1,
        _ => 0,
    };
    let winner = if strength(own_mark) >= strength(prev_mark) {
        own_mark
    } else {
        prev_mark
    };
    match winner {
        KASRA => 'ئ',
        DAMMA => 'ؤ',
        FATHA => 'أ',
        _ => 'ء',
    }
}

fn is_haraka(c: char) -> bool {
    matches!(c, FATHA | DAMMA | KASRA | SUKOON | SHADDA)
}

fn fix_hamza(text: &str, hamza_is_final: bool) -> String {
    let mut chars = ch(text);
    for i in 0..chars.len() {
        if chars[i] != 'ء' {
            continue;
        }
        if i == 0 {
            let own = if chars.len() > 1 && is_haraka(chars[1]) && chars[1] != SHADDA {
                chars[1]
            } else {
                '\0'
            };
            chars[i] = if own == KASRA { 'إ' } else { 'أ' };
            continue;
        }
        let (prev_char, prev_mark) = if is_haraka(chars[i - 1]) {
            let pm = if chars[i - 1] == SHADDA {
                '\0'
            } else {
                chars[i - 1]
            };
            let pc = if i >= 2 { chars[i - 2] } else { '\0' };
            (pc, pm)
        } else {
            (chars[i - 1], '\0')
        };
        let own = if hamza_is_final {
            '\0'
        } else if i + 1 < chars.len() && is_haraka(chars[i + 1]) && chars[i + 1] != SHADDA {
            chars[i + 1]
        } else {
            '\0'
        };
        chars[i] = hamza_seat_for(own, prev_char, prev_mark);
    }
    from_chars(&chars)
}

// ------------------------------------------------------- generic engines ---

fn conjugate_past(stem: &str) -> Vec<String> {
    let bare = drop_last(stem);
    vec![
        stem.to_string(),
        format!("{stem}ا"),
        format!("{bare}ُوا"),
        format!("{stem}تْ"),
        format!("{stem}تَا"),
        format!("{bare}ْنَ"),
        format!("{bare}ْتَ"),
        format!("{bare}ْتُمَا"),
        format!("{bare}ْتُمْ"),
        format!("{bare}ْتِ"),
        format!("{bare}ْتُمَا"),
        format!("{bare}ْتُنَّ"),
        format!("{bare}ْتُ"),
        format!("{bare}ْنَا"),
    ]
}

fn conjugate_present_marfoo(prefix_vowel: char, rest: &str) -> Vec<String> {
    let bare = drop_last(rest);
    let p = prefix_vowel;
    vec![
        format!("ي{p}{rest}"),
        format!("ي{p}{bare}َانِ"),
        format!("ي{p}{bare}ُونَ"),
        format!("ت{p}{rest}"),
        format!("ت{p}{bare}َانِ"),
        format!("ي{p}{bare}ْنَ"),
        format!("ت{p}{rest}"),
        format!("ت{p}{bare}َانِ"),
        format!("ت{p}{bare}ُونَ"),
        format!("ت{p}{bare}ِينَ"),
        format!("ت{p}{bare}َانِ"),
        format!("ت{p}{bare}ْنَ"),
        format!("أ{p}{rest}"),
        format!("ن{p}{rest}"),
    ]
}

fn conjugate_jussive(prefix_vowel: char, rest: &str) -> Vec<String> {
    let bare = drop_last(rest);
    let p = prefix_vowel;
    vec![
        format!("ي{p}{bare}ْ"),
        format!("ي{p}{bare}َا"),
        format!("ي{p}{bare}ُوا"),
        format!("ت{p}{bare}ْ"),
        format!("ت{p}{bare}َا"),
        format!("ي{p}{bare}ْنَ"),
        format!("ت{p}{bare}ْ"),
        format!("ت{p}{bare}َا"),
        format!("ت{p}{bare}ُوا"),
        format!("ت{p}{bare}ِي"),
        format!("ت{p}{bare}َا"),
        format!("ت{p}{bare}ْنَ"),
        format!("أ{p}{bare}ْ"),
        format!("ن{p}{bare}ْ"),
    ]
}

fn noun_forms(base: &str) -> Vec<String> {
    let ext = if last_char(base) == "ى" {
        format!("{}ي", drop_last(base))
    } else {
        base.to_string()
    };
    vec![
        base.to_string(),
        format!("{ext}َانِ"),
        format!("{ext}ُونَ"),
        format!("{ext}َة"),
        format!("{ext}َتَانِ"),
        format!("{ext}َات"),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ]
}

fn naqis_noun_forms(full_base: &str) -> Vec<String> {
    let consonant_base = drop_last_n(full_base, 2);
    vec![
        format!("{consonant_base}ٍ"),
        format!("{full_base}َانِ"),
        format!("{consonant_base}ُونَ"),
        format!("{full_base}َة"),
        format!("{full_base}َتَانِ"),
        format!("{full_base}َات"),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ]
}

// ---------------------------------------------------------- mudgham (ضعّف) ---

fn mudgham_past(f1: char, f2: char, f1_vowel: char, ain_vowel: char) -> Vec<String> {
    let sh = SHADDA;
    let m0 = format!("{f1}{f1_vowel}{f2}{sh}َ");
    let split = format!("{f1}{f1_vowel}{f2}{ain_vowel}{f2}");
    vec![
        m0.clone(),
        format!("{m0}ا"),
        format!("{f1}{f1_vowel}{f2}{sh}ُوا"),
        format!("{m0}تْ"),
        format!("{m0}تَا"),
        format!("{split}ْنَ"),
        format!("{split}ْتَ"),
        format!("{split}ْتُمَا"),
        format!("{split}ْتُمْ"),
        format!("{split}ْتِ"),
        format!("{split}ْتُمَا"),
        format!("{split}ْتُنَّ"),
        format!("{split}ْتُ"),
        format!("{split}ْنَا"),
    ]
}

fn mudgham_marfoo(prefix_vowel: char, f1: char, ain: char, f2: char) -> Vec<String> {
    let sh = SHADDA;
    let mm = format!("{f1}{ain}{f2}{sh}");
    let p = prefix_vowel;
    let sep = format!("{f1}ْ{f2}{ain}{f2}ْنَ");
    vec![
        format!("ي{p}{mm}ُ"),
        format!("ي{p}{mm}َانِ"),
        format!("ي{p}{mm}ُونَ"),
        format!("ت{p}{mm}ُ"),
        format!("ت{p}{mm}َانِ"),
        format!("ي{p}{sep}"),
        format!("ت{p}{mm}ُ"),
        format!("ت{p}{mm}َانِ"),
        format!("ت{p}{mm}ُونَ"),
        format!("ت{p}{mm}ِينَ"),
        format!("ت{p}{mm}َانِ"),
        format!("ت{p}{sep}"),
        format!("أ{p}{mm}ُ"),
        format!("ن{p}{mm}ُ"),
    ]
}

fn mudgham_jussive(prefix_vowel: char, f1: char, ain: char, f2: char) -> Vec<String> {
    let sh = SHADDA;
    let mm = format!("{f1}{ain}{f2}{sh}");
    let p = prefix_vowel;
    let sep = format!("{f1}ْ{f2}{ain}{f2}ْنَ");
    vec![
        format!("ي{p}{mm}َ"),
        format!("ي{p}{mm}َا"),
        format!("ي{p}{mm}ُوا"),
        format!("ت{p}{mm}َ"),
        format!("ت{p}{mm}َا"),
        format!("ي{p}{sep}"),
        format!("ت{p}{mm}َ"),
        format!("ت{p}{mm}َا"),
        format!("ت{p}{mm}ُوا"),
        format!("ت{p}{mm}ِي"),
        format!("ت{p}{mm}َا"),
        format!("ت{p}{sep}"),
        format!("أ{p}{mm}َ"),
        format!("ن{p}{mm}َ"),
    ]
}

// ------------------------------------------------------------ ajwaf (أجوف) ---

fn ajwaf_pres_arrays(
    long_fn: &dyn Fn(&str) -> String,
    short_fn: &dyn Fn(&str) -> String,
) -> (Vec<String>, Vec<String>) {
    let marfoo = vec![
        format!("{}ُ", long_fn("ي")),
        format!("{}َانِ", long_fn("ي")),
        format!("{}ُونَ", long_fn("ي")),
        format!("{}ُ", long_fn("ت")),
        format!("{}َانِ", long_fn("ت")),
        format!("{}ْنَ", short_fn("ي")),
        format!("{}ُ", long_fn("ت")),
        format!("{}َانِ", long_fn("ت")),
        format!("{}ُونَ", long_fn("ت")),
        format!("{}ِينَ", long_fn("ت")),
        format!("{}َانِ", long_fn("ت")),
        format!("{}ْنَ", short_fn("ت")),
        format!("{}ُ", long_fn("أ")),
        format!("{}ُ", long_fn("ن")),
    ];
    let jussive = vec![
        format!("{}ْ", short_fn("ي")),
        format!("{}َا", long_fn("ي")),
        format!("{}ُوا", long_fn("ي")),
        format!("{}ْ", short_fn("ت")),
        format!("{}َا", long_fn("ت")),
        format!("{}ْنَ", short_fn("ي")),
        format!("{}ْ", short_fn("ت")),
        format!("{}َا", long_fn("ت")),
        format!("{}ُوا", long_fn("ت")),
        format!("{}ِي", long_fn("ت")),
        format!("{}َا", long_fn("ت")),
        format!("{}ْنَ", short_fn("ت")),
        format!("{}ْ", short_fn("أ")),
        format!("{}ْ", short_fn("ن")),
    ];
    (marfoo, jussive)
}

#[derive(Default)]
struct VerbForms {
    past_active: Vec<String>,
    past_passive: Vec<String>,
    pres_active_marfoo: Vec<String>,
    pres_active_jussive: Vec<String>,
    pres_passive_marfoo: Vec<String>,
    pres_passive_jussive: Vec<String>,
    ism_fail: String,
    ism_maful: String,
    hamza: String,
    force_hamza_always: bool,
    custom_hamza: Option<String>,
}

fn build_ajwaf(f1: char, f3: char, _past_ain: char, pres_ain: char) -> VerbForms {
    let long_active = if pres_ain == 'ُ' {
        "و"
    } else if pres_ain == 'ِ' {
        "ي"
    } else {
        "ا"
    };
    let past_short = if pres_ain == 'ُ' { 'ُ' } else { 'ِ' };
    let long_stem = format!("{f1}َا{f3}َ");
    let short_stem = format!("{f1}{past_short}{f3}َ");
    let mut past_active = conjugate_past(&long_stem);
    past_active.truncate(5);
    past_active.extend_from_slice(&conjugate_past(&short_stem)[5..]);
    let long_stem_p = format!("{f1}ِي{f3}َ");
    let short_stem_p = format!("{f1}ِ{f3}َ");
    let mut past_passive = conjugate_past(&long_stem_p);
    past_passive.truncate(5);
    past_passive.extend_from_slice(&conjugate_past(&short_stem_p)[5..]);
    let pres_pair = |prefix_vowel: char, long_letter: &str, ain_vowel: char| {
        ajwaf_pres_arrays(
            &|p: &str| format!("{p}{prefix_vowel}{f1}{ain_vowel}{long_letter}{f3}"),
            &|p: &str| format!("{p}{prefix_vowel}{f1}{ain_vowel}{f3}"),
        )
    };
    let (pres_active_marfoo, pres_active_jussive) = pres_pair('َ', long_active, pres_ain);
    let (pres_passive_marfoo, pres_passive_jussive) = pres_pair('ُ', "ا", 'َ');
    VerbForms {
        past_active,
        past_passive,
        pres_active_marfoo,
        pres_active_jussive,
        pres_passive_marfoo,
        pres_passive_jussive,
        ism_fail: format!("{f1}َائِ{f3}"),
        ism_maful: format!("مَ{f1}{}{f3}", if pres_ain == 'ُ' { "و" } else { "ي" }),
        ..Default::default()
    }
}

fn build_ajwaf_iv(f1: char, f3: char) -> VerbForms {
    let mut past_active = conjugate_past(&format!("أَ{f1}َا{f3}َ"));
    past_active.truncate(5);
    past_active.extend_from_slice(&conjugate_past(&format!("أَ{f1}َ{f3}َ"))[5..]);
    let mut past_passive = conjugate_past(&format!("أُ{f1}ِي{f3}َ"));
    past_passive.truncate(5);
    past_passive.extend_from_slice(&conjugate_past(&format!("أُ{f1}ِ{f3}َ"))[5..]);
    let pres = |prefix_vowel: char, merge_vowel: char, merge_letter: &str| {
        ajwaf_pres_arrays(
            &|p: &str| format!("{p}{prefix_vowel}{f1}{merge_vowel}{merge_letter}{f3}"),
            &|p: &str| format!("{p}{prefix_vowel}{f1}{merge_vowel}{f3}"),
        )
    };
    let (pres_active_marfoo, pres_active_jussive) = pres('ُ', 'ِ', "ي");
    let (pres_passive_marfoo, pres_passive_jussive) = pres('ُ', 'َ', "ا");
    VerbForms {
        past_active,
        past_passive,
        pres_active_marfoo,
        pres_active_jussive,
        pres_passive_marfoo,
        pres_passive_jussive,
        ism_fail: format!("مُ{f1}ِي{f3}"),
        ism_maful: format!("مُ{f1}َا{f3}"),
        hamza: "أَ".to_string(),
        force_hamza_always: true,
        ..Default::default()
    }
}

fn build_ajwaf_viii(f1: char, f3: char) -> VerbForms {
    let mut past_active = conjugate_past(&format!("اِ{f1}ْتَا{f3}َ"));
    past_active.truncate(5);
    past_active.extend_from_slice(&conjugate_past(&format!("اِ{f1}ْتَ{f3}َ"))[5..]);
    let mut past_passive = conjugate_past(&format!("اُ{f1}ْتِي{f3}َ"));
    past_passive.truncate(5);
    past_passive.extend_from_slice(&conjugate_past(&format!("اُ{f1}ْتِ{f3}َ"))[5..]);
    let pres = |prefix_vowel: char| {
        ajwaf_pres_arrays(
            &|p: &str| format!("{p}{prefix_vowel}{f1}ْتَا{f3}"),
            &|p: &str| format!("{p}{prefix_vowel}{f1}ْتَ{f3}"),
        )
    };
    let (pres_active_marfoo, pres_active_jussive) = pres('َ');
    let (pres_passive_marfoo, pres_passive_jussive) = pres('ُ');
    VerbForms {
        past_active,
        past_passive,
        pres_active_marfoo,
        pres_active_jussive,
        pres_passive_marfoo,
        pres_passive_jussive,
        ism_fail: format!("مُ{f1}ْتَا{f3}"),
        ism_maful: format!("مُ{f1}ْتَا{f3}"),
        hamza: "اِ".to_string(),
        ..Default::default()
    }
}

fn build_ajwaf_x(f1: char, f3: char) -> VerbForms {
    let mut past_active = conjugate_past(&format!("اِسْتَ{f1}َا{f3}َ"));
    past_active.truncate(5);
    past_active.extend_from_slice(&conjugate_past(&format!("اِسْتَ{f1}َ{f3}َ"))[5..]);
    let mut past_passive = conjugate_past(&format!("اُسْتُ{f1}ِي{f3}َ"));
    past_passive.truncate(5);
    past_passive.extend_from_slice(&conjugate_past(&format!("اُسْتُ{f1}ِ{f3}َ"))[5..]);
    let pres = |prefix_vowel: char, merge_vowel: char, merge_letter: &str| {
        ajwaf_pres_arrays(
            &|p: &str| format!("{p}{prefix_vowel}سْتَ{f1}{merge_vowel}{merge_letter}{f3}"),
            &|p: &str| format!("{p}{prefix_vowel}سْتَ{f1}{merge_vowel}{f3}"),
        )
    };
    let (pres_active_marfoo, pres_active_jussive) = pres('َ', 'ِ', "ي");
    let (pres_passive_marfoo, pres_passive_jussive) = pres('ُ', 'َ', "ا");
    VerbForms {
        past_active,
        past_passive,
        pres_active_marfoo,
        pres_active_jussive,
        pres_passive_marfoo,
        pres_passive_jussive,
        ism_fail: format!("مُسْتَ{f1}ِي{f3}"),
        ism_maful: format!("مُسْتَ{f1}َا{f3}"),
        custom_hamza: Some("اِسْتَ".to_string()),
        ..Default::default()
    }
}

// ------------------------------------------------------------ naqis (ناقص) ---

fn naqis_pres_pair(ain: char, bare_fn: &dyn Fn(&str) -> String) -> (Vec<String>, Vec<String>) {
    let e_letter = if ain == 'ُ' {
        "و"
    } else if ain == 'ِ' {
        "ي"
    } else {
        "ى"
    };
    let g_letter = if ain == 'ُ' { "و" } else { "ي" };
    let m0 = |p: &str| format!("{}{}{}", bare_fn(p), ain, e_letter);
    let m_dual = |p: &str| format!("{}{}{}َانِ", bare_fn(p), ain, g_letter);
    let m_plural = |p: &str| {
        if ain == 'َ' {
            format!("{}َوْنَ", bare_fn(p))
        } else {
            format!("{}ُونَ", bare_fn(p))
        }
    };
    let m_fem_pl = |p: &str| format!("{}{}{}ْنَ", bare_fn(p), ain, g_letter);
    let marfoo = vec![
        m0("ي"),
        m_dual("ي"),
        m_plural("ي"),
        m0("ت"),
        m_dual("ت"),
        m_fem_pl("ي"),
        m0("ت"),
        m_dual("ت"),
        m_plural("ت"),
        format!("{}ِينَ", bare_fn("ت")),
        m_dual("ت"),
        m_fem_pl("ت"),
        m0("أ"),
        m0("ن"),
    ];
    let j0 = |p: &str| format!("{}{}", bare_fn(p), ain);
    let j_dual = |p: &str| format!("{}{}{}َا", bare_fn(p), ain, g_letter);
    let j_plural = |p: &str| {
        if ain == 'َ' {
            format!("{}َوْا", bare_fn(p))
        } else {
            format!("{}ُوا", bare_fn(p))
        }
    };
    let jussive = vec![
        j0("ي"),
        j_dual("ي"),
        j_plural("ي"),
        j0("ت"),
        j_dual("ت"),
        m_fem_pl("ي"),
        j0("ت"),
        j_dual("ت"),
        j_plural("ت"),
        format!("{}ِي", bare_fn("ت")),
        j_dual("ت"),
        m_fem_pl("ت"),
        j0("أ"),
        j0("ن"),
    ];
    (marfoo, jussive)
}

fn build_naqis(f1: char, f2: char, f3: char, pres_ain: char) -> VerbForms {
    let (pres_active_marfoo, pres_active_jussive) =
        naqis_pres_pair(pres_ain, &|p: &str| format!("{p}َ{f1}ْ{f2}"));
    let (pres_passive_marfoo, pres_passive_jussive) =
        naqis_pres_pair('َ', &|p: &str| format!("{p}ُ{f1}ْ{f2}"));
    VerbForms {
        pres_active_marfoo,
        pres_active_jussive,
        pres_passive_marfoo,
        pres_passive_jussive,
        ism_fail: format!("{f1}َا{f2}ِي"),
        ism_maful: format!("مَ{f1}ْ{f2}{}", if f3 == 'و' { "ُوّ" } else { "ِيّ" }),
        hamza: if pres_ain == 'ُ' { "اُ" } else { "اِ" }.to_string(),
        ..Default::default()
    }
}

fn build_naqis_past(f1: char, f2: char, f3: char) -> (Vec<String>, Vec<String>) {
    let long_ending = if f3 == 'و' { "ا" } else { "ى" };
    let a = format!("{f1}َ{f2}َ");
    let past_active = vec![
        format!("{a}{long_ending}"),
        format!("{a}{f3}َا"),
        format!("{a}وْا"),
        format!("{a}تْ"),
        format!("{a}تَا"),
        format!("{a}{f3}ْنَ"),
        format!("{a}{f3}ْتَ"),
        format!("{a}{f3}ْتُمَا"),
        format!("{a}{f3}ْتُمْ"),
        format!("{a}{f3}ْتِ"),
        format!("{a}{f3}ْتُمَا"),
        format!("{a}{f3}ْتُنَّ"),
        format!("{a}{f3}ْتُ"),
        format!("{a}{f3}ْنَا"),
    ];
    let p = format!("{f1}ُ{f2}ِي");
    let past_passive = vec![
        format!("{p}َ"),
        format!("{p}َا"),
        format!("{f1}ُ{f2}ُوا"),
        format!("{p}َتْ"),
        format!("{p}َتَا"),
        format!("{p}نَ"),
        format!("{f1}ُ{f2}ِيتَ"),
        format!("{f1}ُ{f2}ِيتُمَا"),
        format!("{f1}ُ{f2}ِيتُمْ"),
        format!("{f1}ُ{f2}ِيتِ"),
        format!("{f1}ُ{f2}ِيتُمَا"),
        format!("{f1}ُ{f2}ِيتُنَّ"),
        format!("{f1}ُ{f2}ِيتُ"),
        format!("{f1}ُ{f2}ِينَا"),
    ];
    (past_active, past_passive)
}

fn swap_last_vowel(text: &str, new_vowel: char) -> String {
    let c = ch(text);
    if c.last() == Some(&SHADDA) {
        let mut base = c[..c.len().saturating_sub(2)].to_vec();
        base.push(new_vowel);
        base.push(SHADDA);
        from_chars(&base)
    } else if !c.is_empty() {
        let mut base = c[..c.len() - 1].to_vec();
        base.push(new_vowel);
        from_chars(&base)
    } else {
        text.to_string()
    }
}

fn extract_ain(text: &str) -> (char, String) {
    let c = ch(text);
    if c.last() == Some(&SHADDA) {
        let ain = c[c.len() - 2];
        let mut bare: Vec<char> = c[..c.len() - 2].to_vec();
        bare.push(SHADDA);
        (ain, from_chars(&bare))
    } else {
        let ain = *c.last().unwrap_or(&'\0');
        (ain, from_chars(&c[..c.len().saturating_sub(1)]))
    }
}

fn build_naqis_from_measure(
    past_active_stem: &str,
    past_passive_stem: &str,
    pres_active_rest: &str,
    pres_active_prefix: char,
    pres_passive_rest: &str,
    pres_passive_prefix: char,
    f3: char,
) -> VerbForms {
    let past_base = drop_last_n(past_active_stem, 2);
    let long_ending = if f3 == 'و' { "ا" } else { "ى" };
    let past_active = vec![
        format!("{past_base}{long_ending}"),
        format!("{past_base}{f3}َا"),
        format!("{past_base}وْا"),
        format!("{past_base}تْ"),
        format!("{past_base}تَا"),
        format!("{past_base}{f3}ْنَ"),
        format!("{past_base}{f3}ْتَ"),
        format!("{past_base}{f3}ْتُمَا"),
        format!("{past_base}{f3}ْتُمْ"),
        format!("{past_base}{f3}ْتِ"),
        format!("{past_base}{f3}ْتُمَا"),
        format!("{past_base}{f3}ْتُنَّ"),
        format!("{past_base}{f3}ْتُ"),
        format!("{past_base}{f3}ْنَا"),
    ];
    let base_p = drop_last_n(past_passive_stem, 2);
    let base_p_damma = swap_last_vowel(&base_p, 'ُ');
    let past_passive = vec![
        format!("{base_p}يَ"),
        format!("{base_p}يَا"),
        format!("{base_p_damma}وا"),
        format!("{base_p}يَتْ"),
        format!("{base_p}يَتَا"),
        format!("{base_p}ينَ"),
        format!("{base_p}يتَ"),
        format!("{base_p}يتُمَا"),
        format!("{base_p}يتُمْ"),
        format!("{base_p}يتِ"),
        format!("{base_p}يتُمَا"),
        format!("{base_p}يتُنَّ"),
        format!("{base_p}يتُ"),
        format!("{base_p}ينَا"),
    ];
    let pres_pair_from_rest = |rest: &str, prefix_vowel: char| {
        let stripped = drop_last_n(rest, 2);
        let (ain, bare_no_ain) = extract_ain(&stripped);
        let (marfoo, jussive) =
            naqis_pres_pair(ain, &|p: &str| format!("{p}{prefix_vowel}{bare_no_ain}"));
        (marfoo, jussive, bare_no_ain)
    };
    let (pres_active_marfoo, pres_active_jussive, bare_a) =
        pres_pair_from_rest(pres_active_rest, pres_active_prefix);
    let (pres_passive_marfoo, pres_passive_jussive, bare_p) =
        pres_pair_from_rest(pres_passive_rest, pres_passive_prefix);
    VerbForms {
        past_active,
        past_passive,
        pres_active_marfoo,
        pres_active_jussive,
        pres_passive_marfoo,
        pres_passive_jussive,
        ism_fail: format!("مُ{bare_a}ِي"),
        ism_maful: format!("مُ{bare_p}َى"),
        ..Default::default()
    }
}

// ------------------------------------------------- lafif mafruq (لفيف مفروق) ---

fn build_lafif_mafruq_iftaala(f2: char, f3: char) -> VerbForms {
    let past_base = format!("اِتَّ{f2}َ");
    let long_ending = if f3 == 'و' { "ا" } else { "ى" };
    let past_active = vec![
        format!("{past_base}{long_ending}"),
        format!("{past_base}{f3}َا"),
        format!("{past_base}وْا"),
        format!("{past_base}تْ"),
        format!("{past_base}تَا"),
        format!("{past_base}{f3}ْنَ"),
        format!("{past_base}{f3}ْتَ"),
        format!("{past_base}{f3}ْتُمَا"),
        format!("{past_base}{f3}ْتُمْ"),
        format!("{past_base}{f3}ْتِ"),
        format!("{past_base}{f3}ْتُمَا"),
        format!("{past_base}{f3}ْتُنَّ"),
        format!("{past_base}{f3}ْتُ"),
        format!("{past_base}{f3}ْنَا"),
    ];
    let past_base_p = format!("اُتُّ{f2}ِ");
    let base_p_damma = swap_last_vowel(&past_base_p, 'ُ');
    let past_passive = vec![
        format!("{past_base_p}يَ"),
        format!("{past_base_p}يَا"),
        format!("{base_p_damma}وا"),
        format!("{past_base_p}يَتْ"),
        format!("{past_base_p}يَتَا"),
        format!("{past_base_p}ينَ"),
        format!("{past_base_p}يتَ"),
        format!("{past_base_p}يتُمَا"),
        format!("{past_base_p}يتُمْ"),
        format!("{past_base_p}يتِ"),
        format!("{past_base_p}يتُمَا"),
        format!("{past_base_p}يتُنَّ"),
        format!("{past_base_p}يتُ"),
        format!("{past_base_p}ينَا"),
    ];
    let pres_pair = |prefix_vowel: char, bare_no_ain: String, ain: char| {
        naqis_pres_pair(ain, &|p: &str| format!("{p}{prefix_vowel}{bare_no_ain}"))
    };
    let (pres_active_marfoo, pres_active_jussive) = pres_pair('َ', format!("تَّ{f2}"), 'ِ');
    let (pres_passive_marfoo, pres_passive_jussive) = pres_pair('ُ', format!("تَّ{f2}"), 'َ');
    VerbForms {
        past_active,
        past_passive,
        pres_active_marfoo,
        pres_active_jussive,
        pres_passive_marfoo,
        pres_passive_jussive,
        ism_fail: format!("مُتَّ{f2}ِي"),
        ism_maful: format!("مُتَّ{f2}َى"),
        hamza: "اِ".to_string(),
        force_hamza_always: true,
        ..Default::default()
    }
}

// ---------------------------------------------------------------- اوزان ---

fn mujarrad_ain(bab: &str) -> Option<(char, char)> {
    Some(match bab {
        "faala" => ('َ', 'ُ'),
        "faala2" => ('َ', 'ِ'),
        "faala3" => ('َ', 'َ'),
        "faala4" => ('ِ', 'َ'),
        "faala5" => ('ُ', 'ُ'),
        "faala6" => ('ِ', 'ِ'),
        _ => return None,
    })
}

#[derive(Default)]
struct Measure {
    past_active: String,
    pres_prefix: char,
    pres_active: String,
    past_passive: String,
    pres_passive_prefix: char,
    pres_passive: String,
    hamza: String,
    ism_fail: String,
    ism_maful: String,
    has_passive: bool,
    has_ism_zaman: bool,
    custom_hamza: Option<String>,
    ism_alat: String,
    ism_tafdil: String,
    irregular: bool,
}

impl Measure {
    fn new() -> Self {
        Self {
            pres_prefix: 'َ',
            pres_passive_prefix: 'ُ',
            has_passive: true,
            ..Default::default()
        }
    }
}

fn get_measure(bab: &str, f1: char, f2: char, f3: char, r4: char) -> Measure {
    let mut m = Measure::new();
    if let Some((past_ain, pres_ain)) = mujarrad_ain(bab) {
        m.past_active = format!("{f1}َ{f2}{past_ain}{f3}َ");
        m.pres_prefix = 'َ';
        m.pres_active = format!("{f1}ْ{f2}{pres_ain}{f3}ُ");
        m.past_passive = format!("{f1}ُ{f2}ِ{f3}َ");
        m.pres_passive_prefix = 'ُ';
        m.pres_passive = format!("{f1}ْ{f2}َ{f3}ُ");
        m.hamza = if pres_ain == 'ُ' { "اُ" } else { "اِ" }.to_string();
        m.ism_fail = format!("{f1}َا{f2}ِ{f3}");
        m.ism_maful = format!("مَ{f1}ْ{f2}ُو{f3}");
        m.has_ism_zaman = true;
        m.ism_alat = format!("مِ{f1}ْ{f2}َا{f3}");
        m.ism_tafdil = format!("أَ{f1}ْ{f2}َ{f3}");
        if bab == "faala5" {
            m.has_passive = false;
        }
    } else if bab == "afala" {
        m.past_active = format!("أَ{f1}ْ{f2}َ{f3}َ");
        m.pres_prefix = 'ُ';
        m.pres_active = format!("{f1}ْ{f2}ِ{f3}ُ");
        m.past_passive = format!("أُ{f1}ْ{f2}ِ{f3}َ");
        m.pres_passive_prefix = 'ُ';
        m.pres_passive = format!("{f1}ْ{f2}َ{f3}ُ");
        m.hamza = "أَ".to_string();
        m.ism_fail = format!("مُ{f1}ْ{f2}ِ{f3}");
        m.ism_maful = format!("مُ{f1}ْ{f2}َ{f3}");
    } else if bab == "faala_m" {
        m.past_active = format!("{f1}َ{f2}َّ{f3}َ");
        m.pres_prefix = 'ُ';
        m.pres_active = format!("{f1}َ{f2}ِّ{f3}ُ");
        m.past_passive = format!("{f1}ُ{f2}ِّ{f3}َ");
        m.pres_passive_prefix = 'ُ';
        m.pres_passive = format!("{f1}َ{f2}َّ{f3}ُ");
        m.hamza = String::new();
        m.ism_fail = format!("مُ{f1}َ{f2}ِّ{f3}");
        m.ism_maful = format!("مُ{f1}َ{f2}َّ{f3}");
    } else if bab == "faaala" {
        m.past_active = format!("{f1}َا{f2}َ{f3}َ");
        m.pres_prefix = 'ُ';
        m.pres_active = format!("{f1}َا{f2}ِ{f3}ُ");
        m.past_passive = format!("{f1}ُو{f2}ِ{f3}َ");
        m.pres_passive_prefix = 'ُ';
        m.pres_passive = format!("{f1}َا{f2}َ{f3}ُ");
        m.hamza = String::new();
        m.ism_fail = format!("مُ{f1}َا{f2}ِ{f3}");
        m.ism_maful = format!("مُ{f1}َا{f2}َ{f3}");
    } else if bab == "iftaala" {
        m.past_active = format!("اِ{f1}ْتَ{f2}َ{f3}َ");
        m.pres_prefix = 'َ';
        m.pres_active = format!("{f1}ْتَ{f2}ِ{f3}ُ");
        m.past_passive = format!("اُ{f1}ْتُ{f2}ِ{f3}َ");
        m.pres_passive_prefix = 'ُ';
        m.pres_passive = format!("{f1}ْتَ{f2}َ{f3}ُ");
        m.hamza = "اِ".to_string();
        m.ism_fail = format!("مُ{f1}ْتَ{f2}ِ{f3}");
        m.ism_maful = format!("مُ{f1}ْتَ{f2}َ{f3}");
    } else if bab == "istafala" {
        m.past_active = format!("اِسْتَ{f1}ْ{f2}َ{f3}َ");
        m.pres_prefix = 'َ';
        m.pres_active = format!("سْتَ{f1}ْ{f2}ِ{f3}ُ");
        m.past_passive = format!("اُسْتُ{f1}ْ{f2}ِ{f3}َ");
        m.pres_passive_prefix = 'ُ';
        m.pres_passive = format!("سْتَ{f1}ْ{f2}َ{f3}ُ");
        m.hamza = "اِس".to_string();
        m.ism_fail = format!("مُسْتَ{f1}ْ{f2}ِ{f3}");
        m.ism_maful = format!("مُسْتَ{f1}ْ{f2}َ{f3}");
        m.custom_hamza = Some("اِسْتَ".to_string());
    } else if bab == "infaala" {
        m.past_active = format!("اِنْ{f1}َ{f2}َ{f3}َ");
        m.pres_prefix = 'َ';
        m.pres_active = format!("نْ{f1}َ{f2}ِ{f3}ُ");
        m.has_passive = false;
        m.past_passive = format!("اُنْ{f1}ُ{f2}ِ{f3}َ");
        m.pres_passive_prefix = 'ُ';
        m.pres_passive = format!("نْ{f1}َ{f2}َ{f3}ُ");
        m.hamza = "اِ".to_string();
        m.ism_fail = format!("مُنْ{f1}َ{f2}ِ{f3}");
        m.ism_maful = format!("مُنْ{f1}َ{f2}َ{f3}");
    } else if bab == "tafaaala" {
        m.past_active = format!("تَ{f1}َا{f2}َ{f3}َ");
        m.pres_prefix = 'َ';
        m.pres_active = format!("تَ{f1}َا{f2}َ{f3}ُ");
        m.past_passive = format!("تُ{f1}ُو{f2}ِ{f3}َ");
        m.pres_passive_prefix = 'ُ';
        m.pres_passive = format!("تَ{f1}َا{f2}َ{f3}ُ");
        m.hamza = String::new();
        m.ism_fail = format!("مُتَ{f1}َا{f2}ِ{f3}");
        m.ism_maful = format!("مُتَ{f1}َا{f2}َ{f3}");
    } else if bab == "tafa''ala" {
        m.past_active = format!("تَ{f1}َ{f2}َّ{f3}َ");
        m.pres_prefix = 'َ';
        m.pres_active = format!("تَ{f1}َ{f2}َّ{f3}ُ");
        m.past_passive = format!("تُ{f1}ُ{f2}ِّ{f3}َ");
        m.pres_passive_prefix = 'ُ';
        m.pres_passive = format!("تَ{f1}َ{f2}َّ{f3}ُ");
        m.hamza = String::new();
        m.ism_fail = format!("مُتَ{f1}َ{f2}ِّ{f3}");
        m.ism_maful = format!("مُتَ{f1}َ{f2}َّ{f3}");
    } else if bab == "rubaai_mujarrad" {
        m.past_active = format!("{f1}َ{f2}ْ{f3}َ{r4}َ");
        m.pres_prefix = 'ُ';
        m.pres_active = format!("{f1}َ{f2}ْ{f3}ِ{r4}ُ");
        m.past_passive = format!("{f1}ُ{f2}ْ{f3}ِ{r4}َ");
        m.pres_passive_prefix = 'ُ';
        m.pres_passive = format!("{f1}َ{f2}ْ{f3}َ{r4}ُ");
        m.hamza = String::new();
        m.ism_fail = format!("مُ{f1}َ{f2}ْ{f3}ِ{r4}");
        m.ism_maful = format!("مُ{f1}َ{f2}ْ{f3}َ{r4}");
    } else if bab == "rubaai_tafa'lala" {
        m.past_active = format!("تَ{f1}َ{f2}ْ{f3}َ{r4}َ");
        m.pres_prefix = 'َ';
        m.pres_active = format!("تَ{f1}َ{f2}ْ{f3}َ{r4}ُ");
        m.past_passive = format!("تُ{f1}ُ{f2}ْ{f3}ِ{r4}َ");
        m.pres_passive_prefix = 'ُ';
        m.pres_passive = format!("تَ{f1}َ{f2}ْ{f3}َ{r4}ُ");
        m.hamza = String::new();
        m.ism_fail = format!("مُتَ{f1}َ{f2}ْ{f3}ِ{r4}");
        m.ism_maful = format!("مُتَ{f1}َ{f2}ْ{f3}َ{r4}");
        m.has_passive = false;
    } else {
        m.irregular = true;
    }
    m
}

// ------------------------------- اسم زمان/مکان، آلت، تفضیل (derived) ---

fn zaman_makan_triple(
    kind: &str,
    f1: char,
    f2: char,
    f3: char,
    pres_ain: char,
    force_kasra: bool,
) -> Vec<String> {
    match kind {
        "sound" => {
            let kasra = force_kasra || pres_ain == 'ِ';
            let sg = format!("مَ{f1}ْ{f2}{}{f3}", if kasra { 'ِ' } else { 'َ' });
            vec![sg.clone(), format!("{sg}َانِ"), format!("مَ{f1}َا{f2}ِ{f3}")]
        }
        "naqis" => {
            let long_ending = if f3 == 'و' { "ا" } else { "ى" };
            vec![
                format!("مَ{f1}ْ{f2}َ{long_ending}"),
                format!("مَ{f1}ْ{f2}َ{f3}َانِ"),
                format!("مَ{f1}َا{f2}ٍ"),
            ]
        }
        "ajwaf" => {
            let sg = format!("مَ{f1}َا{f3}");
            vec![sg.clone(), format!("{sg}َانِ"), format!("{sg}َات")]
        }
        "mudgham" => {
            let sg = format!("مَ{f1}{}{f2}{SHADDA}", if pres_ain == 'ِ' { 'ِ' } else { 'َ' });
            vec![sg.clone(), format!("{sg}َانِ"), format!("مَ{f1}َا{f2}{SHADDA}")]
        }
        _ => vec![String::new(), String::new(), String::new()],
    }
}

fn alat_triple(kind: &str, f1: char, f2: char, f3: char) -> Vec<String> {
    match kind {
        "sound" => {
            let sg = format!("مِ{f1}ْ{f2}َا{f3}");
            vec![sg.clone(), format!("{sg}َانِ"), format!("مَ{f1}َا{f2}ِي{f3}")]
        }
        "mudgham" => {
            let sg = format!("مِ{f1}ْ{f2}َا{f2}");
            vec![sg.clone(), format!("{sg}َانِ"), format!("مَ{f1}َا{f2}ِي{f2}")]
        }
        "naqis" => {
            let prefix = if is_pure_weak(f1) {
                "مِي".to_string()
            } else {
                format!("مِ{f1}ْ")
            };
            vec![
                format!("{prefix}{f2}َاءٌ"),
                format!("{prefix}{f2}َاوَانِ"),
                format!("مَ{f1}َا{f2}ِيُّ"),
            ]
        }
        _ => vec![String::new(), String::new(), String::new()],
    }
}

fn tafdil_six(kind: &str, f1: char, f2: char, f3: char) -> Vec<String> {
    match kind {
        "sound" => {
            let m_sg = format!("أَ{f1}ْ{f2}َ{f3}");
            let f_base = format!("{f1}ُ{f2}ْ{f3}");
            vec![
                m_sg.clone(),
                format!("{m_sg}َانِ"),
                format!("{m_sg}ُونَ"),
                format!("{f_base}َى"),
                format!("{f_base}َيَانِ"),
                format!("{f_base}َيَات"),
            ]
        }
        "mudgham" => {
            let m_sg = format!("أَ{f1}َ{f2}{SHADDA}");
            let f_base = format!("{f1}ُ{f2}{SHADDA}");
            vec![
                m_sg.clone(),
                format!("{m_sg}َانِ"),
                format!("{m_sg}ُونَ"),
                format!("{f_base}َى"),
                format!("{f_base}َيَانِ"),
                format!("{f_base}َيَات"),
            ]
        }
        "naqis" => {
            let long_ending = if f3 == 'و' { "ا" } else { "ى" };
            let m_sg = format!("أَ{f1}ْ{f2}َ{long_ending}");
            let m_dual = format!("أَ{f1}ْ{f2}َ{f3}َانِ");
            let m_plural = format!("أَ{f1}ْ{f2}َوْنَ");
            let f_base = format!("{f1}ُ{f2}ْ{f3}َ");
            vec![
                m_sg,
                m_dual,
                m_plural,
                format!("{f_base}ا"),
                format!("{f_base}يَانِ"),
                format!("{f_base}يَات"),
            ]
        }
        _ => vec![String::new(); 6],
    }
}

// ------------------------------------------------- مجهول با حرف جر ---

fn harf_jar_forms(harf_jar: &str) -> &'static [&'static str] {
    match harf_jar {
        "bi" => &[
            "بِهِ",
            "بِهِمَا",
            "بِهِمْ",
            "بِهَا",
            "بِهِمَا",
            "بِهِنَّ",
            "بِكَ",
            "بِكُمَا",
            "بِكُمْ",
            "بِكِ",
            "بِكُمَا",
            "بِكُنَّ",
            "بِي",
            "بِنَا",
        ],
        "li" => &[
            "لَهُ",
            "لَهُمَا",
            "لَهُمْ",
            "لَهَا",
            "لَهُمَا",
            "لَهُنَّ",
            "لَكَ",
            "لَكُمَا",
            "لَكُمْ",
            "لَكِ",
            "لَكُمَا",
            "لَكُنَّ",
            "لِي",
            "لَنَا",
        ],
        "an" => &[
            "عَنْهُ",
            "عَنْهُمَا",
            "عَنْهُمْ",
            "عَنْهَا",
            "عَنْهُمَا",
            "عَنْهُنَّ",
            "عَنْكَ",
            "عَنْكُمَا",
            "عَنْكُمْ",
            "عَنْكِ",
            "عَنْكُمَا",
            "عَنْكُنَّ",
            "عَنِّي",
            "عَنَّا",
        ],
        "min" => &[
            "مِنْهُ",
            "مِنْهُمَا",
            "مِنْهُمْ",
            "مِنْهَا",
            "مِنْهُمَا",
            "مِنْهُنَّ",
            "مِنْكَ",
            "مِنْكُمَا",
            "مِنْكُمْ",
            "مِنْكِ",
            "مِنْكُمَا",
            "مِنْكُنَّ",
            "مِنِّي",
            "مِنَّا",
        ],
        "ila" => &[
            "إِلَيْهِ",
            "إِلَيْهِمَا",
            "إِلَيْهِمْ",
            "إِلَيْهَا",
            "إِلَيْهِمَا",
            "إِلَيْهِنَّ",
            "إِلَيْكَ",
            "إِلَيْكُمَا",
            "إِلَيْكُمْ",
            "إِلَيْكِ",
            "إِلَيْكُمَا",
            "إِلَيْكُنَّ",
            "إِلَيَّ",
            "إِلَيْنَا",
        ],
        "ala" => &[
            "عَلَيْهِ",
            "عَلَيْهِمَا",
            "عَلَيْهِمْ",
            "عَلَيْهَا",
            "عَلَيْهِمَا",
            "عَلَيْهِنَّ",
            "عَلَيْكَ",
            "عَلَيْكُمَا",
            "عَلَيْكُمْ",
            "عَلَيْكِ",
            "عَلَيْكُمَا",
            "عَلَيْكُنَّ",
            "عَلَيَّ",
            "عَلَيْنَا",
        ],
        "fi" => &[
            "فِيهِ",
            "فِيهِمَا",
            "فِيهِمْ",
            "فِيهَا",
            "فِيهِمَا",
            "فِيهِنَّ",
            "فِيكَ",
            "فِيكُمَا",
            "فِيكُمْ",
            "فِيكِ",
            "فِيكُمَا",
            "فِيكُنَّ",
            "فِيَّ",
            "فِينَا",
        ],
        _ => &[],
    }
}

fn harf_jar_arrays(
    harf_jar: &str,
    past3: &str,
    pres3: &str,
    juss3: &str,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let forms = harf_jar_forms(harf_jar);
    (
        forms.iter().map(|f| format!("{past3} {f}")).collect(),
        forms.iter().map(|f| format!("{pres3} {f}")).collect(),
        forms.iter().map(|f| format!("{juss3} {f}")).collect(),
    )
}

// ------------------------------------------------------- ساخت جدول‌ها ---

fn reorder(arr: &[String]) -> Vec<String> {
    AMR_NAHI_ORDER.iter().map(|&i| arr[i].clone()).collect()
}

fn combine_amr(amr_no_lam: &[String], lam_amr_active: &[String]) -> Vec<String> {
    let mut combined = lam_amr_active.to_vec();
    combined[6..12].clone_from_slice(&amr_no_lam[6..12]);
    combined
}

fn table(key: &str, columns: &[(&str, Vec<String>)]) -> Value {
    let mut cols = Map::new();
    for (name, forms) in columns {
        cols.insert(name.to_string(), json!(forms));
    }
    json!({"key": key, "columns": cols})
}

fn verb_tables(
    past_a: &[String],
    past_p: &[String],
    pres_a: &[String],
    pres_p: &[String],
    juss_a: &[String],
    juss_p: &[String],
    amr_combined: &[String],
) -> Vec<Value> {
    let lam_a: Vec<String> = juss_a.iter().map(|v| format!("لِ{v}")).collect();
    let lam_p: Vec<String> = juss_p.iter().map(|v| format!("لِ{v}")).collect();
    let pre = |forms: &[String], prefix: &str| -> Vec<String> {
        forms.iter().map(|v| format!("{prefix}{v}")).collect()
    };
    vec![
        table(
            "past",
            &[("active", past_a.to_vec()), ("passive", past_p.to_vec())],
        ),
        table(
            "present",
            &[("active", pres_a.to_vec()), ("passive", pres_p.to_vec())],
        ),
        table(
            "nafi",
            &[
                ("active", pre(pres_a, "لَا ")),
                ("passive", pre(pres_p, "لَا ")),
            ],
        ),
        table(
            "jahd",
            &[
                ("active", pre(juss_a, "لَمْ ")),
                ("passive", pre(juss_p, "لَمْ ")),
            ],
        ),
        table(
            "lam_amr",
            &[("active", lam_a.clone()), ("passive", lam_p.clone())],
        ),
        table(
            "amr_nahi",
            &[
                ("amr_active", reorder(amr_combined)),
                ("amr_passive", reorder(&lam_p)),
                ("nahi_active", reorder(&pre(juss_a, "لَا "))),
                ("nahi_passive", reorder(&pre(juss_p, "لَا "))),
            ],
        ),
    ]
}

fn fail_maful_table(ism_fail: Vec<String>, ism_maful: Vec<String>, show_passive: bool) -> Value {
    let mut cols: Vec<(&str, Vec<String>)> = vec![("ism_fail", ism_fail)];
    if show_passive {
        cols.push(("ism_maful", ism_maful));
    }
    table("ism_fail_maful", &cols)
}

fn harf_jar_maful_table(ism_maful_base: &str, harf_jar: &str) -> Value {
    let forms: Vec<String> = harf_jar_forms(harf_jar)
        .iter()
        .map(|f| format!("{ism_maful_base} {f}"))
        .collect();
    table("ism_maful_harf_jar", &[("ism_maful", forms)])
}

fn derived_block(
    zaman3: Vec<String>,
    alat3: Vec<String>,
    tafdil6: Vec<String>,
    show_alat: bool,
) -> Vec<Value> {
    let mut tables = vec![table("zaman_makan", &[("form", zaman3)])];
    if show_alat {
        tables.push(table("alat", &[("form", alat3)]));
    }
    tables.push(table("tafdil", &[("form", tafdil6)]));
    tables
}

fn zaman_same_table(ism_maful: &str) -> Value {
    table(
        "zaman_same_as_maful",
        &[("form", vec![ism_maful.to_string()])],
    )
}

fn passive_or_harf_jar(
    show_passive: bool,
    harf_jar: &str,
    raw_past_p: &[String],
    raw_pres_p: &[String],
    raw_juss_p: &[String],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    if show_passive {
        (
            raw_past_p.to_vec(),
            raw_pres_p.to_vec(),
            raw_juss_p.to_vec(),
        )
    } else {
        harf_jar_arrays(harf_jar, &raw_past_p[0], &raw_pres_p[0], &raw_juss_p[0])
    }
}

fn amr_from_jussive(juss_active: &[String], decide: &dyn Fn(&str, usize) -> String) -> Vec<String> {
    let mut amr = vec![String::new(); 14];
    for i in 6..12 {
        amr[i] = decide(&skip_chars(&juss_active[i], 2), i);
    }
    amr
}

fn starts_with_sukoon(core: &str) -> bool {
    let c = ch(core);
    c.len() > 1 && c[1] == SUKOON
}

// ---------------------------------------------------------------- رندرها ---

#[derive(Default)]
struct MainFlags {
    mesal_dropped: bool,
    mesal_assimilated: bool,
    ajwaf_as_sound: bool,
}

fn render_mudgham(bab: &str, f1: char, f2: char, trans: &str, harf_jar: &str) -> Vec<Value> {
    let (past_ain, pres_ain) = mujarrad_ain(bab).unwrap();
    let is_laazim = trans == "laazim" || INHERENTLY_LAZIM_BABS.contains(&bab);
    let hamza_vowel = if pres_ain == 'ُ' { "اُ" } else { "اِ" };
    let past_a = mudgham_past(f1, f2, 'َ', past_ain);
    let raw_past_p = mudgham_past(f1, f2, 'ُ', 'ِ');
    let pres_a = mudgham_marfoo('َ', f1, pres_ain, f2);
    let raw_pres_p = mudgham_marfoo('ُ', f1, 'َ', f2);
    let juss_a = mudgham_jussive('َ', f1, pres_ain, f2);
    let raw_juss_p = mudgham_jussive('ُ', f1, 'َ', f2);
    let (past_p, pres_p, juss_p) =
        passive_or_harf_jar(!is_laazim, harf_jar, &raw_past_p, &raw_pres_p, &raw_juss_p);
    let lam_a: Vec<String> = juss_a.iter().map(|v| format!("لِ{v}")).collect();
    let amr = amr_from_jussive(&juss_a, &|core, i| {
        if i == 11 {
            format!("{hamza_vowel}{core}")
        } else {
            core.to_string()
        }
    });
    let mut tables = verb_tables(
        &past_a,
        &past_p,
        &pres_a,
        &pres_p,
        &juss_a,
        &juss_p,
        &combine_amr(&amr, &lam_a),
    );
    let ism_maful_base = format!("مَ{f1}ْ{f2}ُو{f2}");
    tables.push(fail_maful_table(
        noun_forms(&format!("{f1}َا{f2}ّ")),
        noun_forms(&ism_maful_base),
        !is_laazim,
    ));
    if is_laazim {
        tables.push(harf_jar_maful_table(&ism_maful_base, harf_jar));
    }
    tables.extend(derived_block(
        zaman_makan_triple("mudgham", f1, f2, f2, pres_ain, false),
        alat_triple("mudgham", f1, f2, f2),
        tafdil_six("mudgham", f1, f2, f2),
        !is_laazim,
    ));
    tables
}

fn render_ajwaf(
    bab: &str,
    f1: char,
    f2: char,
    f3: char,
    trans: &str,
    harf_jar: &str,
) -> Vec<Value> {
    let (past_ain, pres_ain) = mujarrad_ain(bab).unwrap();
    let is_laazim = trans == "laazim" || INHERENTLY_LAZIM_BABS.contains(&bab);
    let aw = build_ajwaf(f1, f3, past_ain, pres_ain);
    let (past_p, pres_p, juss_p) = passive_or_harf_jar(
        !is_laazim,
        harf_jar,
        &aw.past_passive,
        &aw.pres_passive_marfoo,
        &aw.pres_passive_jussive,
    );
    let lam_a: Vec<String> = aw
        .pres_active_jussive
        .iter()
        .map(|v| format!("لِ{v}"))
        .collect();
    let amr = amr_from_jussive(&aw.pres_active_jussive, &|core, _| core.to_string());
    let mut tables = verb_tables(
        &aw.past_active,
        &past_p,
        &aw.pres_active_marfoo,
        &pres_p,
        &aw.pres_active_jussive,
        &juss_p,
        &combine_amr(&amr, &lam_a),
    );
    tables.push(fail_maful_table(
        noun_forms(&aw.ism_fail),
        noun_forms(&aw.ism_maful),
        !is_laazim,
    ));
    if is_laazim {
        tables.push(harf_jar_maful_table(&aw.ism_maful, harf_jar));
    }
    tables.push(table(
        "zaman_makan",
        &[(
            "form",
            zaman_makan_triple("ajwaf", f1, f2, f3, pres_ain, false),
        )],
    ));
    if !is_laazim {
        let alat3 = vec![
            format!("مِ{f1}ْ{f2}َا{f3}"),
            format!("مِ{f1}ْ{f2}َا{f3}َانِ"),
            format!("مَ{f1}َا{f2}ِي{f3}"),
        ];
        tables.push(table("alat", &[("form", alat3)]));
        tables.push(table(
            "tafdil",
            &[("form", tafdil_six("sound", f1, f2, f3))],
        ));
    }
    tables
}

#[allow(clippy::too_many_arguments)]
fn naqis_family_tables(
    nq: &VerbForms,
    past_a: &[String],
    raw_past_p: &[String],
    show_passive: bool,
    harf_jar: &str,
    amr_decide: &dyn Fn(&str, usize) -> String,
    ism_fail_forms: Vec<String>,
    extra_tables: Vec<Value>,
) -> Vec<Value> {
    let (past_p, pres_p, juss_p) = passive_or_harf_jar(
        show_passive,
        harf_jar,
        raw_past_p,
        &nq.pres_passive_marfoo,
        &nq.pres_passive_jussive,
    );
    let lam_a: Vec<String> = nq
        .pres_active_jussive
        .iter()
        .map(|v| format!("لِ{v}"))
        .collect();
    let amr = amr_from_jussive(&nq.pres_active_jussive, amr_decide);
    let mut tables = verb_tables(
        past_a,
        &past_p,
        &nq.pres_active_marfoo,
        &pres_p,
        &nq.pres_active_jussive,
        &juss_p,
        &combine_amr(&amr, &lam_a),
    );
    tables.push(fail_maful_table(
        ism_fail_forms,
        noun_forms(&nq.ism_maful),
        show_passive,
    ));
    if !show_passive {
        tables.push(harf_jar_maful_table(&nq.ism_maful, harf_jar));
    }
    tables.extend(extra_tables);
    tables
}

fn render_naqis(
    bab: &str,
    f1: char,
    f2: char,
    f3: char,
    trans: &str,
    harf_jar: &str,
) -> Vec<Value> {
    let (_, pres_ain) = mujarrad_ain(bab).unwrap();
    let show_passive = trans != "laazim" && !INHERENTLY_LAZIM_BABS.contains(&bab);
    let nq = build_naqis(f1, f2, f3, pres_ain);
    let (past_a, raw_past_p) = if matches!(bab, "faala" | "faala2" | "faala3") {
        build_naqis_past(f1, f2, f3)
    } else {
        let sm = get_measure(bab, f1, f2, f3, '\0');
        (
            conjugate_past(&sm.past_active),
            conjugate_past(&sm.past_passive),
        )
    };
    let decide = |core: &str, _i: usize| {
        if starts_with_sukoon(core) {
            format!("{}{core}", nq.hamza)
        } else {
            core.to_string()
        }
    };
    let extra = derived_block(
        zaman_makan_triple("naqis", f1, f2, f3, pres_ain, false),
        alat_triple("naqis", f1, f2, f3),
        tafdil_six("naqis", f1, f2, f3),
        true,
    );
    naqis_family_tables(
        &nq,
        &past_a,
        &raw_past_p,
        show_passive,
        harf_jar,
        &decide,
        naqis_noun_forms(&nq.ism_fail),
        extra,
    )
}

fn render_lafif_mafruq(
    bab: &str,
    f1: char,
    f2: char,
    f3: char,
    trans: &str,
    harf_jar: &str,
) -> Vec<Value> {
    let (_, pres_ain) = mujarrad_ain(bab).unwrap();
    let show_passive = trans != "laazim" && !INHERENTLY_LAZIM_BABS.contains(&bab);
    let mut nq = build_naqis(f1, f2, f3, pres_ain);
    let drop_f1 = f1 == 'و' && matches!(bab, "faala" | "faala2" | "faala3");
    if drop_f1 {
        let (pres_a, juss_a) = naqis_pres_pair(pres_ain, &|p: &str| format!("{p}َ{f2}"));
        nq.pres_active_marfoo = pres_a;
        nq.pres_active_jussive = juss_a;
    }
    let (past_a, raw_past_p) = if matches!(bab, "faala" | "faala2" | "faala3") {
        build_naqis_past(f1, f2, f3)
    } else {
        let sm = get_measure(bab, f1, f2, f3, '\0');
        (
            conjugate_past(&sm.past_active),
            conjugate_past(&sm.past_passive),
        )
    };
    let decide = |core: &str, _i: usize| {
        if starts_with_sukoon(core) {
            format!("{}{core}", nq.hamza)
        } else {
            core.to_string()
        }
    };
    let extra = derived_block(
        zaman_makan_triple("naqis", f1, f2, f3, pres_ain, false),
        alat_triple("naqis", f1, f2, f3),
        tafdil_six("naqis", f1, f2, f3),
        true,
    );
    naqis_family_tables(
        &nq,
        &past_a,
        &raw_past_p,
        show_passive,
        harf_jar,
        &decide,
        naqis_noun_forms(&nq.ism_fail),
        extra,
    )
}

fn render_ajwaf_augmented(
    bab: &str,
    f1: char,
    f3: char,
    trans: &str,
    harf_jar: &str,
) -> Vec<Value> {
    let show_passive = trans != "laazim";
    let aw = match bab {
        "afala" => build_ajwaf_iv(f1, f3),
        "iftaala" => build_ajwaf_viii(f1, f3),
        _ => build_ajwaf_x(f1, f3),
    };
    let (past_p, pres_p, juss_p) = passive_or_harf_jar(
        show_passive,
        harf_jar,
        &aw.past_passive,
        &aw.pres_passive_marfoo,
        &aw.pres_passive_jussive,
    );
    let lam_a: Vec<String> = aw
        .pres_active_jussive
        .iter()
        .map(|v| format!("لِ{v}"))
        .collect();
    let decide = |core: &str, _i: usize| {
        if aw.custom_hamza.is_some() {
            format!("اِسْتَ{}", strip_prefix(core, "سْتَ"))
        } else if (aw.force_hamza_always || starts_with_sukoon(core)) && !aw.hamza.is_empty() {
            format!("{}{core}", aw.hamza)
        } else {
            core.to_string()
        }
    };
    let amr = amr_from_jussive(&aw.pres_active_jussive, &decide);
    let mut tables = verb_tables(
        &aw.past_active,
        &past_p,
        &aw.pres_active_marfoo,
        &pres_p,
        &aw.pres_active_jussive,
        &juss_p,
        &combine_amr(&amr, &lam_a),
    );
    tables.push(fail_maful_table(
        noun_forms(&aw.ism_fail),
        noun_forms(&aw.ism_maful),
        show_passive,
    ));
    if !show_passive {
        tables.push(harf_jar_maful_table(&aw.ism_maful, harf_jar));
    }
    tables.push(zaman_same_table(&aw.ism_maful));
    tables
}

fn render_naqis_augmented(
    bab: &str,
    f1: char,
    f2: char,
    f3: char,
    trans: &str,
    harf_jar: &str,
) -> Vec<Value> {
    let m = get_measure(bab, f1, f2, f3, '\0');
    let is_laazim = trans == "laazim";
    let show_passive = m.has_passive && !is_laazim;
    let nq = build_naqis_from_measure(
        &m.past_active,
        &m.past_passive,
        &m.pres_active,
        m.pres_prefix,
        &m.pres_passive,
        m.pres_passive_prefix,
        f3,
    );
    let (past_p, pres_p, juss_p) = passive_or_harf_jar(
        show_passive,
        harf_jar,
        &nq.past_passive,
        &nq.pres_passive_marfoo,
        &nq.pres_passive_jussive,
    );
    let lam_a: Vec<String> = nq
        .pres_active_jussive
        .iter()
        .map(|v| format!("لِ{v}"))
        .collect();
    let force_hamza_always = bab == "afala";
    let decide = |core: &str, _i: usize| {
        if m.custom_hamza.is_some() {
            format!("اِسْتَ{}", strip_prefix(core, "سْتَ"))
        } else if (force_hamza_always || starts_with_sukoon(core)) && !m.hamza.is_empty() {
            format!("{}{core}", m.hamza)
        } else {
            core.to_string()
        }
    };
    let amr = amr_from_jussive(&nq.pres_active_jussive, &decide);
    let mut tables = verb_tables(
        &nq.past_active,
        &past_p,
        &nq.pres_active_marfoo,
        &pres_p,
        &nq.pres_active_jussive,
        &juss_p,
        &combine_amr(&amr, &lam_a),
    );
    tables.push(fail_maful_table(
        noun_forms(&nq.ism_fail),
        noun_forms(&nq.ism_maful),
        show_passive,
    ));
    if !show_passive {
        tables.push(harf_jar_maful_table(&nq.ism_maful, harf_jar));
    }
    tables
}

fn render_lafif_mafruq_iftaala(f2: char, f3: char, trans: &str, harf_jar: &str) -> Vec<Value> {
    let show_passive = trans != "laazim";
    let lm = build_lafif_mafruq_iftaala(f2, f3);
    let (past_p, pres_p, juss_p) = passive_or_harf_jar(
        show_passive,
        harf_jar,
        &lm.past_passive,
        &lm.pres_passive_marfoo,
        &lm.pres_passive_jussive,
    );
    let lam_a: Vec<String> = lm
        .pres_active_jussive
        .iter()
        .map(|v| format!("لِ{v}"))
        .collect();
    let decide = |core: &str, _i: usize| {
        if lm.force_hamza_always {
            format!("{}{core}", lm.hamza)
        } else {
            core.to_string()
        }
    };
    let amr = amr_from_jussive(&lm.pres_active_jussive, &decide);
    let mut tables = verb_tables(
        &lm.past_active,
        &past_p,
        &lm.pres_active_marfoo,
        &pres_p,
        &lm.pres_active_jussive,
        &juss_p,
        &combine_amr(&amr, &lam_a),
    );
    tables.push(fail_maful_table(
        noun_forms(&lm.ism_fail),
        noun_forms(&lm.ism_maful),
        show_passive,
    ));
    if !show_passive {
        tables.push(harf_jar_maful_table(&lm.ism_maful, harf_jar));
    }
    tables
}

#[allow(clippy::too_many_arguments)]
fn render_main(
    bab: &str,
    f1: char,
    f2: char,
    f3: char,
    r4: char,
    trans: &str,
    harf_jar: &str,
    is_mesal: bool,
    is_mahmuz: bool,
) -> (Vec<Value>, MainFlags) {
    let mut m = get_measure(bab, f1, f2, f3, r4);
    let mut force_hamza_always = false;
    let mut flags = MainFlags::default();

    if is_mesal && f1 == 'و' && matches!(bab, "faala" | "faala2" | "faala3") {
        let (_, pres_ain) = mujarrad_ain(bab).unwrap();
        m.pres_active = format!("{f2}{pres_ain}{f3}ُ");
        flags.mesal_dropped = true;
    }
    if is_mesal && f1 == 'و' && bab == "iftaala" {
        m.past_active = format!("اِتَّ{f2}َ{f3}َ");
        m.pres_active = format!("تَّ{f2}ِ{f3}ُ");
        m.past_passive = format!("اُتُّ{f2}ِ{f3}َ");
        m.pres_passive = format!("تَّ{f2}َ{f3}ُ");
        m.ism_fail = format!("مُتَّ{f2}ِ{f3}");
        m.ism_maful = format!("مُتَّ{f2}َ{f3}");
        m.hamza = "اِ".to_string();
        force_hamza_always = true;
        flags.mesal_assimilated = true;
    }

    let show_passive = m.has_passive && trans != "laazim";
    let past_a = conjugate_past(&m.past_active);
    let raw_past_p = conjugate_past(&m.past_passive);
    let pres_a = conjugate_present_marfoo(m.pres_prefix, &m.pres_active);
    let raw_pres_p = conjugate_present_marfoo(m.pres_passive_prefix, &m.pres_passive);
    let juss_a = conjugate_jussive(m.pres_prefix, &m.pres_active);
    let raw_juss_p = conjugate_jussive(m.pres_passive_prefix, &m.pres_passive);
    let (past_p, pres_p, juss_p) = passive_or_harf_jar(
        show_passive,
        harf_jar,
        &raw_past_p,
        &raw_pres_p,
        &raw_juss_p,
    );
    let lam_a: Vec<String> = juss_a.iter().map(|v| format!("لِ{v}")).collect();

    let mahmuz_fa = is_mahmuz && f1 == 'ء';
    let drops_amr = mahmuz_fa
        && mujarrad_ain(bab)
            .map(|(_, pres_ain)| pres_ain == 'ُ')
            .unwrap_or(false);
    let decide = |core: &str, _i: usize| {
        if drops_amr {
            strip_prefix(core, "ءْ")
        } else if m.custom_hamza.is_some() {
            format!("اِسْتَ{}", strip_prefix(core, "سْتَ"))
        } else if (force_hamza_always || starts_with_sukoon(core)) && !m.hamza.is_empty() {
            format!("{}{core}", m.hamza)
        } else {
            core.to_string()
        }
    };
    let amr = amr_from_jussive(&juss_a, &decide);
    let mut tables = verb_tables(
        &past_a,
        &past_p,
        &pres_a,
        &pres_p,
        &juss_a,
        &juss_p,
        &combine_amr(&amr, &lam_a),
    );
    tables.push(fail_maful_table(
        noun_forms(&m.ism_fail),
        noun_forms(&m.ism_maful),
        show_passive,
    ));
    if !show_passive {
        tables.push(harf_jar_maful_table(&m.ism_maful, harf_jar));
    }
    if m.has_ism_zaman {
        let (_, pres_ain) = mujarrad_ain(bab).unwrap();
        tables.extend(derived_block(
            zaman_makan_triple("sound", f1, f2, f3, pres_ain, is_mesal),
            alat_triple("sound", f1, f2, f3),
            tafdil_six("sound", f1, f2, f3),
            show_passive,
        ));
    } else {
        tables.push(zaman_same_table(&m.ism_maful));
    }

    if is_mahmuz {
        let fix = |forms: &[Value]| -> Vec<Value> {
            forms
                .iter()
                .map(|form| {
                    let s = form.as_str().unwrap_or("");
                    let s = s.replace("أَءْ", "آ").replace("أُءْ", "أُو").replace("أِءْ", "إِي");
                    Value::String(fix_hamza(&s, f3 == 'ء'))
                })
                .collect()
        };
        for table in tables.iter_mut() {
            if let Some(cols) = table.get_mut("columns").and_then(|c| c.as_object_mut()) {
                for (_, forms) in cols.iter_mut() {
                    if let Some(arr) = forms.as_array() {
                        *forms = Value::Array(fix(arr));
                    }
                }
            }
        }
    }
    (tables, flags)
}

// ---------------------------------------------------------------- ورودی ---

/// Full conjugation of one verb — the public entry point, mirroring
/// `conjugate_verb` in `mosaref_engine.py`.
pub fn conjugate_verb(
    root: &str,
    bab: &str,
    transitivity: &str,
    harf_jar: &str,
) -> Result<Value, CalcError> {
    if transitivity != "mutaadi" && transitivity != "laazim" {
        return Err(mosaref_err("invalid_transitivity"));
    }
    if harf_jar_forms(harf_jar).is_empty() {
        return Err(mosaref_err("invalid_harf_jar"));
    }
    let root = root.trim();
    let is_rubaai = bab.starts_with("rubaai");
    let min_len = if is_rubaai { 4 } else { 3 };
    let root_chars = ch(root);
    if root.is_empty() || root_chars.len() < min_len {
        return Err(mosaref_err("root_too_short"));
    }
    let mut f1 = root_chars[0];
    let mut f2 = root_chars[1];
    let mut f3 = root_chars[2];
    let mut r4 = if is_rubaai { root_chars[3] } else { '\0' };
    let raw: Vec<char> = if is_rubaai {
        vec![f1, f2, f3, r4]
    } else {
        vec![f1, f2, f3]
    };
    if raw.contains(&'ا') {
        return Err(mosaref_err("alif_in_root"));
    }
    let is_mahmuz = raw.iter().any(|&c| is_hamza_seat(c));
    if is_mahmuz {
        if raw.iter().any(|&c| is_pure_weak(c)) {
            return Err(mosaref_err("mahmuz_weak_mix"));
        }
        if f2 == f3 && raw.iter().any(|&c| is_hamza_seat(c)) {
            return Err(mosaref_err("mahmuz_mudgham_mix"));
        }
        if is_hamza_seat(f1) {
            f1 = 'ء';
        }
        if is_hamza_seat(f2) {
            f2 = 'ء';
        }
        if is_hamza_seat(f3) {
            f3 = 'ء';
        }
        if is_rubaai && is_hamza_seat(r4) {
            r4 = 'ء';
        }
    }
    let letters: Vec<char> = if is_rubaai {
        vec![f1, f2, f3, r4]
    } else {
        vec![f1, f2, f3]
    };
    if is_rubaai && letters.iter().any(|&c| is_pure_weak(c)) {
        return Err(mosaref_err("rubaai_weak_unsupported"));
    }
    if !is_rubaai && is_pure_weak(f1) && is_pure_weak(f2) && !is_pure_weak(f3) {
        return Err(mosaref_err("weak_combo_unsupported"));
    }
    if RARE_BABS.contains(&bab) || !KNOWN_BABS.contains(&bab) {
        return Err(mosaref_err("bab_unsupported"));
    }

    let is_mudgham = !is_rubaai && f2 == f3;
    let is_ajwaf = !is_rubaai && is_pure_weak(f2) && !is_pure_weak(f1) && !is_pure_weak(f3);
    let is_lafif_mafruq = !is_rubaai && is_pure_weak(f1) && is_pure_weak(f3) && !is_pure_weak(f2);
    let is_mesal = !is_rubaai && is_pure_weak(f1) && !is_pure_weak(f2) && !is_pure_weak(f3);
    let is_naqis = !is_rubaai && is_pure_weak(f3) && !is_pure_weak(f1);
    let is_lafif_maqrun = is_naqis && is_pure_weak(f2);

    if is_mudgham && !MUJARRAD_BABS.contains(&bab) {
        return Err(mosaref_err("mudgham_augmented_unsupported"));
    }

    let mut flags = Map::new();
    flags.insert(
        "inherently_lazim".to_string(),
        json!(INHERENTLY_LAZIM_BABS.contains(&bab)),
    );
    flags.insert("approximate_past".to_string(), json!(false));

    let (verb_class, tables);
    if is_mudgham {
        verb_class = "mudgham";
        tables = render_mudgham(bab, f1, f2, transitivity, harf_jar);
    } else if is_ajwaf && matches!(bab, "afala" | "iftaala" | "istafala") {
        verb_class = "ajwaf";
        tables = render_ajwaf_augmented(bab, f1, f3, transitivity, harf_jar);
    } else if is_ajwaf && MUJARRAD_BABS.contains(&bab) {
        verb_class = "ajwaf";
        tables = render_ajwaf(bab, f1, f2, f3, transitivity, harf_jar);
    } else if is_lafif_mafruq && MUJARRAD_BABS.contains(&bab) {
        verb_class = "lafif_mafruq";
        tables = render_lafif_mafruq(bab, f1, f2, f3, transitivity, harf_jar);
        flags.insert(
            "approximate_past".to_string(),
            json!(!matches!(bab, "faala" | "faala2" | "faala3")),
        );
    } else if is_lafif_mafruq && bab == "iftaala" && f1 == 'و' {
        verb_class = "lafif_mafruq";
        tables = render_lafif_mafruq_iftaala(f2, f3, transitivity, harf_jar);
    } else if is_lafif_mafruq {
        verb_class = "lafif_mafruq";
        tables = render_naqis_augmented(bab, f1, f2, f3, transitivity, harf_jar);
    } else if is_naqis && MUJARRAD_BABS.contains(&bab) {
        verb_class = if is_lafif_maqrun {
            "lafif_maqrun"
        } else {
            "naqis"
        };
        tables = render_naqis(bab, f1, f2, f3, transitivity, harf_jar);
        flags.insert(
            "approximate_past".to_string(),
            json!(!matches!(bab, "faala" | "faala2" | "faala3")),
        );
    } else if is_naqis {
        verb_class = if is_lafif_maqrun {
            "lafif_maqrun"
        } else {
            "naqis"
        };
        tables = render_naqis_augmented(bab, f1, f2, f3, transitivity, harf_jar);
    } else {
        verb_class = if is_mesal {
            "mesal"
        } else if is_mahmuz {
            "mahmuz"
        } else if is_ajwaf {
            "ajwaf"
        } else {
            "sound"
        };
        let (t, main_flags) = render_main(
            bab,
            f1,
            f2,
            f3,
            r4,
            transitivity,
            harf_jar,
            is_mesal,
            is_mahmuz,
        );
        tables = t;
        flags.insert("mesal_dropped".to_string(), json!(main_flags.mesal_dropped));
        flags.insert(
            "mesal_assimilated".to_string(),
            json!(main_flags.mesal_assimilated),
        );
        flags.insert(
            "ajwaf_as_sound".to_string(),
            json!(if is_ajwaf {
                true
            } else {
                main_flags.ajwaf_as_sound
            }),
        );
    }

    Ok(json!({
        "root": from_chars(&letters),
        "bab": bab,
        "transitivity": transitivity,
        "harf_jar": harf_jar,
        "verb_class": verb_class,
        "tables": tables,
        "flags": flags,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col<'a>(out: &'a Value, key: &str, name: &str) -> &'a Vec<Value> {
        out["tables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["key"] == key)
            .unwrap()["columns"][name]
            .as_array()
            .unwrap()
    }

    /// Golden fixture `sound_faala_ktb` (mosaref_golden.json).
    #[test]
    fn golden_ktb_sound() {
        let out = conjugate_verb("كتب", "faala", "mutaadi", "bi").unwrap();
        assert_eq!(out["verb_class"], "sound");
        assert_eq!(col(&out, "past", "active")[0], "كَتَبَ");
        assert_eq!(col(&out, "present", "active")[0], "يَكْتُبُ");
        assert_eq!(col(&out, "ism_fail_maful", "ism_fail")[0], "كَاتِب");
        assert_eq!(col(&out, "zaman_makan", "form")[0], "مَكْتَب");
        assert_eq!(col(&out, "tafdil", "form")[0], "أَكْتَب");
        assert_eq!(out["flags"]["inherently_lazim"], false);
    }

    #[test]
    fn golden_classes_spot_checks() {
        // mudgham_faala_mdd
        let out = conjugate_verb("مدد", "faala", "mutaadi", "bi").unwrap();
        assert_eq!(out["verb_class"], "mudgham");
        assert_eq!(
            col(&out, "past", "active")[0],
            "\u{645}\u{64e}\u{62f}\u{651}\u{64e}"
        );
        assert_eq!(
            col(&out, "present", "active")[0],
            "\u{64a}\u{64e}\u{645}\u{64f}\u{62f}\u{651}\u{64f}"
        );
        // ajwaf_qwl
        let out = conjugate_verb("قول", "faala", "mutaadi", "bi").unwrap();
        assert_eq!(out["verb_class"], "ajwaf");
        assert_eq!(col(&out, "past", "active")[0], "قَالَ");
        assert_eq!(col(&out, "present", "active")[0], "يَقُولُ");
        // naqis_daw
        let out = conjugate_verb("دعو", "faala", "mutaadi", "bi").unwrap();
        assert_eq!(out["verb_class"], "naqis");
        assert_eq!(col(&out, "past", "active")[0], "دَعَا");
        assert_eq!(col(&out, "present", "active")[0], "يَدْعُو");
        // naqis_rmy
        let out = conjugate_verb("رمي", "faala2", "mutaadi", "bi").unwrap();
        assert_eq!(out["verb_class"], "naqis");
        assert_eq!(col(&out, "past", "active")[0], "رَمَى");
        assert_eq!(col(&out, "present", "active")[0], "يَرْمِي");
        // lafif_mafruq_wqy
        let out = conjugate_verb("وقي", "faala2", "mutaadi", "bi").unwrap();
        assert_eq!(out["verb_class"], "lafif_mafruq");
        assert_eq!(col(&out, "past", "active")[0], "وَقَى");
        assert_eq!(col(&out, "present", "active")[0], "يَقِي");
        // mesal_wad (waw dropped in the present)
        let out = conjugate_verb("وعد", "faala", "mutaadi", "bi").unwrap();
        assert_eq!(out["verb_class"], "mesal");
        assert_eq!(out["flags"]["mesal_dropped"], true);
        assert_eq!(col(&out, "past", "active")[0], "وَعَدَ");
        assert_eq!(col(&out, "present", "active")[0], "يَعُدُ");
        // mahmuz_fa_akhz
        let out = conjugate_verb("أخذ", "faala", "mutaadi", "bi").unwrap();
        assert_eq!(out["verb_class"], "mahmuz");
        assert_eq!(col(&out, "past", "active")[0], "أَخَذَ");
        // mahmuz_lam_qra
        let out = conjugate_verb("قرأ", "faala", "mutaadi", "bi").unwrap();
        assert_eq!(out["verb_class"], "mahmuz");
        // sound_lazim_khrj + harf_jar table
        let out = conjugate_verb("خرج", "faala", "laazim", "bi").unwrap();
        assert!(out["tables"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["key"] == "ism_maful_harf_jar"));
        // sound_lazim_ila
        let out = conjugate_verb("خرج", "faala", "laazim", "ila").unwrap();
        assert_eq!(
            col(&out, "ism_maful_harf_jar", "ism_maful")[0],
            "مَخْرُوج إِلَيْهِ"
        );
        // rubaai_dhrj
        let out = conjugate_verb("دحرج", "rubaai_mujarrad", "mutaadi", "bi").unwrap();
        assert_eq!(out["verb_class"], "sound");
        assert_eq!(col(&out, "past", "active")[0], "دَحْرَجَ");
        // sound_faala5_hsn — inherently lazim
        let out = conjugate_verb("حسن", "faala5", "laazim", "bi").unwrap();
        assert_eq!(out["flags"]["inherently_lazim"], true);
    }

    #[test]
    fn golden_error_codes() {
        let err = conjugate_verb("اب", "faala", "mutaadi", "bi").unwrap_err();
        assert_eq!(err.code, "root_too_short");
        let err = conjugate_verb("دعا", "faala", "mutaadi", "bi").unwrap_err();
        assert_eq!(err.code, "alif_in_root");
        let err = conjugate_verb("أوي", "faala", "mutaadi", "bi").unwrap_err();
        assert_eq!(err.code, "mahmuz_weak_mix");
        let err = conjugate_verb("كتب", "if'alla", "mutaadi", "bi").unwrap_err();
        assert_eq!(err.code, "bab_unsupported");
        let err = conjugate_verb("مدد", "afala", "mutaadi", "bi").unwrap_err();
        assert_eq!(err.code, "mudgham_augmented_unsupported");
        let err = conjugate_verb("كتب", "faala", "weird", "bi").unwrap_err();
        assert_eq!(err.code, "invalid_transitivity");
        let err = conjugate_verb("كتب", "faala", "mutaadi", "ma").unwrap_err();
        assert_eq!(err.code, "invalid_harf_jar");
    }
}
