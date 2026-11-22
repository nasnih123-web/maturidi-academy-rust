//! Calculator engines — a faithful port of the Python calculators
//! engines (`app/modules/calculators/engine.py`, `zakat_engine.py`,
//! `miras_engine.py`). All rule numbers and formulas are read from the
//! caller-supplied `rule_data` (the approved `rule_config`); nothing
//! fiqh is hardcoded here (ADR-009). The source rule tables
//! (`source_rules.py`) are data the admin seeds as `rule_config`s, not
//! code in this module.
//!
//! Money/weight arithmetic is exact (`calc_decimal::Dec` / `Frac`):
//! no `f64` anywhere, so the Rust/Python `round()` difference
//! (ADR-024/ADR-040) cannot surface here — rounding is the explicit
//! Python-equivalent `quantize` / 28-digit division in `calc_decimal`.

use serde_json::{json, Map, Value};

use super::calc_decimal::{dec_from_json, CalcError, Dec, Frac};

// ---------------------------------------------------------------------------
// rule_data access helpers (mirror engine.py `_frac` / `_dec`)
// ---------------------------------------------------------------------------

pub fn frac_from_rule(v: Option<&Value>, field: &str) -> Result<Frac, CalcError> {
    let Some(s) = v.and_then(|v| v.as_str()) else {
        return Err(CalcError::new(
            "bad_rule_fraction",
            format!(
                "کسر نامعتبر در rule_data (فیلد {field}): {}",
                v.map(|x| x.to_string())
                    .unwrap_or_else(|| "None".to_string())
            ),
        ));
    };
    Frac::parse(s).map_err(|_| {
        CalcError::new(
            "bad_rule_fraction",
            format!("کسر نامعتبر در rule_data (فیلد {field}): '{s}'"),
        )
    })
}

pub fn dec_from_rule(v: Option<&Value>, field: &str) -> Result<Dec, CalcError> {
    let Some(d) = v.and_then(dec_from_json) else {
        return Err(CalcError::new(
            "bad_rule_number",
            format!(
                "عدد نامعتبر در rule_data (فیلد {field}): {}",
                v.map(|x| x.to_string())
                    .unwrap_or_else(|| "None".to_string())
            ),
        ));
    };
    Ok(d)
}

fn money(v: Dec) -> Result<String, CalcError> {
    v.fmt_fixed(4)
}

// ---------------------------------------------------------------------------
// engine.py — compute_inheritance
// ---------------------------------------------------------------------------

/// `engine.compute_inheritance`: generic fixed-share + asaba + awl /
/// radd execution over `rule_data`.
pub fn compute_inheritance(
    heirs: &[(String, i64)],
    estate_value: Dec,
    rule_data: &Value,
    rule_version: i64,
) -> Result<Value, CalcError> {
    if estate_value <= Dec::zero() {
        return Err(CalcError::new(
            "bad_estate_value",
            "ارزش ترکه باید مثبت باشد",
        ));
    }
    if !rule_data.is_object() || rule_data.as_object().is_none_or(|o| o.is_empty()) {
        return Err(CalcError::new(
            "rule_data_empty",
            "rule_data خالی است — قاعده هنوز توسط عالم وارد نشده است",
        ));
    }
    let shares_cfg = rule_data.get("shares").and_then(|v| v.as_object());
    let shares_cfg: Map<String, Value> = shares_cfg.cloned().unwrap_or_default();
    let awl_enabled = rule_data
        .get("awl")
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let radd_cfg = rule_data.get("radd").and_then(|v| v.as_object());
    let radd_enabled = radd_cfg
        .and_then(|o| o.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let radd_excluded: Vec<String> = radd_cfg
        .and_then(|o| o.get("excluded"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let asaba_priority: Vec<String> = rule_data
        .get("asaba_priority")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // --- validate heirs ---
    let mut normalized: Vec<(String, i64)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (raw_type, count) in heirs {
        let htype = raw_type.trim().to_string();
        if htype.is_empty() || *count < 1 {
            return Err(CalcError::new(
                "bad_heir_entry",
                format!("ورودی وارث نامعتبر: {raw_type}/{count}"),
            ));
        }
        if !shares_cfg.contains_key(&htype) && !asaba_priority.contains(&htype) {
            return Err(CalcError::new(
                "unknown_heir",
                format!("نوع وارث «{htype}» در rule_data تعریف نشده است"),
            ));
        }
        if seen.contains(&htype) {
            return Err(CalcError::new(
                "duplicate_heir",
                format!("نوع وارث «{htype}» تکراری است"),
            ));
        }
        seen.push(htype.clone());
        normalized.push((htype, *count));
    }

    // --- fixed shares ---
    let mut group: Vec<(String, Frac)> = Vec::new();
    let mut total_fixed = Frac::zero();
    for (htype, _) in &normalized {
        let Some(cfg) = shares_cfg.get(htype) else {
            continue;
        };
        let frac = frac_from_rule(cfg.get("share"), &format!("shares.{htype}.share"))?;
        if frac < Frac::zero() || frac > Frac::one() {
            return Err(CalcError::new(
                "bad_rule_fraction",
                format!("سهم «{htype}» باید بین ۰ و ۱ باشد"),
            ));
        }
        group.push((htype.clone(), frac));
        total_fixed = total_fixed.add(&frac)?;
    }

    let mut notes: Vec<String> = Vec::new();
    let asaba_types: Vec<String> = asaba_priority
        .iter()
        .filter(|t| seen.contains(t))
        .cloned()
        .collect();
    let remainder = Frac::one().sub(&total_fixed)?;
    let set_group = |group: &mut Vec<(String, Frac)>, key: &str, f: Frac| {
        if let Some(slot) = group.iter_mut().find(|(k, _)| k == key) {
            slot.1 = f;
        } else {
            group.push((key.to_string(), f));
        }
    };

    if remainder < Frac::zero() {
        if !awl_enabled {
            return Err(CalcError::new(
                "shares_exceed_estate",
                "جمع سهم‌های ثابت از کل ترکه بیشتر است و عول در rule_data فعال نشده است",
            ));
        }
        let scale = Frac::one().div(&total_fixed)?;
        for (_, f) in group.iter_mut() {
            *f = f.mul(&scale)?;
        }
        notes.push("awl_applied".to_string());
    } else if remainder > Frac::zero() {
        if let Some(winner) = asaba_types.first() {
            let cur = group
                .iter()
                .find(|(k, _)| k == winner)
                .map(|(_, f)| *f)
                .unwrap_or_else(Frac::zero);
            set_group(&mut group, winner, cur.add(&remainder)?);
            notes.push(format!("asaba_applied:{winner}"));
        } else if radd_enabled {
            let eligible: Vec<String> = group
                .iter()
                .map(|(k, _)| k.clone())
                .filter(|k| !radd_excluded.contains(k))
                .collect();
            if !eligible.is_empty() {
                let mut eligible_total = Frac::zero();
                for k in &eligible {
                    eligible_total = eligible_total.add(
                        &group
                            .iter()
                            .find(|(kk, _)| kk == k)
                            .map(|(_, f)| *f)
                            .unwrap(),
                    )?;
                }
                if eligible_total > Frac::zero() {
                    let scale = (eligible_total.add(&remainder)?).div(&eligible_total)?;
                    for (k, f) in group.iter_mut() {
                        if eligible.contains(k) {
                            *f = f.mul(&scale)?;
                        }
                    }
                    notes.push("radd_applied".to_string());
                } else {
                    notes.push("unallocated_remainder".to_string());
                }
            } else {
                notes.push("unallocated_remainder".to_string());
            }
        } else {
            notes.push("unallocated_remainder".to_string());
        }
    }

    let mut out_shares: Vec<Value> = Vec::new();
    let mut total_allocated = Frac::zero();
    for (htype, count) in &normalized {
        let frac = group
            .iter()
            .find(|(k, _)| k == htype)
            .map(|(_, f)| *f)
            .unwrap_or_else(Frac::zero);
        total_allocated = total_allocated.add(&frac)?;
        let amount = estate_frac_amount(estate_value, frac)?;
        let per_person = amount.div28(&Dec::from_int(*count))?;
        out_shares.push(json!({
            "heir_type": htype,
            "count": count,
            "share_fraction": frac.to_py_string(),
            "amount": money(amount)?,
            "per_person_amount": money(per_person)?,
        }));
    }

    Ok(json!({
        "shares": out_shares,
        "total_allocated_fraction": total_allocated.to_py_string(),
        "notes": notes,
        "applied_rule_version": rule_version,
    }))
}

/// `estate * num / den` with Python's Decimal division semantics.
fn estate_frac_amount(estate: Dec, frac: Frac) -> Result<Dec, CalcError> {
    // The fraction's numerator/denominator are i128; build the exact
    // Decimals directly (rule fractions are small in practice).
    let n = Dec::raw(frac.n, 0);
    let d = Dec::raw(frac.d, 0);
    estate.mul(&n)?.div28(&d)
}

// ---------------------------------------------------------------------------
// engine.py — compute_zakat
// ---------------------------------------------------------------------------

pub struct ZakatRates {
    pub gold_per_gram: Option<Dec>,
    pub silver_per_gram: Option<Dec>,
    pub currency: Option<String>,
    pub stale: bool,
}

pub fn compute_zakat(
    assets: &[(String, Dec)],
    debts: Dec,
    rule_data: &Value,
    rates: &ZakatRates,
    rule_version: i64,
) -> Result<Value, CalcError> {
    if !rule_data.is_object() || rule_data.as_object().is_none_or(|o| o.is_empty()) {
        return Err(CalcError::new(
            "rule_data_empty",
            "rule_data خالی است — قاعده هنوز توسط عالم وارد نشده است",
        ));
    }
    if debts < Dec::zero() {
        return Err(CalcError::new("bad_debts", "بدهی نمی‌تواند منفی باشد"));
    }
    let categories = rule_data
        .get("asset_categories")
        .and_then(|v| v.as_object());
    let Some(categories) = categories else {
        return Err(CalcError::new(
            "rule_data_empty",
            "asset_categories در rule_data تعریف نشده است",
        ));
    };
    if categories.is_empty() {
        return Err(CalcError::new(
            "rule_data_empty",
            "asset_categories در rule_data تعریف نشده است",
        ));
    }

    let mut total = Dec::zero();
    for (key, raw) in assets {
        let Some(cat) = categories.get(key) else {
            return Err(CalcError::new(
                "unknown_asset_category",
                format!("دستهٔ دارایی «{key}» در rule_data تعریف نشده است"),
            ));
        };
        if *raw < Dec::zero() {
            return Err(CalcError::new(
                "bad_asset_value",
                format!("مقدار «{key}» منفی است"),
            ));
        }
        if cat.get("type").and_then(|v| v.as_str()) == Some("grams") {
            let price_key = cat.get("price").and_then(|v| v.as_str()).unwrap_or("");
            let price = match price_key {
                "gold_per_gram" => rates.gold_per_gram,
                "silver_per_gram" => rates.silver_per_gram,
                _ => None,
            };
            let Some(price) = price else {
                return Ok(json!({
                    "status": "price_unavailable",
                    "notes": [format!("missing_price:{price_key}")],
                    "applied_rule_version": rule_version,
                }));
            };
            total = total.add(&raw.mul(&price)?)?;
        } else {
            total = total.add(raw)?;
        }
    }

    let mut notes: Vec<String> = Vec::new();
    let mut net = total.sub(&debts)?;
    if net < Dec::zero() {
        net = Dec::zero();
        notes.push("debts_exceed_assets".to_string());
    }

    let basis = rule_data
        .get("nisab_basis")
        .and_then(|v| v.as_str())
        .unwrap_or("gold");
    let grams_key = if basis == "gold" {
        "nisab_gold_grams"
    } else {
        "nisab_silver_grams"
    };
    let price_key = if basis == "gold" {
        "gold_per_gram"
    } else {
        "silver_per_gram"
    };
    let nisab_grams = dec_from_rule(rule_data.get(grams_key), grams_key)?;
    if nisab_grams <= Dec::zero() {
        return Err(CalcError::new(
            "bad_rule_number",
            format!("نصاب باید مثبت باشد ({grams_key})"),
        ));
    }
    let price = if basis == "gold" {
        rates.gold_per_gram
    } else {
        rates.silver_per_gram
    };
    let Some(price) = price else {
        return Ok(json!({
            "status": "price_unavailable",
            "notes": [format!("missing_price:{price_key}")],
            "applied_rule_version": rule_version,
        }));
    };
    let nisab_value = nisab_grams.mul(&price)?;
    let rate = dec_from_rule(rule_data.get("zakat_rate"), "zakat_rate")?;
    if rate < Dec::zero() || rate > Dec::from_int(1) {
        return Err(CalcError::new(
            "bad_rule_number",
            "نرخ زکات باید بین ۰ و ۱ باشد",
        ));
    }
    let nisab_met = net >= nisab_value;
    let zakat_due = if nisab_met {
        net.mul(&rate)?
    } else {
        Dec::zero()
    };
    if rates.stale {
        notes.push("price_stale".to_string());
    }

    Ok(json!({
        "status": "ok",
        "total_assets": money(total)?,
        "debts": money(debts)?,
        "net_assets": money(net)?,
        "nisab_basis": basis,
        "nisab_grams": nisab_grams.to_py_string(),
        "nisab_value": money(nisab_value)?,
        "nisab_met": nisab_met,
        "zakat_rate": rate.to_py_string(),
        "zakat_due": money(zakat_due)?,
        "currency": rates.currency,
        "price_stale": rates.stale,
        "notes": notes,
        "applied_rule_version": rule_version,
    }))
}

// ---------------------------------------------------------------------------
// zakat_engine.py — compute_zakat_extended
// ---------------------------------------------------------------------------

/// The validated `zakat/extended` sections, built by the route from
/// the request body (mirrors the router's `sections` dict).
#[derive(Debug, Default, Clone)]
pub struct ExtendedSections {
    pub money: Option<(Dec, Dec)>, // (amount, coin_price)
    pub gold_grams: Option<Dec>,
    pub silver_grams: Option<Dec>,
    pub sheep: Option<i64>,
    pub cows: Option<(i64, Option<Dec>)>, // (count, musinna_price?)
    pub camels: Option<i64>,
    pub horses: Option<(i64, Option<Dec>)>, // (count, total_value?)
    pub oshr: Option<(Dec, String)>,        // (amount_kg, irrigation)
    pub khoms: Option<Dec>,
}

fn rule_dec(cfg: &Value, key: &str) -> Result<Dec, CalcError> {
    dec_from_rule(cfg.get(key), key)
}

fn staircase(
    amount: Dec,
    nisab: Dec,
    at_nisab: Dec,
    step: Dec,
    step_zakat: Dec,
) -> Result<(bool, Dec), CalcError> {
    if amount < nisab {
        return Ok((false, Dec::zero()));
    }
    let steps = amount.sub(&nisab)?.floor_div(&step)?;
    let extra = steps.mul(&step_zakat)?;
    Ok((true, at_nisab.add(&extra)?))
}

fn money_section(cfg: &Value, amount: Dec, coin_price: Dec) -> Result<Value, CalcError> {
    let nisab = rule_dec(cfg, "nisab_coins")?;
    let total_coins = amount.div28(&coin_price)?;
    let (met, coins) = staircase(
        total_coins,
        nisab,
        rule_dec(cfg, "zakat_at_nisab_coins")?,
        rule_dec(cfg, "step_coins")?,
        rule_dec(cfg, "step_zakat_coins")?,
    )?;
    let due_amount = coins.mul(&coin_price)?.quantize(0)?;
    Ok(json!({
        "nisab_met": met,
        "total_coins": total_coins.to_py_string(),
        "zakat_coins": coins.to_py_string(),
        "zakat_amount": due_amount.to_py_string(),
    }))
}

fn metal_section(cfg: &Value, grams: Dec) -> Result<Value, CalcError> {
    let (met, zakat) = staircase(
        grams,
        rule_dec(cfg, "nisab_grams")?,
        rule_dec(cfg, "zakat_at_nisab_grams")?,
        rule_dec(cfg, "step_grams")?,
        rule_dec(cfg, "step_zakat_grams")?,
    )?;
    Ok(json!({
        "nisab_met": met,
        "zakat_grams": zakat.fmt_fixed(2)?,
    }))
}

fn round_input(grams: Dec, round_to: &str) -> Result<Dec, CalcError> {
    let unit = Dec::parse(round_to)
        .ok_or_else(|| CalcError::new("bad_rule_number", format!("عدد نامعتبر: '{round_to}'")))?;
    grams.div28(&unit)?.quantize(0)?.mul(&unit)
}

fn sheep_due(cfg: &Value, count: i64) -> Result<Vec<Value>, CalcError> {
    let nisab = cfg
        .get("nisab")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| CalcError::internal("sheep nisab در rule_data نیست"))?;
    if count < nisab {
        return Ok(vec![]);
    }
    let per_hundred = cfg
        .get("per_hundred")
        .ok_or_else(|| CalcError::internal("sheep per_hundred در rule_data نیست"))?;
    let from = per_hundred
        .get("from")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| CalcError::internal("sheep per_hundred.from نیست"))?;
    let divisor = per_hundred
        .get("divisor")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| CalcError::internal("sheep per_hundred.divisor نیست"))?;
    if count >= from {
        return Ok(vec![json!({"kind": "sheep", "count": count / divisor})]);
    }
    let table = cfg
        .get("table")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CalcError::internal("sheep table در rule_data نیست"))?;
    for entry in table {
        let (Some(upper), Some(due)) = (
            entry.get(0).and_then(|v| v.as_i64()),
            entry.get(1).and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        if count <= upper {
            return Ok(vec![json!({"kind": "sheep", "count": due})]);
        }
    }
    Ok(vec![])
}

