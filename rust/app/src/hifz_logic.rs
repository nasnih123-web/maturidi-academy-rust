//! Pure Hifz logic — no I/O: Arabic normalization, Tanzil text,
//! SM-2 SRS, test scoring, tasmi state machines, plan ordering and
//! test-passage building. Mirrors `hifz_plan.py`, `hifz_testing.py`
//! (`build_passages`, `load_mushaf_lines`), `hifz_voice_search.py`
//! (`normalize_arabic`) and `hifz_mutashabihat.py` (via
//! `crate::hifz_mutashabihat`, which reuses `tanzil_ayahs` here).
//!
//! Two deliberate divergences from CPython, both recorded in the
//! ADRs: (1) test-passage sampling uses a SplitMix64 PRNG, not
//! CPython's Mersenne Twister — the seed (first 8 bytes of the test
//! UUID, little-endian) and the wire shape are identical, but the
//! sampled passages may differ from Python's; (2) floats are rounded
//! with Rust's `round()` (half away from zero) where Python uses
//! banker's rounding — see ADR-024.

use std::sync::OnceLock;

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use crate::errors::ApiError;
use crate::quran_data::get_surah;

// --- Arabic normalization (hifz_voice_search.normalize_arabic) -----------

fn is_tashkeel(c: char) -> bool {
    matches!(c, '\u{064B}'..='\u{0652}' | '\u{0670}' | '\u{06D6}'..='\u{06ED}')
}

fn has_arabic_letter(word: &str) -> bool {
    word.chars().any(|c| {
        matches!(c,
            '\u{0621}'..='\u{064A}' | '\u{0671}'..='\u{06D3}' | '\u{06FA}'..='\u{06FC}' | '\u{06FF}')
    })
}

/// Strip tashkeel + tatweel, fold alef/yeh/kaf/teh-marbuta variants.
pub fn normalize_arabic(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_tashkeel(c) || c == '\u{0640}' {
            continue;
        }
        let folded = match c {
            '\u{0671}' | '\u{0622}' | '\u{0623}' | '\u{0625}' => '\u{0627}',
            '\u{0649}' | '\u{06CC}' => '\u{064A}',
            '\u{06A9}' => '\u{0643}',
            '\u{0629}' => '\u{0647}',
            other => other,
        };
        out.push(folded);
    }
    out
}

// --- Tanzil text -------------------------------------------------------------

/// One ayah: (surah, ayah, normalized words, original Tanzil text).
pub type TanzilAyah = (i32, i32, Vec<String>, String);

static TANZIL: OnceLock<Vec<TanzilAyah>> = OnceLock::new();

/// The bundled Tanzil Uthmani text (CC BY 3.0), parsed once.
pub fn tanzil_ayahs() -> &'static [TanzilAyah] {
    TANZIL.get_or_init(|| {
        let raw = include_str!("../data/quran/tanzil_uthmani.txt");
        let mut ayahs = Vec::with_capacity(6236);
        for line in raw.split('\n') {
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                continue;
            }
            let mut parts = line.splitn(3, '|');
            let (Some(s), Some(a), Some(text)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let (Ok(surah), Ok(ayah)) = (s.parse::<i32>(), a.parse::<i32>()) else {
                continue;
            };
            let words: Vec<String> = text
                .split_whitespace()
                .map(normalize_arabic)
                .filter(|w| has_arabic_letter(w))
                .collect();
            if !words.is_empty() {
                ayahs.push((surah, ayah, words, text.to_string()));
            }
        }
        ayahs
    })
}

/// Uthmani words of one ayah (whitespace split, original form), or
/// `None` when the ayah has no text — the 503 `quran_text_unavailable`
/// case in grading.
pub fn ayah_words(surah: i32, ayah: i32) -> Option<Vec<String>> {
    tanzil_ayahs()
        .iter()
        .find(|(s, a, _, _)| *s == surah && *a == ayah)
        .map(|(_, _, _, text)| text.split_whitespace().map(|w| w.to_string()).collect())
}

// --- SM-2 (hifz_plan.py) ------------------------------------------------------

