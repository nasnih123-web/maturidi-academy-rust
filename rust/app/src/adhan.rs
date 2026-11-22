//! Prayer times — a direct port of `app/modules/quran/adhan.py`
//! (hand-rolled PrayTimes), `jalali.py` (Birashk algorithm) and the
//! Ramadan / Torbat Jam table parts of `adhan_data.py`.
//!
//! This is astronomy, not a fiqh rule: angles and methods are copied
//! verbatim from the Python module. Times come back as `"HH:MM"` local
//! strings for a requested UTC offset (`tz`).

use chrono::{Datelike, NaiveDate};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::quran_data::{adhan_city, adhan_method};

const D2R: f64 = std::f64::consts::PI / 180.0;
const R2D: f64 = 180.0 / std::f64::consts::PI;
const IMSAK_OFFSET_MIN: i64 = 10;

fn fix_angle(a: f64) -> f64 {
    a - 360.0 * (a / 360.0).floor()
}

fn fix_hour(a: f64) -> f64 {
    a - 24.0 * (a / 24.0).floor()
}

fn julian(year: i64, month: i64, day: i64) -> f64 {
    let (mut year, mut month) = (year, month);
    if month <= 2 {
        year -= 1;
        month += 12;
    }
    let a = year.div_euclid(100);
    let b = 2 - a + a.div_euclid(4);
    (365.25 * (year + 4716) as f64).floor()
        + (30.6001 * (month + 1) as f64).floor()
        + day as f64
        + b as f64
        - 1524.5
}

/// (sun declination in degrees, equation of time in hours).
fn sun_position(jd: f64) -> (f64, f64) {
    let d = jd - 2451545.0;
    let g = fix_angle(357.529 + 0.98560028 * d);
    let q = fix_angle(280.459 + 0.98564736 * d);
    let lng = fix_angle(q + 1.915 * (g * D2R).sin() + 0.020 * (2.0 * g * D2R).sin());
    let e = 23.439 - 0.00000036 * d;
    let ra = fix_hour(((e * D2R).cos() * (lng * D2R).sin()).atan2((lng * D2R).cos()) * R2D / 15.0);
    let decl = ((e * D2R).sin() * (lng * D2R).sin()).asin() * R2D;
    let eqt = q / 15.0 - ra;
    (decl, eqt)
}

struct Calculator {
    lat: f64,
    lng: f64,
    tz: f64,
    fajr: f64,
    isha: Option<f64>,
    maghrib: Option<f64>,
    isha_minutes: Option<i64>,
    isha_minutes_ramadan: Option<i64>,
    asr_factor: f64,
    ramadan: bool,
}

impl Calculator {
    fn mid_day(&self, time: f64, jdn: f64) -> f64 {
        let (_, eqt) = sun_position(jdn + time);
        fix_hour(12.0 - eqt)
    }

    fn sun_angle_time(&self, angle: f64, time: f64, jdn: f64, direction: &str) -> f64 {
        let (decl, _) = sun_position(jdn + time);
        let noon = self.mid_day(time, jdn);
        let mut lat_eff = self.lat;
        let mut cos_t = 0.0f64;
        let mut clamped = false;
        for _ in 0..80 {
            let lat_r = lat_eff * D2R;
            let decl_r = decl * D2R;
            cos_t =
                (-(angle * D2R).sin() - lat_r.sin() * decl_r.sin()) / (lat_r.cos() * decl_r.cos());
            if (-1.0..=1.0).contains(&cos_t) {
                break;
            }
            if lat_eff.abs() < 1.0 {
                cos_t = cos_t.clamp(-1.0, 1.0);
                clamped = true;
                break;
            }
            lat_eff *= 0.97;
        }
        if !clamped && !(-1.0..=1.0).contains(&cos_t) {
            // Python `for...else`: the loop ran out without a break.
            cos_t = cos_t.clamp(-1.0, 1.0);
        }
        let t = (1.0 / 15.0) * cos_t.acos() * R2D;
        noon + if direction == "ccw" { -t } else { t }
    }

    fn asr_time(&self, time: f64, jdn: f64) -> f64 {
        let (decl, _) = sun_position(jdn + time);
        let angle = -(1.0 / (self.asr_factor + (self.lat - decl).abs().to_radians().tan()))
            .atan()
            .to_degrees();
        self.sun_angle_time(angle, time, jdn, "cw")
    }