fn merge_due(items: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for item in items {
        let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let age = item.get("age_years").and_then(|v| v.as_i64());
        let mut found = false;
        for ex in merged.iter_mut() {
            let ex_kind = ex.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let ex_age = ex.get("age_years").and_then(|v| v.as_i64());
            if ex_kind == kind && ex_age == age {
                let cur = ex.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
                let add = item.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
                if let Some(map) = ex.as_object_mut() {
                    map.insert("count".to_string(), json!(cur + add));
                }
                found = true;
                break;
            }
        }
        if !found {
            merged.push(item);
        }
    }
    merged
}

fn table_due(entries: &Value) -> Vec<Value> {
    let mut due = Vec::new();
    let Some(arr) = entries.as_array() else {
        return due;
    };
    for e in arr {
        let kind = e.get(0).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let age = e.get(1).and_then(|v| v.as_i64());
        let count = e.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
        let mut item = Map::new();
        item.insert("kind".to_string(), json!(kind));
        item.insert("count".to_string(), json!(count));
        if let Some(a) = age {
            item.insert("age_years".to_string(), json!(a));
        }
        due.push(Value::Object(item));
    }
    due
}

fn camel_due(cfg: &Value, count: i64) -> Result<Vec<Value>, CalcError> {
    let over = cfg
        .get("over")
        .ok_or_else(|| CalcError::internal("camel over در rule_data نیست"))?;
    let threshold = over
        .get("threshold")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| CalcError::internal("camel over.threshold نیست"))?;
    if count > threshold {
        let base_per = over
            .get("base_per")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| CalcError::internal("camel over.base_per نیست"))?;
        let base_due = over
            .get("base_due")
            .ok_or_else(|| CalcError::internal("camel over.base_due نیست"))?;
        let kind = base_due.get(0).and_then(|v| v.as_str()).unwrap_or("");
        let age = base_due.get(1).and_then(|v| v.as_i64());
        let base_count = count / base_per;
        let mut due = vec![{
            let mut m = Map::new();
            m.insert("kind".to_string(), json!(kind));
            m.insert("count".to_string(), json!(base_count));
            if let Some(a) = age {
                m.insert("age_years".to_string(), json!(a));
            }
            Value::Object(m)
        }];
        let remainder = count - base_count * base_per;
        let rem_table = over
            .get("remainder_table")
            .and_then(|v| v.as_array())
            .ok_or_else(|| CalcError::internal("camel remainder_table نیست"))?;
        for entry in rem_table {
            let Some(upper) = entry.get(0).and_then(|v| v.as_i64()) else {
                continue;
            };
            if remainder <= upper {
                if let Some(entries) = entry.get(1) {
                    due.extend(table_due(entries));
                }
                break;
            }
        }
        return Ok(merge_due(due));
    }
    let table = cfg
        .get("table")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CalcError::internal("camel table در rule_data نیست"))?;
    for entry in table {
        let Some(upper) = entry.get(0).and_then(|v| v.as_i64()) else {
            continue;
        };
        if count <= upper {
            if let Some(entries) = entry.get(1) {
                return Ok(table_due(entries));
            }
        }
    }
    Ok(vec![])
}