pub const SM2_MIN_EASE: f64 = 1.3;
pub const SM2_LAPSE_QUALITY: i64 = 3;
pub const SM2_FIRST_INTERVAL: i64 = 1;
pub const SM2_SECOND_INTERVAL: i64 = 6;
pub const SM2_EASY_BONUS: f64 = 1.3;
pub const SM2_LAPSE_EASE_PENALTY: f64 = 0.2;

/// `quality_from_grade(counts, peek_count, total_words)` — 0..=5.
pub fn quality_from_grade(
    correct: i64,
    close: i64,
    wrong: i64,
    skipped: i64,
    peek_count: i64,
    total_words: Option<i64>,
) -> i64 {
    let total = total_words.unwrap_or(correct + close + wrong + skipped);
    if total <= 0 {
        return 0;
    }
    let quality = if wrong + skipped == 0 && close == 0 {
        5
    } else if wrong + skipped == 0 {
        if close <= (1).max(total / 4) {
            4
        } else {
            3
        }
    } else if correct == 0 && close == 0 {
        0
    } else if wrong >= total / 2 || skipped >= total / 2 {
        1
    } else if wrong + skipped <= (1).max(total / 4) {
        if close > 0 {
            2
        } else {
            3
        }
    } else {
        2
    };
    let penalty = (peek_count).min(2);
    (quality - penalty).max(0)
}

/// One SM-2 step. Returns `(repetitions, ease, interval_days, due_at)`.
pub fn sm2_next(
    quality: i64,
    repetitions: i64,
    ease: f64,
    interval_days: i64,
    now: DateTime<Utc>,
) -> (i64, f64, i64, DateTime<Utc>) {
    let mut ease = (SM2_MIN_EASE)
        .max(ease + (0.1 - (5 - quality) as f64 * (0.08 + (5 - quality) as f64 * 0.02)));
    let (repetitions, interval) = if quality < SM2_LAPSE_QUALITY {
        ease = (SM2_MIN_EASE).max(ease - SM2_LAPSE_EASE_PENALTY);
        (0, 1)
    } else {
        let reps = repetitions + 1;
        let interval = if reps == 1 {
            SM2_FIRST_INTERVAL
        } else if reps == 2 {
            SM2_SECOND_INTERVAL
        } else {
            let mut iv = (interval_days as f64 * ease).ceil() as i64;
            if quality == 5 {
                iv = (iv as f64 * SM2_EASY_BONUS).ceil() as i64;
            }
            iv
        };
        (reps, interval)
    };
    let interval = interval.max(1);
    (
        repetitions,
        (ease * 10_000.0).round() / 10_000.0,
        interval,
        now + Duration::days(interval),
    )
}

// --- test scoring (hifz_plan.test_question_score / test_total_score) --------

/// Score one test question from its raw events (non-removed only).
/// Returns `(score, [correct, close, wrong, skipped], peek_count)`.
pub fn test_question_score(raw_events: &[Value]) -> (Option<f64>, [i64; 4], i64) {
    let mut counts = [0i64; 4];
    let mut peeks = 0i64;
    let mut seen_peeks: Vec<(i64, i64, i64)> = Vec::new();
    for ev in raw_events {
        if ev.get("removed").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        if ev.get("mistake_type").and_then(|v| v.as_str()) == Some("peek") {
            let key = (
                ev.get("surah").and_then(|v| v.as_i64()).unwrap_or(0),
                ev.get("ayah").and_then(|v| v.as_i64()).unwrap_or(0),
                ev.get("word_index").and_then(|v| v.as_i64()).unwrap_or(0),
            );
            if !seen_peeks.contains(&key) {
                seen_peeks.push(key);
                peeks += 1;
            }
            continue;
        }
        match ev.get("status").and_then(|v| v.as_str()) {
            Some("correct") => counts[0] += 1,
            Some("close") => counts[1] += 1,
            Some("wrong") => counts[2] += 1,
            Some("skipped") => counts[3] += 1,
            _ => {}
        }
    }
    let total: i64 = counts.iter().sum();
    if total == 0 {
        return (None, counts, peeks);
    }
    let score = (counts[0] as f64 / total as f64 * 10_000.0).round() / 10_000.0;
    (Some(score), counts, peeks)
}