    /// Times as fractions of the day (two refinement iterations).
    fn times_frac(&self, day: NaiveDate) -> HashMap<&'static str, f64> {
        let jdn =
            julian(day.year() as i64, day.month() as i64, day.day() as i64) - self.lng / 360.0;
        let mut t: HashMap<&'static str, f64> = [
            ("fajr", 5.0 / 24.0),
            ("sunrise", 6.0 / 24.0),
            ("dhuhr", 12.0 / 24.0),
            ("asr", 13.0 / 24.0),
            ("sunset", 18.0 / 24.0),
            ("maghrib", 18.0 / 24.0),
            ("isha", 18.0 / 24.0),
        ]
        .into_iter()
        .collect();
        for _ in 0..2 {
            let mut n: HashMap<&'static str, f64> = HashMap::new();
            n.insert(
                "fajr",
                self.sun_angle_time(self.fajr, t["fajr"], jdn, "ccw") / 24.0,
            );
            n.insert(
                "sunrise",
                self.sun_angle_time(0.833, t["sunrise"], jdn, "ccw") / 24.0,
            );
            n.insert("dhuhr", self.mid_day(t["dhuhr"], jdn) / 24.0);
            n.insert("asr", self.asr_time(t["asr"], jdn) / 24.0);
            n.insert(
                "sunset",
                self.sun_angle_time(0.833, t["sunset"], jdn, "cw") / 24.0,
            );
            if let Some(mag) = self.maghrib {
                n.insert(
                    "maghrib",
                    self.sun_angle_time(mag, t["maghrib"], jdn, "cw") / 24.0,
                );
            } else {
                n.insert("maghrib", n["sunset"]);
            }
            if let Some(minutes) = self.isha_minutes {
                let m = if self.ramadan {
                    self.isha_minutes_ramadan.unwrap_or(minutes)
                } else {
                    minutes
                };
                n.insert("isha", n["maghrib"] + (m as f64 / 60.0) / 24.0);
            } else {
                n.insert(
                    "isha",
                    self.sun_angle_time(self.isha.unwrap_or(15.0), t["isha"], jdn, "cw") / 24.0,
                );
            }
            t = n;
        }
        t
    }

    /// Times in hours (local, after the `tz - lng/15` offset).
    fn compute(&self, day: NaiveDate) -> HashMap<&'static str, f64> {
        let frac = self.times_frac(day);
        let mut t: HashMap<&'static str, f64> = frac.iter().map(|(k, v)| (*k, v * 24.0)).collect();
        let offset = self.tz - self.lng / 15.0;
        for v in t.values_mut() {
            *v = fix_hour(*v + offset);
        }
        let next = day + chrono::Duration::days(1);
        let fajr_next = fix_hour(self.times_frac(next)["fajr"] * 24.0 + offset);
        let end = if fajr_next < t["sunset"] {
            fajr_next + 24.0
        } else {
            fajr_next
        };
        t.insert("midnight", fix_hour((t["sunset"] + end) / 2.0));
        t.insert(
            "imsak",
            fix_hour(t["fajr"] - IMSAK_OFFSET_MIN as f64 / 60.0),
        );
        t
    }
}

/// Python `round()` (banker's rounding) for a non-negative float.
fn py_round(x: f64) -> f64 {
    let f = x.floor();
    let frac = x - f;
    if frac < 0.5 {
        f
    } else if frac > 0.5 {
        f + 1.0
    } else if (f as i64) % 2 == 0 {
        f
    } else {
        f + 1.0
    }
}

fn fmt(hours: f64) -> String {
    let total = (py_round(fix_hour(hours) * 60.0) as i64).rem_euclid(24 * 60);
    format!("{:02}:{:02}", total / 60, total % 60)
}

fn fmt_minutes(total: i64) -> String {
    let total = total.rem_euclid(24 * 60);
    format!("{:02}:{:02}", total / 60, total % 60)
}

fn to_minutes(hhmm: &str) -> i64 {
    let (h, m) = hhmm.split_once(':').unwrap_or(("0", "0"));
    h.parse::<i64>().unwrap_or(0) * 60 + m.parse::<i64>().unwrap_or(0)
}