fn cow_result(cfg: &Value, count: i64, musinna_price: Option<Dec>) -> Result<Value, CalcError> {
    let need = |key: &str| -> Result<i64, CalcError> {
        cfg.get(key)
            .and_then(|v| v.as_i64())
            .ok_or_else(|| CalcError::internal(format!("cow {key} در rule_data نیست")))
    };
    let tabii_from = need("tabii_from")?;
    let musinna_from = need("musinna_from")?;
    let double_from = need("double_tabii_from")?;
    let combo_from = need("combo_from")?;
    let mut due: Vec<Value> = Vec::new();
    let mut fortieths: i64 = 0;
    if count < tabii_from {
        // nothing due
    } else if count < musinna_from {
        due = vec![json!({"kind": "cow_tabii", "count": 1})];
    } else if count < double_from {
        due = vec![json!({"kind": "cow_musinna", "count": 1})];
        fortieths = count - musinna_from;
    } else if count < combo_from {
        due = vec![json!({"kind": "cow_tabii", "count": 2})];
    } else {
        let units = cfg
            .get("combo_units")
            .and_then(|v| v.as_array())
            .ok_or_else(|| CalcError::internal("cow combo_units نیست"))?;
        let unit_small = units.first().and_then(|v| v.as_i64()).unwrap_or(30);
        let unit_big = units.get(1).and_then(|v| v.as_i64()).unwrap_or(40);
        let round_to = need("combo_round_to")?;
        let rounded = (count / round_to) * round_to;
        let mut best: Option<(i64, i64, i64)> = None; // (diff, a, b)
        let mut b = 0i64;
        while b * unit_big <= rounded {
            let rest = rounded - b * unit_big;
            if rest % unit_small == 0 {
                let a = rest / unit_small;
                let diff = (a - b).abs();
                if best.is_none() || diff < best.unwrap().0 {
                    best = Some((diff, a, b));
                }
            }
            b += 1;
        }
        if let Some((_, a, b)) = best {
            if a > 0 {
                due.push(json!({"kind": "cow_tabii", "count": a}));
            }
            if b > 0 {
                due.push(json!({"kind": "cow_musinna", "count": b}));
            }
        }
    }
    let mut out = Map::new();
    out.insert("due".to_string(), json!(due));
    out.insert("musinna_fortieths".to_string(), json!(fortieths));
    if fortieths > 0 {
        if let Some(price) = musinna_price {
            let denom = need("musinna_share_denominator")?;
            let extra = Dec::from_int(fortieths)
                .mul(&price)?
                .div28(&Dec::from_int(denom))?;
            out.insert(
                "extra_money".to_string(),
                json!(extra.quantize(0)?.to_py_string()),
            );
        }
    }
    Ok(Value::Object(out))
}

fn horse_result(cfg: &Value, count: i64, total_value: Option<Dec>) -> Result<Value, CalcError> {
    let rate = cfg
        .get("value_rate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CalcError::internal("horse value_rate نیست"))?;
    let (num, den) = rate
        .split_once('/')
        .and_then(|(a, b)| Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?)))
        .ok_or_else(|| CalcError::internal("horse value_rate نامعتبر است"))?;
    let per_head = cfg
        .get("per_head_dinar")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| CalcError::internal("horse per_head_dinar نیست"))?;
    let mut out = Map::new();
    out.insert(
        "rule_note".to_string(),
        json!("dinar_or_fortieth_per_horse"),
    );
    out.insert("dinar_per_head".to_string(), json!(per_head));
    out.insert("total_dinar".to_string(), json!(per_head * count));
    if let Some(value) = total_value {
        let due = value.mul(&Dec::from_int(num))?.div28(&Dec::from_int(den))?;
        out.insert(
            "value_zakat".to_string(),
            json!(due.quantize(0)?.to_py_string()),
        );
    }
    Ok(Value::Object(out))
}

/// `zakat_engine.compute_zakat_extended` — every requested section is
/// computed from the approved `rule_data` (`source_extended_v1`).
pub fn compute_zakat_extended(
    sections: &ExtendedSections,
    rule_data: &Value,
) -> Result<Value, CalcError> {
    if rule_data.get("format").and_then(|v| v.as_str()) != Some("source_extended_v1") {
        return Err(CalcError::new(
            "unsupported_rule_format",
            "قالب rule_data زکات پشتیبانی نمی‌شود",
        ));
    }
    let need = |key: &str| -> Result<&Value, CalcError> {
        rule_data
            .get(key)
            .ok_or_else(|| CalcError::internal(format!("بخش {key} در rule_data نیست")))
    };
    let mut out = Map::new();
    if let Some((amount, coin_price)) = sections.money {
        out.insert(
            "money".to_string(),
            money_section(need("money")?, amount, coin_price)?,
        );
    }
    if let Some(grams) = sections.gold_grams {
        out.insert("gold".to_string(), metal_section(need("gold")?, grams)?);
    }
    if let Some(grams) = sections.silver_grams {
        let cfg = need("silver")?;
        let grams = match cfg.get("input_round_grams").and_then(|v| v.as_str()) {
            Some(unit) => round_input(grams, unit)?,
            None => grams,
        };
        out.insert("silver".to_string(), metal_section(cfg, grams)?);
    }
    if let Some(count) = sections.sheep {
        out.insert(
            "sheep".to_string(),
            json!({"due": sheep_due(need("sheep")?, count)?}),
        );
    }
    if let Some((count, price)) = sections.cows {
        out.insert("cows".to_string(), cow_result(need("cow")?, count, price)?);
    }
    if let Some(count) = sections.camels {
        out.insert(
            "camels".to_string(),
            json!({"due": camel_due(need("camel")?, count)?}),
        );
    }
    if let Some((count, value)) = sections.horses {
        out.insert(
            "horses".to_string(),
            horse_result(need("horse")?, count, value)?,
        );
    }
    if let Some((amount_kg, irrigation)) = &sections.oshr {
        let rates = need("oshr")?
            .get("rates")
            .ok_or_else(|| CalcError::internal("oshr rates نیست"))?;
        let Some(rate) = rates.get(irrigation).and_then(|v| v.as_str()) else {
            return Err(CalcError::new(
                "unknown_irrigation",
                "نوع آبیاری نامعتبر است",
            ));
        };
        let (num, den) = rate
            .split_once('/')
            .and_then(|(a, b)| Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?)))
            .ok_or_else(|| CalcError::internal("oshr rate نامعتبر است"))?;
        let kg = amount_kg
            .mul(&Dec::from_int(num))?
            .div28(&Dec::from_int(den))?;
        out.insert("oshr".to_string(), json!({"oshr_kg": kg.plain()}));
    }
    if let Some(amount) = sections.khoms {
        let rate = rule_dec(need("khoms")?, "rate")?;
        out.insert(
            "khoms".to_string(),
            json!({"khoms": amount.mul(&rate)?.plain()}),
        );
    }
    if out.is_empty() {
        return Err(CalcError::new(
            "no_sections",
            "هیچ بخشی برای محاسبه ارسال نشده است",
        ));
    }
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------
// miras_engine.py — compute_miras (Hanafi fara'id, as-Sirajiyya)
// ---------------------------------------------------------------------------