/// Total test score: mean over answered questions of
/// `max(0, score − 0.05 * peek_count)`, rounded to 4 dp.
/// Input: `(score, peek_count)` per question; `total` = question count.
pub fn test_total_score(questions: &[(Option<f64>, i64)]) -> Option<Value> {
    let mut scores: Vec<f64> = Vec::new();
    for (score, peeks) in questions {
        if let Some(s) = score {
            scores.push((s - 0.05 * *peeks as f64).max(0.0));
        }
    }
    if scores.is_empty() {
        return None;
    }
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    Some(json!({
        "score": (mean * 10_000.0).round() / 10_000.0,
        "answered": scores.len(),
        "total": questions.len(),
    }))
}

// --- plan ordering (hifz_plan.plan_ayah_order) -------------------------------

/// All (surah, ayah) pairs of a continuous range, in plan order.
/// `normal`: surahs ascending; `reverse`: surahs descending, ayahs
/// still ascending within each surah.
pub fn plan_ayah_order(
    start_surah: i32,
    start_ayah: i32,
    end_surah: i32,
    end_ayah: i32,
    direction: &str,
) -> Vec<(i32, i32)> {
    let mut order: Vec<(i32, i32)> = Vec::new();
    if start_surah == end_surah {
        for a in start_ayah..=end_ayah {
            order.push((start_surah, a));
        }
        return order;
    }
    let mut surahs: Vec<i32> = (start_surah..=end_surah).collect();
    if direction == "reverse" {
        surahs.reverse();
    }
    for s in surahs {
        let total = get_surah(s).map(|m| m.ayahs).unwrap_or(0);
        let first = if s == start_surah { start_ayah } else { 1 };
        let last = if s == end_surah { end_ayah } else { total };
        for a in first..=last {
            order.push((s, a));
        }
    }
    order
}

// --- tasmi state machines (hifz_plan.py) --------------------------------------

pub const METHOD_6446_SETS: &[(&str, i64)] =
    &[("read", 6), ("recite", 4), ("read", 4), ("recite", 6)];

pub const METHOD_3X3_STEPS: &[(&str, i64)] = &[
    ("read_all", 5),
    ("recite_1", 3),
    ("recite_2", 3),
    ("recite_3", 3),
    ("join_1_2", 3),
    ("join_2_3", 3),
    ("join_all", 3),
];

pub const TASMI8_STAGES: &[(&str, &str)] = &[
    ("meet", "Meet the Ayahs"),
    ("first_recall", "First Recall"),
    ("recite_sequence", "Recite the Sequence"),
    ("bridge_gaps", "Bridge the Gaps"),
    ("continue_from_here", "Continue from Here"),
    ("chain_challenge", "Chain Challenge"),
    ("recite_in_prayer", "Recite in Prayer"),
    ("share_reflection", "Share a Reflection"),
];

pub fn tasmi8_variant_stages(variant: &str) -> Option<Vec<&'static str>> {
    match variant {
        "standard" => Some(TASMI8_STAGES.iter().map(|(id, _)| *id).collect()),
        "gentle" => Some(vec![
            "meet",
            "meet",
            "first_recall",
            "first_recall",
            "recite_sequence",
            "recite_sequence",
            "bridge_gaps",
            "continue_from_here",
            "chain_challenge",
            "recite_in_prayer",
            "share_reflection",
        ]),
        "revision" => Some(vec![
            "first_recall",
            "recite_sequence",
            "bridge_gaps",
            "chain_challenge",
        ]),
        _ => None,
    }
}

/// `tasmi8_initial_state` — unknown variant raises `ValueError` in
/// Python (→ 500); mirrored here as `internal_error`.
pub fn tasmi8_initial_state(variant: &str) -> Result<Value, ApiError> {
    let Some(stages) = tasmi8_variant_stages(variant) else {
        return Err(ApiError::internal());
    };
    Ok(json!({
        "variant": variant,
        "stages": stages,
        "stage_pos": 0,
        "done": false,
    }))
}

