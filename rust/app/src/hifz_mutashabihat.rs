//! Mutashabihat (shared phrases) — mirrors
//! `hifz_mutashabihat.py`: all word n-grams of length 4–8 with ≥ 2
//! occurrences in the whole Quran, counted once per ayah per phrase,
//! computed once per process from the bundled Tanzil text (CC BY 3.0)
//! and cached. Groups sorted by occurrence count desc, then phrase
//! asc. No phrase list is hard-coded.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::hifz_logic::{normalize_arabic, tanzil_ayahs};

pub const MIN_PHRASE_WORDS: usize = 4;
pub const MAX_PHRASE_WORDS: usize = 8;
pub const MIN_OCCURRENCES: usize = 2;

#[derive(Debug, Clone)]
pub struct MutashabihatGroup {
    pub phrase: String,
    /// (surah, ayah, original Tanzil text of the whole ayah)
    pub occurrences: Vec<(i32, i32, String)>,
}

static COMPUTED: OnceLock<Vec<MutashabihatGroup>> = OnceLock::new();

pub fn compute_mutashabihat() -> &'static [MutashabihatGroup] {
    COMPUTED.get_or_init(|| {
        let mut index: HashMap<String, Vec<(i32, i32, String)>> = HashMap::new();
        for (surah, ayah, words, text) in tanzil_ayahs() {
            let mut seen_in_ayah: HashSet<String> = HashSet::new();
            for n in MIN_PHRASE_WORDS..=MAX_PHRASE_WORDS {
                if words.len() < n {
                    continue;
                }
                for start in 0..=(words.len() - n) {
                    let phrase = words[start..start + n].join(" ");
                    if !seen_in_ayah.insert(phrase.clone()) {
                        continue;
                    }
                    index
                        .entry(phrase)
                        .or_default()
                        .push((*surah, *ayah, text.clone()));
                }
            }
        }
        let mut groups: Vec<MutashabihatGroup> = index
            .into_iter()
            .filter(|(_, occ)| occ.len() >= MIN_OCCURRENCES)
            .map(|(phrase, occurrences)| MutashabihatGroup {
                phrase,
                occurrences,
            })
            .collect();
        groups.sort_by(|a, b| {
            b.occurrences
                .len()
                .cmp(&a.occurrences.len())
                .then_with(|| a.phrase.cmp(&b.phrase))
        });
        groups
    })
}

pub fn group_to_json(g: &MutashabihatGroup) -> Value {
    json!({
        "phrase": g.phrase,
        "occurrences": g.occurrences.iter().map(|(s, a, text)| {
            json!({ "surah": s, "ayah": a, "text": text })
        }).collect::<Vec<_>>(),
    })
}

/// Groups containing the given ayah, in the global group order.
pub fn groups_for_ayah(surah: i32, ayah: i32) -> Vec<Value> {
    compute_mutashabihat()
        .iter()
        .filter(|g| {
            g.occurrences
                .iter()
                .any(|(s, a, _)| *s == surah && *a == ayah)
        })
        .map(group_to_json)
        .collect()
}

/// One group by exact normalized phrase, or `None` (→ JSON `null`).
pub fn group_for_phrase(phrase: &str) -> Option<Value> {
    let needle = normalize_arabic(phrase);
    let needle = needle.split_whitespace().collect::<Vec<_>>().join(" ");
    compute_mutashabihat()
        .iter()
        .find(|g| g.phrase == needle)
        .map(group_to_json)
}

pub fn phrase_count() -> usize {
    compute_mutashabihat().len()
}