/// `MirasStopped`: the source refuses to issue a result (impediment,
/// no distributable heir, incomplete total, multi-branch dhawu) — the
/// service answers 200 with `status: "stopped"`, never an error.
#[derive(Debug, Clone)]
pub enum MirasFail {
    Stopped(&'static str),
    Err(CalcError),
}

impl From<CalcError> for MirasFail {
    fn from(e: CalcError) -> Self {
        MirasFail::Err(e)
    }
}

/// The `MirasIn` family, after schema validation.
#[derive(Debug, Clone, Default)]
pub struct MirasFamily {
    pub gender: String,
    pub husband: bool,
    pub father: bool,
    pub mother: bool,
    pub grandfather: bool,
    pub mgm: bool,
    pub pgm: bool,
    pub wives: i64,
    pub sons: i64,
    pub daughters: i64,
    pub son_sons: i64,
    pub son_daughters: i64,
    pub brothers: i64,
    pub sisters: i64,
    pub paternal_brothers: i64,
    pub paternal_sisters: i64,
    pub uterine_brothers: i64,
    pub uterine_sisters: i64,
    pub full_brother_sons: i64,
    pub paternal_brother_sons: i64,
    pub full_uncles: i64,
    pub paternal_uncles: i64,
    pub full_uncle_sons: i64,
    pub paternal_uncle_sons: i64,
    pub daughter_sons: i64,
    pub daughter_daughters: i64,
    pub son_daughter_sons: i64,
    pub son_daughter_daughters: i64,
    pub maternal_grandfather: i64,
    pub remote_grandmothers: i64,
    pub sister_sons: i64,
    pub sister_daughters: i64,
    pub full_brother_daughters: i64,
    pub paternal_brother_daughters: i64,
    pub uterine_brother_children: i64,
    pub uterine_sister_children: i64,
    pub maternal_uncles: i64,
    pub maternal_aunts: i64,
    pub maternal_paternal_uncles: i64,
    pub paternal_aunts: i64,
    pub paternal_uncle_daughters: i64,
}

impl MirasFamily {
    /// Count by engine key (used for the distant-heir key lists and
    /// the blocking loops).
    pub fn count(&self, key: &str) -> i64 {
        match key {
            "wives" => self.wives,
            "sons" => self.sons,
            "daughters" => self.daughters,
            "son_sons" => self.son_sons,
            "son_daughters" => self.son_daughters,
            "brothers" => self.brothers,
            "sisters" => self.sisters,
            "paternal_brothers" => self.paternal_brothers,
            "paternal_sisters" => self.paternal_sisters,
            "uterine_brothers" => self.uterine_brothers,
            "uterine_sisters" => self.uterine_sisters,
            "full_brother_sons" => self.full_brother_sons,
            "paternal_brother_sons" => self.paternal_brother_sons,
            "full_uncles" => self.full_uncles,
            "paternal_uncles" => self.paternal_uncles,
            "full_uncle_sons" => self.full_uncle_sons,
            "paternal_uncle_sons" => self.paternal_uncle_sons,
            "daughter_sons" => self.daughter_sons,
            "daughter_daughters" => self.daughter_daughters,
            "son_daughter_sons" => self.son_daughter_sons,
            "son_daughter_daughters" => self.son_daughter_daughters,
            "maternal_grandfather" => self.maternal_grandfather,
            "remote_grandmothers" => self.remote_grandmothers,
            "sister_sons" => self.sister_sons,
            "sister_daughters" => self.sister_daughters,
            "full_brother_daughters" => self.full_brother_daughters,
            "paternal_brother_daughters" => self.paternal_brother_daughters,
            "uterine_brother_children" => self.uterine_brother_children,
            "uterine_sister_children" => self.uterine_sister_children,
            "maternal_uncles" => self.maternal_uncles,
            "maternal_aunts" => self.maternal_aunts,
            "maternal_paternal_uncles" => self.maternal_paternal_uncles,
            "paternal_aunts" => self.paternal_aunts,
            "paternal_uncle_daughters" => self.paternal_uncle_daughters,
            _ => 0,
        }
    }
}

pub const DISTANT_KEYS: &[&str] = &[
    "daughter_sons",
    "daughter_daughters",
    "son_daughter_sons",
    "son_daughter_daughters",
    "maternal_grandfather",
    "remote_grandmothers",
    "sister_sons",
    "sister_daughters",
    "full_brother_daughters",
    "paternal_brother_daughters",
    "uterine_brother_children",
    "uterine_sister_children",
    "maternal_uncles",
    "maternal_aunts",
    "maternal_paternal_uncles",
    "paternal_aunts",
    "paternal_uncle_daughters",
];

/// The estate side of `MirasIn` (funeral/debt/will/impediments).
#[derive(Debug, Clone)]
pub struct MirasEstate {
    pub estate_value: Dec,
    pub funeral: Dec,
    pub debt: Dec,
    pub will: Dec,
    pub will_for_heir: bool,
    pub will_approved: bool,
    pub religion_mixed: bool,
    pub killer: bool,
    pub simultaneous_death: bool,
    pub slavery: bool,
}

#[derive(Debug, Clone)]
struct DistRow {
    key: String,
    count: i64,
    share: Frac,
    /// `Some` only for father/grandfather; the merged asaba part.
    asaba_part: Option<Frac>,
}

#[derive(Debug, Default)]
struct Dist {
    rows: Vec<DistRow>,
    blocked: Vec<String>,
    notes: Vec<String>,
}

impl Dist {
    fn add_share(&mut self, key: &str, count: i64, share: Frac) -> Result<(), CalcError> {
        if count <= 0 || share <= Frac::zero() {
            return Ok(());
        }
        if let Some(row) = self.rows.iter_mut().find(|r| r.key == key) {
            row.share = row.share.add(&share)?;
            row.count = row.count.max(count);
            if key == "father" || key == "grandfather" {
                if let Some(part) = row.asaba_part {
                    row.asaba_part = Some(part.add(&share)?);
                }
            }
            return Ok(());
        }
        let asaba_part = if key == "father" || key == "grandfather" {
            Some(Frac::zero())
        } else {
            None
        };
        self.rows.push(DistRow {
            key: key.to_string(),
            count,
            share,
            asaba_part,
        });
        Ok(())
    }

    fn block(&mut self, key: &str) {
        if !self.blocked.iter().any(|k| k == key) {
            self.blocked.push(key.to_string());
        }
    }

    fn note(&mut self, code: &str) {
        if !self.notes.iter().any(|n| n == code) {
            self.notes.push(code.to_string());
        }
    }

    fn total(&self) -> Result<Frac, CalcError> {
        let mut t = Frac::zero();
        for r in &self.rows {
            t = t.add(&r.share)?;
        }
        Ok(t)
    }
}

fn miras_frac(v: Option<&Value>) -> Result<Frac, CalcError> {
    frac_from_rule(v, "miras")
}

pub fn compute_miras(
    family_in: &MirasFamily,
    estate: &MirasEstate,
    rule_data: &Value,
) -> Result<Value, MirasFail> {
    if rule_data.get("format").and_then(|v| v.as_str()) != Some("miras_v1") {
        return Err(MirasFail::Err(CalcError::new(
            "unsupported_rule_format",
            "قالب rule_data میراث پشتیبانی نمی‌شود",
        )));
    }
    let cfg = rule_data
        .get("shares")
        .ok_or_else(|| CalcError::internal("miras shares در rule_data نیست"))?;

    // ---- validate + normalise family (structural spouse rules) ----
    if family_in.gender != "male" && family_in.gender != "female" {
        return Err(MirasFail::Err(CalcError::new(
            "invalid_gender",
            "جنسیت متوفی نامعتبر است",
        )));
    }
    let mut s = family_in.clone();
    if s.gender == "female" {
        s.wives = 0;
    } else {
        s.husband = false;
    }
    if s.gender == "male" && s.wives > 4 {
        return Err(MirasFail::Err(CalcError::new(
            "too_many_wives",
            "تعداد همسران بیش از چهار است",
        )));
    }
    for key in [
        "wives",
        "sons",
        "daughters",
        "son_sons",
        "son_daughters",
        "brothers",
        "sisters",
        "paternal_brothers",
        "paternal_sisters",
        "uterine_brothers",
        "uterine_sisters",
        "full_brother_sons",
        "paternal_brother_sons",
        "full_uncles",
        "paternal_uncles",
        "full_uncle_sons",
        "paternal_uncle_sons",
    ]
    .into_iter()
    .chain(DISTANT_KEYS.iter().copied())
    {
        if s.count(key) < 0 {
            return Err(MirasFail::Err(CalcError::new(
                "invalid_family",
                "ورودی خانواده نامعتبر است",
            )));
        }
    }

    // ---- estate stages: funeral + debt, then will, then net ----
    let total = estate.estate_value;
    let funeral = estate.funeral;
    let debt = estate.debt;
    let will_in = estate.will;
    let mut net0 = total.sub(&funeral)?.sub(&debt)?;
    if net0 < Dec::zero() {
        net0 = Dec::zero();
    }

    if estate.religion_mixed {
        return Err(MirasFail::Stopped("impediment_religion"));
    }
    if estate.killer {
        return Err(MirasFail::Stopped("impediment_killer"));
    }
    if estate.simultaneous_death {
        return Err(MirasFail::Stopped("impediment_simultaneous_death"));
    }
    if estate.slavery {
        return Err(MirasFail::Stopped("impediment_slavery"));
    }

    let cap = miras_frac(rule_data.get("will").and_then(|v| v.get("non_heir_cap")))?;
    let cap_amount = Dec::raw(cap.n, 0).div28(&Dec::raw(cap.d, 0))?.mul(&net0)?;
    let will_applied = if estate.will_for_heir {
        if estate.will_approved && will_in <= net0 {
            will_in
        } else {
            Dec::zero()
        }
    } else if will_in <= cap_amount {
        will_in
    } else if estate.will_approved {
        if will_in <= net0 {
            will_in
        } else {
            net0
        }
    } else {
        cap_amount
    };
    let net = net0.sub(&will_applied)?;
    if net <= Dec::zero() {
        return Err(MirasFail::Stopped("nothing_to_distribute"));
    }

    let mut d = Dist::default();
    let desc = (s.sons + s.daughters + s.son_sons + s.son_daughters) > 0;
    let male_desc = (s.sons + s.son_sons) > 0;
    let female_desc = (s.daughters + s.son_daughters) > 0;
    let sib_count = s.brothers
        + s.sisters
        + s.paternal_brothers
        + s.paternal_sisters
        + s.uterine_brothers
        + s.uterine_sisters;
    let spouse_present = (s.gender == "female" && s.husband) || (s.gender == "male" && s.wives > 0);
    let uterine_count = s.uterine_brothers + s.uterine_sisters;
    let mother_cfg = cfg
        .get("mother")
        .ok_or_else(|| CalcError::internal("miras mother در rule_data نیست"))?;
    let sib_threshold = mother_cfg
        .get("sibling_threshold")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| CalcError::internal("miras sibling_threshold نیست"))?;

    // ---- 1) spouse ----
    if s.gender == "female" && s.husband {
        let c = cfg
            .get("husband")
            .ok_or_else(|| CalcError::internal("miras husband نیست"))?;
        d.add_share(
            "husband",
            1,
            miras_frac(c.get(if desc {
                "with_descendants"
            } else {
                "without_descendants"
            }))?,
        )?;
    }
    if s.gender == "male" && s.wives > 0 {
        let c = cfg
            .get("wives")
            .ok_or_else(|| CalcError::internal("miras wives نیست"))?;
        d.add_share(
            "wives",
            s.wives,
            miras_frac(c.get(if desc {
                "with_descendants"
            } else {
                "without_descendants"
            }))?,
        )?;
    }

    // ---- 2) mother (adjusted later under umariyya) ----
    let umariyya = !desc && s.father && s.mother && spouse_present && sib_count < sib_threshold;
    if s.mother && !umariyya {
        if desc || sib_count >= sib_threshold {
            d.add_share("mother", 1, miras_frac(mother_cfg.get("reduced"))?)?;
        } else {
            d.add_share("mother", 1, miras_frac(mother_cfg.get("full"))?)?;
        }
    }

    // ---- 3) grandmothers (blocked by mother; paternal by father) ----
    if s.mother {
        if s.mgm {
            d.block("mgm");
        }
        if s.pgm {
            d.block("pgm");
        }
    }
    if s.father && s.pgm {
        d.block("pgm");
    }
    let mut grandmothers = 0;
    if !s.mother {
        grandmothers = (if s.mgm { 1 } else { 0 }) + (if s.pgm && !s.father { 1 } else { 0 });
    }
    if grandmothers > 0 {
        d.add_share(
            "grandmothers",
            grandmothers,
            miras_frac(cfg.get("grandmothers"))?,
        )?;
    }

    // ---- 4) father's / grandfather's fixed share ----
    if s.father {
        if male_desc {
            d.add_share("father", 1, miras_frac(cfg.get("father_fixed"))?)?;
        } else if s.daughters > 0 || s.son_daughters > 0 {
            d.add_share("father", 1, miras_frac(cfg.get("father_fixed"))?)?;
            d.note("father_fixed_plus_asaba");
        }
    } else if s.grandfather {
        if male_desc {
            d.add_share("grandfather", 1, miras_frac(cfg.get("grandfather_fixed"))?)?;
        } else if s.daughters > 0 || s.son_daughters > 0 {
            d.add_share("grandfather", 1, miras_frac(cfg.get("grandfather_fixed"))?)?;
            d.note("grandfather_fixed_plus_asaba");
        }
    }