pub fn method_6446_initial_state(ayahs: &[(i32, i32)]) -> Value {
    json!({
        "ayahs": ayahs.iter().map(|(s, a)| json!([s, a])).collect::<Vec<_>>(),
        "ayah_pos": 0,
        "set_pos": 0,
        "count_in_set": 0,
        "done": false,
    })
}

pub fn method_3x3_initial_state(ayahs: &[(i32, i32)]) -> Value {
    let groups: Vec<Value> = ayahs
        .chunks(3)
        .map(|g| json!(g.iter().map(|(s, a)| json!([s, a])).collect::<Vec<_>>()))
        .collect();
    json!({
        "groups": groups,
        "group_pos": 0,
        "step_pos": 0,
        "count_in_step": 0,
        "done": false,
    })
}

fn state_int(state: &Value, key: &str) -> i64 {
    state.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
}

fn state_bool(state: &Value, key: &str) -> bool {
    state.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

pub fn method_6446_tap(state: &Value) -> Value {
    let mut s = state.clone();
    if state_bool(&s, "done") {
        return s;
    }
    let ayahs_len = s
        .get("ayahs")
        .and_then(|v| v.as_array())
        .map(|a| a.len() as i64)
        .unwrap_or(0);
    let count = state_int(&s, "count_in_set") + 1;
    let mut set_pos = state_int(&s, "set_pos");
    let (_, target) = METHOD_6446_SETS[set_pos as usize % METHOD_6446_SETS.len()];
    if count >= target {
        let count = 0;
        set_pos += 1;
        if set_pos >= METHOD_6446_SETS.len() as i64 {
            set_pos = 0;
            let ayah_pos = state_int(&s, "ayah_pos") + 1;
            if ayah_pos >= ayahs_len {
                s["done"] = json!(true);
                s["ayah_pos"] = json!(ayahs_len);
                s["set_pos"] = json!(0);
                s["count_in_set"] = json!(0);
                return s;
            }
            s["ayah_pos"] = json!(ayah_pos);
        }
        s["set_pos"] = json!(set_pos);
        s["count_in_set"] = json!(count);
        return s;
    }
    s["set_pos"] = json!(set_pos);
    s["count_in_set"] = json!(count);
    s
}

pub fn method_3x3_tap(state: &Value) -> Value {
    let mut s = state.clone();
    if state_bool(&s, "done") {
        return s;
    }
    let groups_len = s
        .get("groups")
        .and_then(|v| v.as_array())
        .map(|a| a.len() as i64)
        .unwrap_or(0);
    let count = state_int(&s, "count_in_step") + 1;
    let mut step_pos = state_int(&s, "step_pos");
    let (_, target) = METHOD_3X3_STEPS[step_pos as usize % METHOD_3X3_STEPS.len()];
    if count >= target {
        let count = 0;
        step_pos += 1;
        if step_pos >= METHOD_3X3_STEPS.len() as i64 {
            step_pos = 0;
            let group_pos = state_int(&s, "group_pos") + 1;
            if group_pos >= groups_len {
                s["done"] = json!(true);
                s["group_pos"] = json!(groups_len);
                s["step_pos"] = json!(0);
                s["count_in_step"] = json!(0);
                return s;
            }
            s["group_pos"] = json!(group_pos);
        }
        s["step_pos"] = json!(step_pos);
        s["count_in_step"] = json!(count);
        return s;
    }
    s["step_pos"] = json!(step_pos);
    s["count_in_step"] = json!(count);
    s
}

pub fn tasmi8_advance(state: &Value) -> Value {
    let mut s = state.clone();
    if state_bool(&s, "done") {
        return s;
    }
    let stages_len = s
        .get("stages")
        .and_then(|v| v.as_array())
        .map(|a| a.len() as i64)
        .unwrap_or(0);
    let pos = state_int(&s, "stage_pos") + 1;
    if pos >= stages_len {
        s["done"] = json!(true);
        s["stage_pos"] = json!(stages_len);
        return s;
    }
    s["stage_pos"] = json!(pos);
    s
}

/// Review due time for a tasmi rating — unknown rating raises
/// `ValueError` in Python (→ 500); mirrored as `internal_error`.
pub fn tasmi_review_due_at(rating: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, ApiError> {
    let delta = match rating {
        "forgot" => Duration::minutes(10),
        "hard" => Duration::days(1),
        "good" => Duration::days(4),
        "easy" => Duration::days(9),
        _ => return Err(ApiError::internal()),
    };
    Ok(now + delta)
}

/// The `current` derivation for a tasmi session's state.
pub fn tasmi_current(method: &str, state: &Value) -> Value {
    match method {
        "method_6446" => {
            let ayahs = state
                .get("ayahs")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let pos = state_int(state, "ayah_pos");
            let set_pos = state_int(state, "set_pos") as usize % METHOD_6446_SETS.len();
            let (kind, target) = METHOD_6446_SETS[set_pos];
            json!({
                "ayah": ayahs.get(pos as usize).cloned().unwrap_or(Value::Null),
                "ayah_pos": pos,
                "ayah_total": ayahs.len(),
                "set_pos": state_int(state, "set_pos"),
                "set_kind": kind,
                "count_in_set": state_int(state, "count_in_set"),
                "set_target": target,
                "done": state_bool(state, "done"),
            })
        }
        "method_3x3" => {
            let groups = state
                .get("groups")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let pos = state_int(state, "group_pos");
            let step_pos = state_int(state, "step_pos") as usize % METHOD_3X3_STEPS.len();
            let (kind, target) = METHOD_3X3_STEPS[step_pos];
            json!({
                "group": groups.get(pos as usize).cloned().unwrap_or(Value::Null),
                "group_pos": pos,
                "group_total": groups.len(),
                "step_pos": state_int(state, "step_pos"),
                "step_kind": kind,
                "count_in_step": state_int(state, "count_in_step"),
                "step_target": target,
                "done": state_bool(state, "done"),
            })
        }
        _ => {
            // tasmi8
            let stages = state
                .get("stages")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let pos = state_int(state, "stage_pos");
            let stage_id = stages
                .get(pos as usize)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let title = stage_id.as_deref().and_then(|id| {
                TASMI8_STAGES
                    .iter()
                    .find(|(s, _)| *s == id)
                    .map(|(_, t)| *t)
            });
            json!({
                "stage": stage_id,
                "stage_title": title,
                "stage_pos": pos,
                "stage_total": stages.len(),
                "done": state_bool(state, "done"),
            })
        }
    }
}

// --- test passages (hifz_testing.build_passages / load_mushaf_lines) --------

#[derive(Debug, Clone)]
pub struct MushafLine {
    pub text: String,
    pub surah_start: i64,
    pub ayah_start: i64,
    pub surah_end: i64,
    pub ayah_end: i64,
}

static MUSHAF_LINES: OnceLock<Result<Vec<MushafLine>, ()>> = OnceLock::new();

/// Printed-mushaf lines (`apps/web/lib/quran-mushaf-lines.json`,
/// copied to `rust/app/data/quran/`), parsed once. Missing/corrupt/
/// empty → 503 `hifz_mushaf_lines_unavailable`, like Python.
pub fn mushaf_lines() -> Result<&'static [MushafLine], ApiError> {
    let loaded = MUSHAF_LINES.get_or_init(|| {
        let raw = include_str!("../data/quran/mushaf-lines.json");
        let parsed: Value = serde_json::from_str(raw).map_err(|_| ())?;
        let mut lines: Vec<MushafLine> = Vec::new();
        if let Some(pages) = parsed.get("pages").and_then(|v| v.as_array()) {
            for page in pages {
                let Some(arr) = page.as_array() else { continue };
                for line in arr {
                    if line.get("t").and_then(|v| v.as_str()) != Some("text") {
                        continue;
                    }
                    let parse_ref = |key: &str| -> Option<(i64, i64)> {
                        let s = line.get(key)?.as_str()?;
                        let (a, b) = s.split_once(':')?;
                        Some((a.parse().ok()?, b.parse().ok()?))
                    };
                    let (Some((v_s, v_a)), Some((w_s, w_a))) = (parse_ref("v"), parse_ref("w"))
                    else {
                        continue;
                    };
                    let text = line
                        .get("x")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if text.is_empty() {
                        continue;
                    }
                    lines.push(MushafLine {
                        text,
                        surah_start: v_s,
                        ayah_start: v_a,
                        surah_end: w_s,
                        ayah_end: w_a,
                    });
                }
            }
        }
        if lines.is_empty() {
            return Err(());
        }
        Ok(lines)
    });
    loaded.as_ref().map(|v| v.as_slice()).map_err(|_| {
        ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "hifz_mushaf_lines_unavailable",
            "دادهٔ سطرهای چاپی مصحف در دسترس نیست؛ ساخت آزمون ممکن نیست.",
        )
    })
}