/// Prayer times for one day — `{"imsak","fajr","sunrise","dhuhr","asr",
/// "maghrib","isha","midnight"}` as `"HH:MM"`. `None` for `ramadan`
/// means auto-detect (Kuwaiti tabular calendar). Unknown method/madhab
/// → `Err(())` (the router maps it to 400 `invalid_adhan_params`,
/// like Python's `ValueError` catch).
pub fn prayer_times(
    day: NaiveDate,
    lat: f64,
    lng: f64,
    tz: f64,
    method: &str,
    madhab: &str,
    ramadan: Option<bool>,
) -> Result<HashMap<&'static str, String>, ()> {
    let def = adhan_method(method).ok_or(())?;
    let factor = match madhab {
        "shafi" => 1.0,
        "hanafi" => 2.0,
        _ => return Err(()),
    };
    let ramadan = ramadan.unwrap_or_else(|| is_ramadan(day));
    let calc = Calculator {
        lat,
        lng,
        tz,
        fajr: def.fajr.unwrap_or(15.0),
        isha: def.isha,
        maghrib: def.maghrib,
        isha_minutes: def.isha_minutes,
        isha_minutes_ramadan: def.isha_minutes_ramadan,
        asr_factor: factor,
        ramadan,
    };
    let raw = calc.compute(day);
    let order = [
        "imsak", "fajr", "sunrise", "dhuhr", "asr", "maghrib", "isha", "midnight",
    ];
    Ok(order.iter().map(|k| (*k, fmt(raw[k]))).collect())
}

// --- Jalali calendar (Birashk, same logic as jalaali-js) ---------------

const BREAKS: &[i64] = &[
    -61, 9, 38, 199, 426, 686, 756, 818, 1111, 1181, 1210, 1635, 2060, 2097, 2192, 2262, 2322,
    2394, 2456, 3178,
];

struct JalCal {
    leap: i64,
    gy: i64,
    march: i64,
}

fn jal_cal(jy: i64) -> JalCal {
    let gy = jy + 621;
    let mut leap_j: i64 = -14;
    let mut jp = BREAKS[0];
    let mut jump: i64 = 0;
    for &jm in BREAKS.iter().skip(1) {
        jump = jm - jp;
        if jy < jm {
            break;
        }
        leap_j += (jump / 33) * 8 + ((jump % 33) / 4);
        jp = jm;
    }
    let mut n = jy - jp;
    leap_j += (n / 33) * 8 + (((n % 33) + 3) / 4);
    if jump % 33 == 4 && jump - n == 4 {
        leap_j += 1;
    }
    let leap_g = gy / 4 - ((gy / 100 + 1) * 3 / 4) - 150;
    let march = 20 + leap_j - leap_g;
    if jump - n < 6 {
        n = n - jump + ((jump + 4) / 33) * 33;
    }
    let mut leap = (((n + 1) % 33) - 1) % 4;
    if leap == 4 {
        leap = 0;
    }
    JalCal { leap, gy, march }
}

fn gregorian_to_jdn(y: i64, m: i64, d: i64) -> i64 {
    let a = (14 - m) / 12;
    let yy = y + 4800 - a;
    let mm = m + 12 * a - 3;
    d + ((153 * mm + 2) / 5) + 365 * yy + yy / 4 - yy / 100 + yy / 400 - 32045
}

/// Gregorian → (Jalali year, month, day).
pub fn to_jalali(g: NaiveDate) -> (i64, i64, i64) {
    let mut jy = g.year() as i64 - 621;
    let r = jal_cal(jy);
    let jdn = gregorian_to_jdn(g.year() as i64, g.month() as i64, g.day() as i64);
    let jdn1f = gregorian_to_jdn(r.gy, 3, r.march);
    let mut k = jdn - jdn1f;
    if k >= 0 {
        if k <= 185 {
            return (jy, 1 + k / 31, k % 31 + 1);
        }
        k -= 186;
    } else {
        jy -= 1;
        k += 179;
        if r.leap == 1 {
            k += 1;
        }
    }
    (jy, 7 + k / 30, k % 30 + 1)
}

// --- Kuwaiti tabular Islamic calendar (Ramadan detection) -------------

fn islamic_to_jd(y: i64, m: i64, d: i64) -> f64 {
    d as f64
        + (29.5 * (m - 1) as f64).ceil()
        + (y - 1) as f64 * 354.0
        + ((3 + 11 * y) as f64 / 30.0).floor()
        + 1948439.5
        - 1.0
}

fn jd_to_islamic(jd: f64) -> (i64, i64, i64) {
    let jd = jd.floor() + 0.5;
    let y = ((30.0 * (jd - 1948439.5) + 10646.0) / 10631.0).floor() as i64;
    let m = (12i64).min((((jd - (29.0 + islamic_to_jd(y, 1, 1))) / 29.5).ceil() as i64) + 1);
    let d = (jd - islamic_to_jd(y, m, 1)) as i64 + 1;
    (y, m, d)
}

fn gregorian_to_jd(g: NaiveDate) -> f64 {
    let a = (14 - g.month() as i64) / 12;
    let y = g.year() as i64 + 4800 - a;
    let m = g.month() as i64 + 12 * a - 3;
    g.day() as f64 + ((153 * m + 2) / 5) as f64 + 365.0 * y as f64 + (y / 4) as f64
        - (y / 100) as f64
        + (y / 400) as f64
        - 32045.0
        - 0.5
}