    // Hanafi blocking: a true grandfather (no father) blocks siblings.
    if !s.father && s.grandfather {
        for key in [
            "brothers",
            "sisters",
            "paternal_brothers",
            "paternal_sisters",
            "uterine_brothers",
            "uterine_sisters",
            "full_brother_sons",
            "paternal_brother_sons",
            "full_uncles",
            "paternal_uncles",
            "full_uncle_sons",
            "paternal_uncle_sons",
        ] {
            if s.count(key) > 0 {
                d.block(key);
            }
        }
        d.note("grandfather_blocks_siblings");
    }

    // ---- 5) uterine siblings (blocked by descendants/father/grandfather) ----
    if desc || s.father || s.grandfather {
        if s.uterine_brothers > 0 {
            d.block("uterine_brothers");
        }
        if s.uterine_sisters > 0 {
            d.block("uterine_sisters");
        }
    } else if uterine_count == 1 {
        d.add_share(
            "uterine",
            1,
            miras_frac(cfg.get("uterine").and_then(|v| v.get("single")))?,
        )?;
    } else if uterine_count >= 2 {
        d.add_share(
            "uterine",
            uterine_count,
            miras_frac(cfg.get("uterine").and_then(|v| v.get("multiple")))?,
        )?;
    }

    // ---- 6) daughters and son's daughters (fixed) ----
    if s.sons == 0 && s.daughters > 0 {
        let c = cfg
            .get("daughters")
            .ok_or_else(|| CalcError::internal("miras daughters نیست"))?;
        d.add_share(
            "daughters",
            s.daughters,
            miras_frac(c.get(if s.daughters == 1 {
                "single"
            } else {
                "multiple"
            }))?,
        )?;
    }
    if s.sons > 0 && s.son_daughters > 0 {
        d.block("son_daughters");
    } else if s.son_sons == 0 && s.son_daughters > 0 {
        let c = cfg
            .get("son_daughters")
            .ok_or_else(|| CalcError::internal("miras son_daughters نیست"))?;
        if s.daughters == 0 {
            d.add_share(
                "sonDaughters",
                s.son_daughters,
                miras_frac(c.get(if s.son_daughters == 1 {
                    "single"
                } else {
                    "multiple"
                }))?,
            )?;
        } else if s.daughters == 1 {
            d.add_share(
                "sonDaughters",
                s.son_daughters,
                miras_frac(c.get("completion"))?,
            )?;
        } else {
            d.block("son_daughters");
        }
    }

    // ---- 7) full sisters (fixed, when not asaba ma'a al-ghayr) ----
    let can_full_fixed = !male_desc && !s.father && !s.grandfather && s.brothers == 0;
    if s.brothers == 0 && s.sisters > 0 && !female_desc && can_full_fixed {
        let c = cfg
            .get("full_sisters")
            .ok_or_else(|| CalcError::internal("miras full_sisters نیست"))?;
        d.add_share(
            "fullSisters",
            s.sisters,
            miras_frac(c.get(if s.sisters == 1 { "single" } else { "multiple" }))?,
        )?;
    }

    // ---- 8) paternal sisters (fixed/completion/group — source branches) ----
    if s.paternal_brothers == 0 && s.paternal_sisters > 0 {
        if s.brothers > 0 || (s.sisters > 0 && female_desc) || (s.sisters >= 2 && !female_desc) {
            d.block("paternal_sisters");
        } else if female_desc {
            // asaba ma'a al-ghayr; handled at the asaba stage
        } else if s.sisters == 1 && !male_desc && !s.father && !s.grandfather {
            d.add_share(
                "paternalSisters",
                s.paternal_sisters,
                miras_frac(
                    cfg.get("paternal_sisters")
                        .and_then(|v| v.get("completion")),
                )?,
            )?;
        } else if !male_desc && !s.father && !s.grandfather {
            // Source behaviour: a group of paternal sisters takes 2/3
            // (even a single one).
            d.add_share(
                "paternalSisters",
                s.paternal_sisters,
                miras_frac(cfg.get("paternal_sisters").and_then(|v| v.get("group")))?,
            )?;
        }
    }

    // ---- 9) umariyya: mother takes a third of the remainder ----
    if umariyya {
        let spouse_share = if s.gender == "female" {
            miras_frac(
                cfg.get("husband")
                    .and_then(|v| v.get("without_descendants")),
            )?
        } else {
            miras_frac(cfg.get("wives").and_then(|v| v.get("without_descendants")))?
        };
        let remainder = Frac::one().sub(&spouse_share)?;
        let frac_of_rest = miras_frac(
            rule_data
                .get("umariyya")
                .and_then(|v| v.get("mother_fraction_of_remainder")),
        )?;
        d.add_share("mother", 1, remainder.mul(&frac_of_rest)?)?;
        d.note("umariyya");
    }

    let fixed = d.total()?;

    // ---- 10) awl, asaba, or radd / dhawu al-arham ----
    let mut awl = false;
    let mut radd_applied = false;
    let mut dhawu_used = false;
    if fixed > Frac::one() {
        let factor = Frac::one().div(&fixed)?;
        for row in d.rows.iter_mut() {
            row.share = row.share.mul(&factor)?;
        }
        awl = true;
        d.note("awl");
    } else {
        let residue = Frac::one().sub(&fixed)?;
        let asaba_key = select_asaba(&s, female_desc);
        if residue > Frac::zero() {
            if let Some(key) = asaba_key {
                apply_asaba(&mut d, &s, key, residue)?;
            } else {
                dhawu_used = !d
                    .rows
                    .iter()
                    .any(|r| r.share > Frac::zero() && r.key != "husband" && r.key != "wives");
                radd_applied = apply_residue(
                    &mut d,
                    &s,
                    rule_data,
                    residue,
                    dhawu_used,
                    desc,
                    spouse_present,
                )?;
            }
        }
    }

    // ---- source safety lock: multi-branch dhawu -> no result ----
    let distant_present: Vec<&str> = DISTANT_KEYS
        .iter()
        .copied()
        .filter(|k| s.count(k) > 0)
        .collect();
    if !distant_present.is_empty() && dhawu_used && distant_present.len() > 1 {
        return Err(MirasFail::Stopped("dhawu_multibranch"));
    }
    if !distant_present.is_empty() && !dhawu_used {
        for key in &distant_present {
            d.block(key);
        }
    }

    // ---- 11) final blocking ----
    if s.father && s.grandfather {
        d.block("grandfather");
    }
    if male_desc || s.father || s.grandfather {
        for key in [
            "brothers",
            "sisters",
            "paternal_brothers",
            "paternal_sisters",
        ] {
            if s.count(key) > 0 {
                d.block(key);
            }
        }
    }
    if s.sons > 0 {
        if s.son_sons > 0 {
            d.block("son_sons");
        }
        if s.son_daughters > 0 {
            d.block("son_daughters");
        }
    }
    if s.father || s.grandfather || s.sons > 0 || s.son_sons > 0 {
        for key in [
            "full_brother_sons",
            "paternal_brother_sons",
            "full_uncles",
            "paternal_uncles",
            "full_uncle_sons",
            "paternal_uncle_sons",
        ] {
            if s.count(key) > 0 {
                d.block(key);
            }
        }
    }
    if s.brothers > 0 && s.full_brother_sons > 0 {
        d.block("full_brother_sons");
    }
    if (s.brothers > 0 || s.full_brother_sons > 0) && s.paternal_brother_sons > 0 {
        d.block("paternal_brother_sons");
    }
    if s.full_uncles > 0 && s.paternal_uncles > 0 {
        d.block("paternal_uncles");
    }
    if s.full_uncle_sons > 0 && s.paternal_uncle_sons > 0 {
        d.block("paternal_uncle_sons");
    }
    if female_desc && s.sisters > 0 && !s.father && !s.grandfather && !male_desc && s.brothers == 0
    {
        if s.paternal_brothers > 0 {
            d.block("paternal_brothers");
        }
        if s.paternal_sisters > 0 {
            d.block("paternal_sisters");
        }
    }

    // ---- 12) akdariyya (Abu Hanifa: a sister does not share with a grandfather) ----
    let akdariyya = s.gender == "female"
        && s.husband
        && s.mother
        && s.grandfather
        && s.sisters == 1
        && s.brothers == 0
        && s.paternal_brothers == 0
        && s.paternal_sisters == 0
        && !desc
        && uterine_count == 0;
    if akdariyya {
        d.rows
            .retain(|r| r.key != "fullSisters" && r.key != "sisters");
        if let Some(pos) = d.blocked.iter().position(|k| k == "sisters") {
            d.blocked.remove(pos);
        }
        d.block("sisters");
        d.note("akdariyya");
    }

    // ---- final control: never issue a partial result ----
    let positive_rows: Vec<&DistRow> = d.rows.iter().filter(|r| r.share > Frac::zero()).collect();
    if positive_rows.is_empty() {
        return Err(MirasFail::Stopped("no_distributable_heir"));
    }
    if d.total()? != Frac::one() {
        return Err(MirasFail::Stopped("incomplete_total"));
    }

    let mut rows_out: Vec<Value> = Vec::new();
    for row in &d.rows {
        let share = row.share;
        let amount_dec = net
            .mul(&Dec::raw(share.n, 0))?
            .div28(&Dec::raw(share.d, 0))?;
        let mut item = Map::new();
        item.insert("key".to_string(), json!(row.key));
        item.insert("count".to_string(), json!(row.count));
        item.insert("share".to_string(), json!(share.to_py_string()));
        item.insert("amount".to_string(), json!(money(amount_dec)?));
        item.insert(
            "per_person_amount".to_string(),
            json!(money(amount_dec.div28(&Dec::from_int(row.count))?)?),
        );
        if let Some(part) = row.asaba_part {
            if !part.is_zero() {
                item.insert("asaba_part".to_string(), json!(part.to_py_string()));
            }
        }
        rows_out.push(Value::Object(item));
    }