fn is_digit_char(c: char) -> bool {
    c.is_ascii_digit()
        || ('\u{0660}'..='\u{0669}').contains(&c)
        || ('\u{06F0}'..='\u{06F9}').contains(&c)
}

/// First 3 non-numeral words of a line (ayah numbers are not words).
pub fn prompt_words(text: &str, n: usize) -> String {
    text.split_whitespace()
        .filter(|w| !w.is_empty() && !w.chars().all(is_digit_char))
        .take(n)
        .collect::<Vec<_>>()
        .join(" ")
}

/// SplitMix64 — deterministic PRNG for passage sampling (see module
/// docs: not CPython's Mersenne Twister).
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        // Rejection sampling, unbiased like Python's _randbelow.
        let n = n as u64;
        let mask = n.next_power_of_two() - 1;
        loop {
            let v = self.next() & mask;
            if v < n {
                return v as usize;
            }
        }
    }
}

/// Build test passages from the printed mushaf lines. Each passage:
/// `{surah, ayah_start, ayah_end, prompt}`; never crosses a surah
/// boundary; `ayah_end` extends past N lines until the last ayah
/// ends, `line_end` stops exactly at line N.
pub fn build_passages(
    surah_start: i64,
    surah_end: i64,
    count: i64,
    length: &str,
    stop_rule: &str,
    seed: u64,
) -> Result<Vec<Value>, ApiError> {
    let n_lines: usize = match length {
        "short" => 2,
        "medium" => 5,
        "long" => 12,
        _ => {
            return Err(ApiError::new(
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "hifz_test_length",
                "طول سؤال آزمون نامعتبر است",
            ))
        }
    };
    let all_lines = mushaf_lines()?;
    let starts: Vec<usize> = all_lines
        .iter()
        .enumerate()
        .filter(|(_, ln)| (surah_start..=surah_end).contains(&ln.surah_start))
        .map(|(i, _)| i)
        .collect();
    if starts.is_empty() {
        return Err(ApiError::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_test_range_empty",
            "در این بازه هیچ سطر مصحفی برای آزمون نیست.",
        ));
    }
    // Fisher–Yates partial shuffle with the seeded PRNG.
    let mut rng = SplitMix64(seed);
    let mut picked = starts.clone();
    let take = (count as usize).min(picked.len());
    for i in 0..take {
        let j = i + rng.below(picked.len() - i);
        picked.swap(i, j);
    }
    picked.truncate(take);
    picked.sort_unstable();
    let mut passages: Vec<Value> = Vec::new();
    for start_idx in picked {
        let first = &all_lines[start_idx];
        let surah = first.surah_start;
        let mut end_idx = start_idx;
        let mut taken = 1usize;
        while taken < n_lines && end_idx + 1 < all_lines.len() {
            let nxt = &all_lines[end_idx + 1];
            if nxt.surah_start != surah {
                break;
            }
            end_idx += 1;
            taken += 1;
        }
        if stop_rule == "ayah_end" {
            let mut last = &all_lines[end_idx];
            while end_idx + 1 < all_lines.len() {
                let nxt = &all_lines[end_idx + 1];
                if nxt.surah_start != surah {
                    break;
                }
                if (nxt.surah_start, nxt.ayah_start) != (last.surah_end, last.ayah_end) {
                    break;
                }
                end_idx += 1;
                last = &all_lines[end_idx];
            }
        }
        let last = &all_lines[end_idx];
        passages.push(json!({
            "surah": surah,
            "ayah_start": first.ayah_start,
            "ayah_end": last.ayah_end,
            "prompt": prompt_words(&first.text, 3),
        }));
    }
    Ok(passages)
}