pub fn is_ramadan(g: NaiveDate) -> bool {
    jd_to_islamic(gregorian_to_jd(g)).1 == 9
}

// --- Torbat Jam table ----------------------------------------------------

fn haversine_km(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlmb = (lng2 - lng1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlmb / 2.0).sin().powi(2);
    2.0 * 6371.0 * a.sqrt().asin()
}

pub fn near_torbat(lat: f64, lng: f64) -> bool {
    let c = adhan_city("torbat-e-jam").unwrap();
    haversine_km(lat, lng, c.0, c.1) <= 35.0
}

fn torbat_table() -> Option<&'static HashMap<String, [String; 6]>> {
    static TABLE: OnceLock<Option<HashMap<String, [String; 6]>>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            let raw = include_str!("../data/quran/torbat_jam_table.json");
            let parsed: Value = serde_json::from_str(raw).ok()?;
            let obj = parsed.as_object()?;
            if obj.len() != 365 || obj.contains_key("12-30") {
                return None;
            }
            let mut out = HashMap::new();
            for (k, v) in obj {
                let arr = v.as_array()?;
                if k.len() != 5 || arr.len() != 6 {
                    return None;
                }
                let mut row = [
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                ];
                for (i, item) in arr.iter().enumerate() {
                    let s = item.as_str()?;
                    let (hh, mm) = s.split_once(':')?;
                    let h: i64 = hh.parse().ok()?;
                    let m: i64 = mm.parse().ok()?;
                    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
                        return None;
                    }
                    row[i] = s.to_string();
                }
                out.insert(k.clone(), row);
            }
            Some(out)
        })
        .as_ref()
}

fn torbat_row(mm_dd: &str) -> Option<&'static [String; 6]> {
    torbat_table()?.get(mm_dd)
}

/// One day from the Torbat Jam table: (times, per-prayer sources, source).
pub type TableDayTimes = (
    HashMap<&'static str, String>,
    HashMap<&'static str, &'static str>,
    &'static str,
);

/// One day from the Torbat Jam table, or `None` when no row exists
/// (e.g. 12-30). Returns (times, sources, source) like Python's
/// `_table_day_times`.
pub fn table_day_times(day: NaiveDate, madhab: &str, tz: f64) -> Option<TableDayTimes> {
    let (_, jm, jd) = to_jalali(day);
    let row = torbat_row(&format!("{jm:02}-{jd:02}"))?;
    let (fajr, sunrise, dhuhr, asr_table, maghrib, isha) = (
        row[0].clone(),
        row[1].clone(),
        row[2].clone(),
        row[3].clone(),
        row[4].clone(),
        row[5].clone(),
    );
    let city = adhan_city("torbat-e-jam").unwrap();
    let (asr, asr_source): (String, &'static str) = if madhab == "hanafi" {
        (asr_table, "table")
    } else {
        let comp = prayer_times(day, city.0, city.1, tz, "council", "shafi", None).ok()?;
        (comp["asr"].clone(), "computed")
    };
    let next = day + chrono::Duration::days(1);
    let (_, njm, njd) = to_jalali(next);
    let next_fajr = torbat_row(&format!("{njm:02}-{njd:02}"))
        .map(|r| to_minutes(&r[0]))
        .unwrap_or_else(|| to_minutes(&fajr));
    let maghrib_min = to_minutes(&maghrib);
    let end = if next_fajr < maghrib_min {
        next_fajr + 24 * 60
    } else {
        next_fajr
    };
    let mut times: HashMap<&'static str, String> = HashMap::new();
    times.insert("imsak", fmt_minutes(to_minutes(&fajr) - 10));
    times.insert("fajr", fajr);
    times.insert("sunrise", sunrise);
    times.insert("dhuhr", dhuhr);
    times.insert("asr", asr);
    times.insert("maghrib", maghrib);
    times.insert("isha", isha);
    times.insert("midnight", fmt_minutes((maghrib_min + end) / 2));
    let mut sources: HashMap<&'static str, &'static str> = HashMap::new();
    sources.insert("fajr", "table");
    sources.insert("sunrise", "table");
    sources.insert("dhuhr", "table");
    sources.insert("asr", asr_source);
    sources.insert("maghrib", "table");
    sources.insert("isha", "table");
    sources.insert("imsak", "derived");
    sources.insert("midnight", "derived");
    let source = if asr_source == "table" {
        "table"
    } else {
        "mixed"
    };
    Some((times, sources, source))
}