    Ok(json!({
        "status": "computed",
        "estate_value": total.to_py_string(),
        "funeral": funeral.to_py_string(),
        "debt": debt.to_py_string(),
        "will_applied": will_applied.to_py_string(),
        "net_estate": net.to_py_string(),
        "awl": awl,
        "radd": radd_applied,
        "rows": rows_out,
        "blocked": d.blocked,
        "notes": d.notes,
    }))
}

fn select_asaba(s: &MirasFamily, female_desc: bool) -> Option<&'static str> {
    if s.sons > 0 {
        return Some("sons");
    }
    if s.son_sons > 0 {
        return Some("sonSons");
    }
    if s.father {
        return Some("father");
    }
    if s.grandfather {
        return Some("grandfather");
    }
    if s.brothers > 0 {
        return Some("brothers");
    }
    if female_desc && s.sisters > 0 && !s.father && !s.grandfather {
        return Some("sistersFemaleDesc");
    }
    if s.paternal_brothers > 0 {
        return Some("pBrothers");
    }
    if female_desc
        && s.paternal_sisters > 0
        && s.sisters == 0
        && !s.father
        && !s.grandfather
        && s.brothers == 0
    {
        return Some("pSistersFemaleDesc");
    }
    if s.full_brother_sons > 0 {
        return Some("fullBrotherSons");
    }
    if s.paternal_brother_sons > 0 {
        return Some("paternalBrotherSons");
    }
    if s.full_uncles > 0 {
        return Some("fullUncles");
    }
    if s.paternal_uncles > 0 {
        return Some("paternalUncles");
    }
    if s.full_uncle_sons > 0 {
        return Some("fullUncleSons");
    }
    if s.paternal_uncle_sons > 0 {
        return Some("paternalUncleSons");
    }
    None
}

fn apply_asaba(d: &mut Dist, s: &MirasFamily, key: &str, residue: Frac) -> Result<(), CalcError> {
    fn grouped(
        d: &mut Dist,
        males: i64,
        females: i64,
        row_m: &str,
        row_f: Option<&str>,
        residue: Frac,
    ) -> Result<(), CalcError> {
        let units = 2 * males + females;
        let unit = residue.div(&Frac::from_int(units))?;
        d.add_share(row_m, males, unit.mul_int(2 * males)?)?;
        if females > 0 {
            if let Some(rf) = row_f {
                d.add_share(rf, females, unit.mul_int(females)?)?;
            }
        }
        Ok(())
    }

    match key {
        "sons" => grouped(
            d,
            s.sons,
            s.daughters,
            "sons",
            Some("daughtersAsabah"),
            residue,
        ),
        "sonSons" => grouped(
            d,
            s.son_sons,
            s.son_daughters,
            "sonSons",
            Some("sonDaughtersAsabah"),
            residue,
        ),
        "father" => {
            d.add_share("father", 1, residue)?;
            d.note("father_asaba");
            Ok(())
        }
        "grandfather" => {
            d.add_share("grandfather", 1, residue)?;
            d.note("grandfather_asaba");
            Ok(())
        }
        "brothers" => grouped(
            d,
            s.brothers,
            s.sisters,
            "brothers",
            Some("sistersAsabah"),
            residue,
        ),
        "sistersFemaleDesc" => {
            d.add_share("sistersFemaleDesc", s.sisters, residue)?;
            d.note("sisters_with_female_descendants");
            Ok(())
        }
        "pBrothers" => grouped(
            d,
            s.paternal_brothers,
            s.paternal_sisters,
            "pBrothers",
            Some("pSistersAsabah"),
            residue,
        ),
        "pSistersFemaleDesc" => {
            d.add_share("pSistersFemaleDesc", s.paternal_sisters, residue)?;
            d.note("sisters_with_female_descendants");
            Ok(())
        }
        _ => {
            let counts = [
                ("fullBrotherSons", s.full_brother_sons),
                ("paternalBrotherSons", s.paternal_brother_sons),
                ("fullUncles", s.full_uncles),
                ("paternalUncles", s.paternal_uncles),
                ("fullUncleSons", s.full_uncle_sons),
                ("paternalUncleSons", s.paternal_uncle_sons),
            ];
            let count = counts
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, c)| *c)
                .unwrap_or(0);
            d.add_share(key, count, residue)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_residue(
    d: &mut Dist,
    s: &MirasFamily,
    rule_data: &Value,
    residue: Frac,
    dhawu_used: bool,
    desc: bool,
    spouse_present: bool,
) -> Result<bool, CalcError> {
    fn do_radd(
        d: &mut Dist,
        s: &MirasFamily,
        rule_data: &Value,
        desc: bool,
        spouse_present: bool,
    ) -> Result<bool, CalcError> {
        let cfg = rule_data
            .get("shares")
            .ok_or_else(|| CalcError::internal("miras shares نیست"))?;
        let excluded: Vec<String> = rule_data
            .get("radd")
            .and_then(|v| v.get("excluded_keys"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let eligible: Vec<usize> = d
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !excluded.contains(&r.key) && r.share > Frac::zero())
            .map(|(i, _)| i)
            .collect();
        let mut total_fixed = Frac::zero();
        for &i in &eligible {
            total_fixed = total_fixed.add(&d.rows[i].share)?;
        }
        if total_fixed <= Frac::zero() {
            d.note("residue_unallocated");
            return Ok(false);
        }
        let target = if spouse_present {
            let spouse_fixed = if s.gender == "female" {
                miras_frac(cfg.get("husband").and_then(|v| {
                    v.get(if desc {
                        "with_descendants"
                    } else {
                        "without_descendants"
                    })
                }))?
            } else {
                miras_frac(cfg.get("wives").and_then(|v| {
                    v.get(if desc {
                        "with_descendants"
                    } else {
                        "without_descendants"
                    })
                }))?
            };
            Frac::one().sub(&spouse_fixed)?
        } else {
            Frac::one()
        };
        let factor = target.div(&total_fixed)?;
        for &i in &eligible {
            d.rows[i].share = d.rows[i].share.mul(&factor)?;
        }
        d.note("radd");
        Ok(true)
    }

    if !dhawu_used {
        return do_radd(d, s, rule_data, desc, spouse_present);
    }

    let pairs: [(&str, &str, &str, &str); 2] = [
        (
            "daughter_sons",
            "daughter_daughters",
            "dhawu_daughterSons",
            "dhawu_daughterDaughters",
        ),
        (
            "son_daughter_sons",
            "son_daughter_daughters",
            "dhawu_sonDaughterSons",
            "dhawu_sonDaughterDaughters",
        ),
    ];
    let class1: Vec<(&str, &str, &str, &str)> = pairs
        .iter()
        .copied()
        .filter(|(m, f, _, _)| s.count(m) > 0 || s.count(f) > 0)
        .collect();
    let class2: Vec<&str> = ["maternal_grandfather", "remote_grandmothers"]
        .iter()
        .copied()
        .filter(|k| s.count(k) > 0)
        .collect();
    let class3_keys = [
        "sister_sons",
        "sister_daughters",
        "full_brother_daughters",
        "paternal_brother_daughters",
        "uterine_brother_children",
        "uterine_sister_children",
    ];
    let class3: Vec<&str> = class3_keys
        .iter()
        .copied()
        .filter(|k| s.count(k) > 0)
        .collect();
    let class4: Vec<&str> = [
        "maternal_uncles",
        "maternal_aunts",
        "maternal_paternal_uncles",
        "paternal_aunts",
        "paternal_uncle_daughters",
    ]
    .iter()
    .copied()
    .filter(|k| s.count(k) > 0)
    .collect();

    // Combined class-3 rule (the Imam Muhammad position in the source).
    if s.full_brother_daughters > 0
        && (s.sister_sons > 0 || s.sister_daughters > 0)
        && class3.iter().all(|k| {
            matches!(
                *k,
                "full_brother_daughters" | "sister_sons" | "sister_daughters"
            )
        })
    {
        let total_units = 2 * s.full_brother_daughters + s.sister_sons + s.sister_daughters;
        let unit = residue.div(&Frac::from_int(total_units))?;
        d.add_share(
            "dhawu_class3_fullBrotherDaughters_mix",
            s.full_brother_daughters,
            unit.mul_int(2)?,
        )?;
        let children = s.sister_sons + s.sister_daughters;
        let group_share = unit.mul_int(children)?;
        let denom = 2 * s.sister_sons + s.sister_daughters;
        if s.sister_sons > 0 {
            d.add_share(
                "dhawu_class3_sisterSons_mix",
                s.sister_sons,
                group_share.mul(&Frac::new(2 * s.sister_sons as i128, denom as i128)?)?,
            )?;
        }
        if s.sister_daughters > 0 {
            d.add_share(
                "dhawu_class3_sisterDaughters_mix",
                s.sister_daughters,
                group_share.mul(&Frac::new(s.sister_daughters as i128, denom as i128)?)?,
            )?;
        }
        d.note("dhawu_class3_mix");
        return Ok(false);
    }

    if class1.len() == 1 {
        let (m_key, f_key, row_m, row_f) = class1[0];
        let (m, f) = (s.count(m_key), s.count(f_key));
        if m > 0 && f > 0 {
            let unit = residue.div(&Frac::from_int(2 * m + f))?;
            // Source behaviour: the PER-PERSON share is recorded,
            // not the group share.
            d.add_share(row_m, m, unit.mul_int(2)?)?;
            d.add_share(row_f, f, unit)?;
        } else if m > 0 {
            d.add_share(row_m, m, residue.div(&Frac::from_int(m))?)?;
        } else {
            d.add_share(row_f, f, residue.div(&Frac::from_int(f))?)?;
        }
        d.note("dhawu_class1");
        return Ok(false);
    }
    if class1.len() > 1 {
        d.note("dhawu_class1_multibranch");
        return Ok(false);
    }
    if class2.len() == 1 {
        let key = class2[0];
        let row_key = match key {
            "maternal_grandfather" => "dhawu_class2_maternalGrandfather",
            _ => "dhawu_class2_remoteGrandmothers",
        };
        d.add_share(
            row_key,
            s.count(key),
            residue.div(&Frac::from_int(s.count(key)))?,
        )?;
        d.note("dhawu_class2");
        return Ok(false);
    }
    if class2.len() > 1 {
        d.note("dhawu_class2_multibranch");
        return Ok(false);
    }
    if (s.sister_sons > 0 || s.sister_daughters > 0)
        && class3
            .iter()
            .all(|k| matches!(*k, "sister_sons" | "sister_daughters"))
    {
        let (m, f) = (s.sister_sons, s.sister_daughters);
        let unit = residue.div(&Frac::from_int(2 * m + f))?;
        if m > 0 {
            d.add_share("dhawu_class3_sisterSons", m, unit.mul_int(2)?)?;
        }
        if f > 0 {
            d.add_share("dhawu_class3_sisterDaughters", f, unit)?;
        }
        d.note("dhawu_class3");
        return Ok(false);
    }
    if class3.len() == 1 {
        let key = class3[0];
        let row_key = match key {
            "sister_sons" => "dhawu_class3_sisterSons",
            "sister_daughters" => "dhawu_class3_sisterDaughters",
            "full_brother_daughters" => "dhawu_class3_fullBrotherDaughters",
            "paternal_brother_daughters" => "dhawu_class3_paternalBrotherDaughters",
            "uterine_brother_children" => "dhawu_class3_uterineBrotherChildren",
            _ => "dhawu_class3_uterineSisterChildren",
        };
        d.add_share(row_key, s.count(key), residue)?;
        d.note("dhawu_class3");
        return Ok(false);
    }
    if class3.len() > 1 {
        d.note("dhawu_class3_multibranch");
        return Ok(false);
    }
    if class4.len() == 1 {
        let key = class4[0];
        let row_key = match key {
            "maternal_uncles" => "dhawu_class4_maternalUncles",
            "maternal_aunts" => "dhawu_class4_maternalAunts",
            "maternal_paternal_uncles" => "dhawu_class4_maternalPaternalUncles",
            "paternal_aunts" => "dhawu_class4_paternalAunts",
            _ => "dhawu_class4_paternalUncleDaughters",
        };
        d.add_share(
            row_key,
            s.count(key),
            residue.div(&Frac::from_int(s.count(key)))?,
        )?;
        d.note("dhawu_class4");
        return Ok(false);
    }
    if class4.len() > 1 {
        d.note("dhawu_class4_multibranch");
        return Ok(false);
    }
    do_radd(d, s, rule_data, desc, spouse_present)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_rule() -> Value {
        json!({
            "note": "SYNTHETIC TEST FIXTURE — not fiqh, engine math only",
            "shares": {
                "spouse_t": {"share": "1/4"},
                "mother_t": {"share": "1/6"},
            },
            "awl": {"enabled": true},
            "radd": {"enabled": true, "excluded": ["spouse_t"]},
            "asaba_priority": ["child_t"],
        })
    }

    fn synth_zakat_rule() -> Value {
        json!({
            "note": "SYNTHETIC TEST FIXTURE — not fiqh",
            "zakat_rate": "0.1",
            "nisab_basis": "gold",
            "nisab_gold_grams": "100",
            "nisab_silver_grams": "700",
            "asset_categories": {
                "cash": {"type": "value"},
                "trade_goods": {"type": "value"},
                "gold_grams": {"type": "grams", "price": "gold_per_gram"},
            },
        })
    }

    fn synth_rates() -> ZakatRates {
        ZakatRates {
            gold_per_gram: Some(Dec::from_int(10)),
            silver_per_gram: Some(Dec::from_int(1)),
            currency: Some("TST".to_string()),
            stale: false,
        }
    }

    /// Verbatim copy of `ZAKAT_SOURCE_RULE_DATA` from
    /// `app/modules/calculators/source_rules.py` (data, not fiqh
    /// invention) — the approved-rule payload the HTTP tests seed.
    pub fn zakat_source_rule_data() -> Value {
        json!({
            "format": "source_extended_v1",
            "money": {"nisab_coins": "10.4545454545", "zakat_at_nisab_coins": "0.2613636364", "step_coins": "2.0909090909", "step_zakat_coins": "0.0727272727"},
            "gold": {"nisab_grams": "92", "zakat_at_nisab_grams": "2.3", "step_grams": "18.4", "step_zakat_grams": "0.46"},
            "silver": {"nisab_grams": "644", "zakat_at_nisab_grams": "16.1", "step_grams": "128.8", "step_zakat_grams": "3.22", "input_round_grams": "0.1"},
            "sheep": {"nisab": 40, "table": [[120, 1], [200, 2], [399, 3]], "per_hundred": {"from": 400, "divisor": 100}},
            "camel": {"table": [[4, []], [9, [["sheep", null, 1]]], [14, [["sheep", null, 2]]], [19, [["sheep", null, 3]]], [24, [["sheep", null, 4]]], [35, [["camel_female", 1, 1]]], [45, [["camel_female", 2, 1]]], [60, [["camel_female", 3, 1]]], [75, [["camel_female", 4, 1]]], [90, [["camel_female", 2, 2]]], [124, [["camel_female", 3, 2]]], [149, [["camel_female", 3, 2], ["camel_female", 1, 1]]], [150, [["camel_female", 3, 3]]]], "over": {"threshold": 150, "base_per": 50, "base_due": ["camel_female", 3, 1], "remainder_table": [[4, []], [9, [["sheep", null, 1]]], [14, [["sheep", null, 2]]], [19, [["sheep", null, 3]]], [24, [["sheep", null, 4]]], [35, [["camel_female", 1, 1]]], [45, [["camel_female", 2, 1]]], [49, [["camel_female", 3, 1]]]]}},
            "cow": {"tabii_from": 30, "musinna_from": 40, "double_tabii_from": 60, "combo_from": 70, "combo_round_to": 10, "combo_units": [30, 40], "musinna_share_denominator": 40},
            "horse": {"per_head_dinar": 1, "value_rate": "1/40"},
            "oshr": {"rates": {"rain_fed": "1/10", "irrigated": "1/20"}},
            "khoms": {"rate": "0.2"},
        })
    }

    /// Verbatim copy of `MIRAS_SOURCE_RULE_DATA` from
    /// `app/modules/calculators/source_rules.py`.
    pub fn miras_source_rule_data() -> Value {
        json!({
            "format": "miras_v1",
            "shares": {
                "husband": {"with_descendants": "1/4", "without_descendants": "1/2"},
                "wives": {"with_descendants": "1/8", "without_descendants": "1/4"},
                "mother": {"reduced": "1/6", "full": "1/3", "sibling_threshold": 2},
                "grandmothers": "1/6",
                "father_fixed": "1/6",
                "grandfather_fixed": "1/6",
                "uterine": {"single": "1/6", "multiple": "1/3"},
                "daughters": {"single": "1/2", "multiple": "2/3"},
                "son_daughters": {"single": "1/2", "multiple": "2/3", "completion": "1/6"},
                "full_sisters": {"single": "1/2", "multiple": "2/3"},
                "paternal_sisters": {"completion": "1/6", "group": "2/3"},
            },
            "umariyya": {"mother_fraction_of_remainder": "1/3"},
            "radd": {"excluded_keys": ["husband", "wives"]},
            "asaba_priority": ["sons", "sonSons", "father", "grandfather", "brothers", "sistersFemaleDesc", "pBrothers", "pSistersFemaleDesc", "fullBrotherSons", "paternalBrotherSons", "fullUncles", "paternalUncles", "fullUncleSons", "paternalUncleSons"],
            "will": {"non_heir_cap": "1/3"},
        })
    }

    fn dec(s: &str) -> Dec {
        Dec::parse(s).unwrap()
    }

    // --- phase-7 engine (synthetic fixtures, test_calculators_phase7.py) ---

    #[test]
    fn inheritance_fixed_shares_and_asaba() {
        let out = compute_inheritance(
            &[
                ("spouse_t".to_string(), 1),
                ("mother_t".to_string(), 1),
                ("child_t".to_string(), 2),
            ],
            dec("1200"),
            &synth_rule(),
            1,
        )
        .unwrap();
        let by_type: std::collections::HashMap<&str, &Value> = out["shares"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["heir_type"].as_str().unwrap(), s))
            .collect();
        assert_eq!(by_type["spouse_t"]["amount"], "300.0000");
        assert_eq!(by_type["mother_t"]["amount"], "200.0000");
        assert_eq!(by_type["child_t"]["amount"], "700.0000");
        assert_eq!(by_type["child_t"]["per_person_amount"], "350.0000");
        assert_eq!(out["total_allocated_fraction"], "1/1");
        assert!(out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().starts_with("asaba_applied")));
        assert_eq!(out["applied_rule_version"], 1);
    }

    #[test]
    fn inheritance_awl_scales_down() {
        let rule = json!({
            "shares": {
                "a_t": {"share": "1/2"},
                "b_t": {"share": "1/3"},
                "c_t": {"share": "1/4"},
            },
            "awl": {"enabled": true},
        });
        let out = compute_inheritance(
            &[
                ("a_t".to_string(), 1),
                ("b_t".to_string(), 1),
                ("c_t".to_string(), 1),
            ],
            dec("1300"),
            &rule,
            1,
        )
        .unwrap();
        let by_type: std::collections::HashMap<&str, &Value> = out["shares"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["heir_type"].as_str().unwrap(), s))
            .collect();
        assert_eq!(by_type["a_t"]["amount"], "600.0000");
        assert_eq!(by_type["b_t"]["amount"], "400.0000");
        assert_eq!(by_type["c_t"]["amount"], "300.0000");
        assert!(out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "awl_applied"));
    }

    #[test]
    fn inheritance_radd_returns_surplus() {
        let rule = json!({
            "shares": {"mother_t": {"share": "1/6"}},
            "radd": {"enabled": true, "excluded": []},
        });
        let out =
            compute_inheritance(&[("mother_t".to_string(), 1)], dec("600"), &rule, 1).unwrap();
        assert_eq!(out["shares"][0]["amount"], "600.0000");
        assert_eq!(out["shares"][0]["share_fraction"], "1/1");
        assert!(out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "radd_applied"));
    }

    #[test]
    fn inheritance_unknown_heir_and_empty_rule() {
        let err = compute_inheritance(&[("nope_t".to_string(), 1)], dec("100"), &synth_rule(), 1)
            .unwrap_err();
        assert_eq!(err.code, "unknown_heir");
        let err = compute_inheritance(&[("spouse_t".to_string(), 1)], dec("100"), &json!({}), 1)
            .unwrap_err();
        assert_eq!(err.code, "rule_data_empty");
    }

    #[test]
    fn zakat_ok_below_nisab_price_unavailable() {
        let out = compute_zakat(
            &[
                ("cash".to_string(), dec("500")),
                ("gold_grams".to_string(), dec("60")),
                ("trade_goods".to_string(), dec("0")),
            ],
            dec("100"),
            &synth_zakat_rule(),
            &synth_rates(),
            1,
        )
        .unwrap();
        assert_eq!(out["status"], "ok");
        assert_eq!(out["total_assets"], "1100.0000");
        assert_eq!(out["net_assets"], "1000.0000");
        assert_eq!(out["nisab_value"], "1000.0000");
        assert_eq!(out["nisab_met"], true);
        assert_eq!(out["zakat_due"], "100.0000");
        assert_eq!(out["price_stale"], false);

        let out = compute_zakat(
            &[("cash".to_string(), dec("50"))],
            dec("0"),
            &synth_zakat_rule(),
            &synth_rates(),
            1,
        )
        .unwrap();
        assert_eq!(out["nisab_met"], false);
        assert_eq!(out["zakat_due"], "0.0000");

        let rates = ZakatRates {
            gold_per_gram: None,
            ..synth_rates()
        };
        let out = compute_zakat(
            &[("gold_grams".to_string(), dec("10"))],
            dec("0"),
            &synth_zakat_rule(),
            &rates,
            1,
        )
        .unwrap();
        assert_eq!(out["status"], "price_unavailable");
    }

    // --- extended zakat (golden fixtures, zakat_golden.json) ---

    #[test]
    fn extended_golden_spot_checks() {
        let rule = zakat_source_rule_data();
        // test_zakat_money_is_decimal_exact
        let out = compute_zakat_extended(
            &ExtendedSections {
                money: Some((dec("200000000"), dec("10000000"))),
                ..Default::default()
            },
            &rule,
        )
        .unwrap();
        assert_eq!(out["money"]["zakat_coins"], "0.5522727272");
        assert_eq!(out["money"]["zakat_amount"], "5522727");
        // gold_100
        let out = compute_zakat_extended(
            &ExtendedSections {
                gold_grams: Some(dec("100")),
                ..Default::default()
            },
            &rule,
        )
        .unwrap();
        assert_eq!(out["gold"]["nisab_met"], true);
        assert_eq!(out["gold"]["zakat_grams"], "2.30");
        // silver rounding boundary (643.94 below, 643.95 at nisab)
        let out = compute_zakat_extended(
            &ExtendedSections {
                silver_grams: Some(dec("643.94")),
                ..Default::default()
            },
            &rule,
        )
        .unwrap();
        assert_eq!(out["silver"]["nisab_met"], false);
        let out = compute_zakat_extended(
            &ExtendedSections {
                silver_grams: Some(dec("643.95")),
                ..Default::default()
            },
            &rule,
        )
        .unwrap();
        assert_eq!(out["silver"]["nisab_met"], true);
        assert_eq!(out["silver"]["zakat_grams"], "16.10");
        // sheep_45, camel_30 (API test values)
        let out = compute_zakat_extended(
            &ExtendedSections {
                sheep: Some(45),
                camels: Some(30),
                gold_grams: Some(dec("100")),
                ..Default::default()
            },
            &rule,
        )
        .unwrap();
        assert_eq!(out["sheep"]["due"], json!([{"kind": "sheep", "count": 1}]));
        assert_eq!(
            out["camels"]["due"],
            json!([{"kind": "camel_female", "count": 1, "age_years": 1}])
        );
        // cow_45 with the source example price
        let out = compute_zakat_extended(
            &ExtendedSections {
                cows: Some((45, Some(dec("40000000")))),
                ..Default::default()
            },
            &rule,
        )
        .unwrap();
        assert_eq!(
            out["cows"]["due"],
            json!([{"kind": "cow_musinna", "count": 1}])
        );
        assert_eq!(out["cows"]["musinna_fortieths"], 5);
        assert_eq!(out["cows"]["extra_money"], "5000000");
        // cow_70 / cow_100 combo
        let out = compute_zakat_extended(
            &ExtendedSections {
                cows: Some((70, None)),
                ..Default::default()
            },
            &rule,
        )
        .unwrap();
        assert_eq!(
            out["cows"]["due"],
            json!([{"kind": "cow_tabii", "count": 1}, {"kind": "cow_musinna", "count": 1}])
        );
        let out = compute_zakat_extended(
            &ExtendedSections {
                cows: Some((100, None)),
                ..Default::default()
            },
            &rule,
        )
        .unwrap();
        assert_eq!(
            out["cows"]["due"],
            json!([{"kind": "cow_tabii", "count": 2}, {"kind": "cow_musinna", "count": 1}])
        );
        // camel_200 (over-150 rule)
        let out = compute_zakat_extended(
            &ExtendedSections {
                camels: Some(200),
                ..Default::default()
            },
            &rule,
        )
        .unwrap();
        assert_eq!(
            out["camels"]["due"],
            json!([{"kind": "camel_female", "count": 4, "age_years": 3}])
        );
        // oshr dimi 1000 -> 100 kg; khoms 1000 -> 200
        let out = compute_zakat_extended(
            &ExtendedSections {
                oshr: Some((dec("1000"), "rain_fed".to_string())),
                khoms: Some(dec("1000")),
                ..Default::default()
            },
            &rule,
        )
        .unwrap();
        assert_eq!(out["oshr"]["oshr_kg"], "100");
        assert_eq!(out["khoms"]["khoms"], "200");
        // horse: rule only, no numeric computation in the source
        let out = compute_zakat_extended(
            &ExtendedSections {
                horses: Some((0, None)),
                ..Default::default()
            },
            &rule,
        )
        .unwrap();
        assert_eq!(out["horses"]["rule_note"], "dinar_or_fortieth_per_horse");
    }

    #[test]
    fn extended_invalid_inputs() {
        let rule = zakat_source_rule_data();
        let err = compute_zakat_extended(&ExtendedSections::default(), &rule).unwrap_err();
        assert_eq!(err.code, "no_sections");
        let err = compute_zakat_extended(
            &ExtendedSections {
                oshr: Some((dec("5"), "drip".to_string())),
                ..Default::default()
            },
            &rule,
        )
        .unwrap_err();
        assert_eq!(err.code, "unknown_irrigation");
    }

    // --- miras (golden fixtures, miras_golden.json) ---

    fn estate(value: &str) -> MirasEstate {
        MirasEstate {
            estate_value: dec(value),
            funeral: dec("0"),
            debt: dec("0"),
            will: dec("0"),
            will_for_heir: false,
            will_approved: false,
            religion_mixed: false,
            killer: false,
            simultaneous_death: false,
            slavery: false,
        }
    }

    fn miras_shares(out: &Value) -> Vec<(&str, &str)> {
        out["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["key"].as_str().unwrap(), r["share"].as_str().unwrap()))
            .collect()
    }

    #[test]
    fn miras_golden_spot_checks() {
        let rule = miras_source_rule_data();
        // wife_mother_father_sons_daughters
        let out = compute_miras(
            &MirasFamily {
                gender: "male".to_string(),
                wives: 1,
                father: true,
                mother: true,
                sons: 2,
                daughters: 1,
                ..Default::default()
            },
            &estate("120000"),
            &rule,
        )
        .unwrap();
        assert_eq!(out["status"], "computed");
        assert_eq!(
            miras_shares(&out),
            [
                ("wives", "1/8"),
                ("mother", "1/6"),
                ("father", "1/6"),
                ("sons", "13/30"),
                ("daughtersAsabah", "13/120"),
            ]
        );
        assert_eq!(out["blocked"], json!([]));
        // API test: male 90000, wives 1, sons 1, daughters 1
        let out = compute_miras(
            &MirasFamily {
                gender: "male".to_string(),
                wives: 1,
                sons: 1,
                daughters: 1,
                ..Default::default()
            },
            &estate("90000"),
            &rule,
        )
        .unwrap();
        assert_eq!(
            miras_shares(&out),
            [
                ("wives", "1/8"),
                ("sons", "7/12"),
                ("daughtersAsabah", "7/24")
            ]
        );
        // umariyya_husband
        let out = compute_miras(
            &MirasFamily {
                gender: "female".to_string(),
                husband: true,
                father: true,
                mother: true,
                ..Default::default()
            },
            &estate("60000"),
            &rule,
        )
        .unwrap();
        assert_eq!(
            miras_shares(&out),
            [("husband", "1/2"), ("mother", "1/6"), ("father", "1/3")]
        );
        // awl_husband_mother_sisters_uterine
        let out = compute_miras(
            &MirasFamily {
                gender: "female".to_string(),
                husband: true,
                mother: true,
                sisters: 2,
                uterine_brothers: 1,
                uterine_sisters: 1,
                ..Default::default()
            },
            &estate("100000"),
            &rule,
        )
        .unwrap();
        assert_eq!(out["awl"], true);
        assert_eq!(
            miras_shares(&out),
            [
                ("husband", "3/10"),
                ("mother", "1/10"),
                ("uterine", "1/5"),
                ("fullSisters", "2/5"),
            ]
        );
        // daughter_only_radd
        let out = compute_miras(
            &MirasFamily {
                gender: "male".to_string(),
                daughters: 1,
                ..Default::default()
            },
            &estate("50000"),
            &rule,
        )
        .unwrap();
        assert_eq!(out["radd"], true);
        assert_eq!(miras_shares(&out), [("daughters", "1/1")]);
        // stopped: impediment_religion
        let stopped = compute_miras(
            &MirasFamily {
                gender: "female".to_string(),
                husband: true,
                ..Default::default()
            },
            &MirasEstate {
                religion_mixed: true,
                ..estate("60000")
            },
            &rule,
        );
        assert!(matches!(
            stopped,
            Err(MirasFail::Stopped("impediment_religion"))
        ));
    }

    #[test]
    fn miras_invalid_inputs() {
        let rule = miras_source_rule_data();
        let err = compute_miras(
            &MirasFamily {
                gender: "male".to_string(),
                wives: 5,
                ..Default::default()
            },
            &estate("1000"),
            &rule,
        );
        assert!(matches!(err, Err(MirasFail::Err(e)) if e.code == "too_many_wives"));
        let err = compute_miras(
            &MirasFamily {
                gender: "male".to_string(),
                sons: -1,
                ..Default::default()
            },
            &estate("1000"),
            &rule,
        );
        assert!(matches!(err, Err(MirasFail::Err(e)) if e.code == "invalid_family"));
    }
}
