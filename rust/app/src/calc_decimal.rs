//! Exact decimal + fraction arithmetic for the calculators module.
//!
//! The Python engines compute with `decimal.Decimal` (28-digit context
//! for division, `ROUND_HALF_UP` for quantize) and `fractions.Fraction`
//! (exact rationals). There is no decimal crate in `Cargo.toml` (and it
//! is off-limits for this wave), so this module implements the used
//! subset on `i128` mantissas — more than enough for money/weight
//! amounts — with Python's exact semantics:
//!
//! * `Dec` is `mantissa * 10^exponent`, preserving the input exponent,
//!   so `str()` reproduces Python's `str(Decimal)` (including trailing
//!   zeros and scientific notation for large/small exponents).
//! * Division reproduces the default decimal context: 28 significant
//!   digits, `ROUND_HALF_EVEN`, exact short results stay short.
//! * `quantize` uses `ROUND_HALF_UP` (ties away from zero), like the
//!   Python engines' `quantize(..., rounding=ROUND_HALF_UP)`.
//! * `Frac` is an exact rational, always printed as `n/d` (Python's
//!   `f"{n}/{d}"` form, including `0/1`).
//!
//! Float rounding note (ADR-024/ADR-040): this module never goes
//! through `f64`, so the Rust-vs-Python `round()` difference does not
//! apply here; the only rounding is the explicit Python-equivalent
//! `quantize` / 28-digit division above.

use axum::http::StatusCode;

/// A calculator error: an HTTP status (422 for a domain error, 500 for
/// a structural rule-data problem that Python would raise as an
/// unhandled exception) + a stable machine code + a Persian message.
#[derive(Debug, Clone)]
pub struct CalcError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl CalcError {
    /// A 422 domain error, like Python's `CalculatorError` → `APIError`.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code,
            message: message.into(),
        }
    }

    /// A 500, like a Python `KeyError`/`TypeError` escaping the engine.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: message.into(),
        }
    }

    pub fn to_api(&self) -> crate::errors::ApiError {
        crate::errors::ApiError::new(self.status, self.code, self.message.clone())
    }
}

/// Python `repr()` of a short string value (single quotes), used in
/// engine error messages to mirror the Python wording.
pub fn py_repr(text: &str) -> String {
    format!("'{text}'")
}

// ---------------------------------------------------------------------------
// Decimal
// ---------------------------------------------------------------------------

/// An exact decimal: `value = mantissa * 10^exponent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Dec {
    m: i128,
    e: i32,
}

fn pow10(exp: u32) -> Option<i128> {
    10i128.checked_pow(exp)
}

impl Dec {
    pub fn zero() -> Self {
        Self { m: 0, e: 0 }
    }

    pub fn from_int(n: i64) -> Self {
        Self { m: n as i128, e: 0 }
    }

    /// Build from an explicit mantissa/exponent pair.
    pub fn raw(m: i128, e: i32) -> Self {
        Self { m, e }
    }

    pub fn is_negative(&self) -> bool {
        self.m < 0
    }

    /// Parse a Python `Decimal` literal: optional sign, digits with an
    /// optional single `.`, optional `e`/`E` exponent. Underscores
    /// between digits are accepted, like Python's `Decimal`. `NaN` /
    /// `Infinity` are rejected (Pydantic rejects them too).
    pub fn parse(text: &str) -> Option<Self> {
        let t = text.trim().replace('_', "");
        if t.is_empty() {
            return None;
        }
        let lower = t.to_ascii_lowercase();
        if lower.contains("nan") || lower.contains("inf") {
            return None;
        }
        let (neg, rest) = match t.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, t.strip_prefix('+').unwrap_or(&t)),
        };
        let (mantissa_part, exp_part) = match rest.split_once(['e', 'E']) {
            Some((a, b)) => (a, Some(b)),
            None => (rest, None),
        };
        let mut exp: i32 = match exp_part {
            Some(s) => s.parse::<i32>().ok()?,
            None => 0,
        };
        let (int_part, frac_part) = match mantissa_part.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (mantissa_part, None),
        };
        if int_part.is_empty() && frac_part.is_none_or(str::is_empty) {
            return None;
        }
        if !int_part.chars().all(|c| c.is_ascii_digit())
            || !frac_part.is_none_or(|f| f.chars().all(|c| c.is_ascii_digit()))
        {
            return None;
        }
        let mut digits = format!("{int_part}{}", frac_part.unwrap_or(""));
        // Strip leading zeros (keep at least one digit).
        let stripped = digits.trim_start_matches('0');
        digits = if stripped.is_empty() {
            "0".to_string()
        } else {
            stripped.to_string()
        };
        if let Some(frac) = frac_part {
            exp = exp.checked_sub(frac.len() as i32)?;
        }
        let mut m: i128 = digits.parse().ok()?;
        if neg {
            m = -m;
        }
        if m == 0 {
            // Python keeps the exponent for zero (`Decimal("0.00")`
            // prints as `0.00`); keep it, but canonicalise `-0` to `0`.
            return Some(Self { m: 0, e: exp });
        }
        Some(Self { m, e: exp })
    }

    /// `str(Decimal)`: scientific notation when the exponent is
    /// positive or the adjusted exponent is below -6, plain otherwise.
    /// Trailing zeros of the mantissa are kept (they are significant
    /// in a `Decimal`).
    pub fn to_py_string(self) -> String {
        let sign = if self.m < 0 { "-" } else { "" };
        let digits = self.m.unsigned_abs().to_string();
        if self.m == 0 {
            if self.e < 0 {
                return format!("0.{}{}", "0".repeat((-self.e) as usize), "");
            }
            return "0".to_string();
        }
        // Adjusted exponent: exponent of the most significant digit.
        let adj = digits.len() as i32 - 1 + self.e;
        if self.e > 0 || adj < -6 {
            let mut coef = String::new();
            coef.push_str(&digits[..1]);
            if digits.len() > 1 {
                coef.push('.');
                coef.push_str(&digits[1..]);
            }
            let exp_sign = if adj >= 0 { "+" } else { "-" };
            return format!("{sign}{coef}E{exp_sign}{}", adj.abs());
        }
        if self.e >= 0 {
            return format!("{sign}{digits}{}", "0".repeat(self.e as usize));
        }
        // Negative exponent: insert the decimal point.
        let point = digits.len() as i32 + self.e;
        if point > 0 {
            let (a, b) = digits.split_at(point as usize);
            format!("{sign}{a}.{b}")
        } else {
            format!("{sign}0.{}{digits}", "0".repeat((-point) as usize))
        }
    }

    /// Plain notation with trailing zeros stripped (`_plain` in
    /// `zakat_engine.py`): `2.30` → `2.3`, `100` → `100`.
    pub fn plain(&self) -> String {
        let mut s = self.to_py_string();
        if s.contains('E') {
            // Expand scientific form, then strip.
            let d = Self::parse(&s).unwrap_or(*self);
            s = d.expand_plain();
        }
        if s.contains('.') {
            s = s.trim_end_matches('0').trim_end_matches('.').to_string();
        }
        if s.is_empty() || s == "-" {
            "0".to_string()
        } else {
            s
        }
    }

    /// Expand to plain notation without stripping zeros.
    fn expand_plain(&self) -> String {
        let sign = if self.m < 0 { "-" } else { "" };
        let digits = self.m.unsigned_abs().to_string();
        if self.m == 0 {
            if self.e < 0 {
                return format!("0.{}", "0".repeat((-self.e) as usize));
            }
            return "0".to_string();
        }
        if self.e >= 0 {
            return format!("{sign}{digits}{}", "0".repeat(self.e as usize));
        }
        let point = digits.len() as i32 + self.e;
        if point > 0 {
            let (a, b) = digits.split_at(point as usize);
            format!("{sign}{a}.{b}")
        } else {
            format!("{sign}0.{}{digits}", "0".repeat((-point) as usize))
        }
    }

    /// Round to the given exponent with `ROUND_HALF_UP` (ties away
    /// from zero), like the engines' `quantize`.
    pub fn quantize(&self, exp: i32) -> Result<Self, CalcError> {
        let overflow = || CalcError::new("bad_rule_number", "عدد خارج از بازهٔ محاسبه");
        if self.e == exp {
            return Ok(*self);
        }
        if self.e < exp {
            // Fewer fractional digits: divide, rounding half away
            // from zero (Python ROUND_HALF_UP).
            let drop = (exp - self.e) as u32;
            let div = pow10(drop).ok_or_else(overflow)?;
            let neg = self.m < 0;
            let a = self.m.unsigned_abs();
            let (mut q, r) = (a / div as u128, a % div as u128);
            // Half up: 2r >= div rounds away from zero.
            if 2 * r >= div as u128 {
                q += 1;
            }
            if q > i128::MAX as u128 {
                return Err(overflow());
            }
            let mut m = q as i128;
            if neg {
                m = -m;
            }
            return Ok(Self { m, e: exp });
        }
        // More fractional digits: scale the mantissa up.
        let raise = (self.e - exp) as u32;
        let mul = pow10(raise).ok_or_else(overflow)?;
        let m = self.m.checked_mul(mul).ok_or_else(overflow)?;
        Ok(Self { m, e: exp })
    }

    /// Quantize to `places` decimal places and print (`_money` with 4
    /// places, nisab prices with 6).
    pub fn fmt_fixed(&self, places: u32) -> Result<String, CalcError> {
        Ok(self.quantize(-(places as i32))?.to_py_string())
    }

    fn align(&self, other: &Self) -> Result<(i128, i128, i32), CalcError> {
        let overflow = || CalcError::new("bad_rule_number", "عدد خارج از بازهٔ محاسبه");
        let e = self.e.min(other.e);
        let m1 = self
            .m
            .checked_mul(pow10((self.e - e) as u32).ok_or_else(overflow)?)
            .ok_or_else(overflow)?;
        let m2 = other
            .m
            .checked_mul(pow10((other.e - e) as u32).ok_or_else(overflow)?)
            .ok_or_else(overflow)?;
        Ok((m1, m2, e))
    }

    pub fn add(&self, other: &Self) -> Result<Self, CalcError> {
        let (m1, m2, e) = self.align(other)?;
        let m = m1
            .checked_add(m2)
            .ok_or_else(|| CalcError::new("bad_rule_number", "عدد خارج از بازهٔ محاسبه"))?;
        Ok(Self { m, e })
    }

    pub fn sub(&self, other: &Self) -> Result<Self, CalcError> {
        let (m1, m2, e) = self.align(other)?;
        let m = m1
            .checked_sub(m2)
            .ok_or_else(|| CalcError::new("bad_rule_number", "عدد خارج از بازهٔ محاسبه"))?;
        Ok(Self { m, e })
    }

    pub fn mul(&self, other: &Self) -> Result<Self, CalcError> {
        let m = self
            .m
            .checked_mul(other.m)
            .ok_or_else(|| CalcError::new("bad_rule_number", "عدد خارج از بازهٔ محاسبه"))?;
        let e = self
            .e
            .checked_add(other.e)
            .ok_or_else(|| CalcError::new("bad_rule_number", "عدد خارج از بازهٔ محاسبه"))?;
        Ok(Self { m, e })
    }

    fn cmp_mag(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        if self.m == 0 && other.m == 0 {
            return Ordering::Equal;
        }
        if self.m == 0 {
            return Ordering::Less;
        }
        if other.m == 0 {
            return Ordering::Greater;
        }
        let d1 = self.m.unsigned_abs().to_string();
        let d2 = other.m.unsigned_abs().to_string();
        // Number of integer digits (may be <= 0).
        let p1 = d1.len() as i32 + self.e;
        let p2 = d2.len() as i32 + other.e;
        if p1 != p2 {
            return p1.cmp(&p2);
        }
        // Same point position: compare digit by digit, padding with
        // trailing zeros (i.e. compare the integer parts first, then
        // the fractional digits).
        let len = d1.len().max(d2.len());
        for i in 0..len {
            let c1 = d1.as_bytes().get(i).copied().unwrap_or(b'0');
            let c2 = d2.as_bytes().get(i).copied().unwrap_or(b'0');
            if c1 != c2 {
                return c1.cmp(&c2);
            }
        }
        Ordering::Equal
    }

    /// Division with the default decimal context: 28 significant
    /// digits, `ROUND_HALF_EVEN`; exact short results stay short.
    ///
    /// Algorithm: long division digit by digit (integer part, then
    /// fractional digits) until the result is exact or 29 significant
    /// digits are known; then round to 28 significant digits with the
    /// 29th digit and the sticky bit. The value is
    /// `S * 10^(e1 - e2 - f + k)` where `S` is the rounded significant
    /// digit integer, `f` the number of fractional digits generated,
    /// and `k` the number of dropped significant digits (plus one
    /// more when the rounding carry grows the digit count).
    pub fn div28(&self, other: &Self) -> Result<Self, CalcError> {
        if other.m == 0 {
            return Err(CalcError::internal("تقسیم بر صفر در محاسبه"));
        }
        if self.m == 0 {
            return Ok(Self::zero());
        }
        let neg = (self.m < 0) != (other.m < 0);
        let num = self.m.unsigned_abs();
        let den = other.m.unsigned_abs();
        // Integer part digits (no leading zeros; empty when zero).
        let mut int_part = num / den;
        let mut rem = num % den;
        let mut digits: Vec<u8> = Vec::new();
        if int_part > 0 {
            let mut stack = Vec::new();
            while int_part > 0 {
                stack.push((int_part % 10) as u8);
                int_part /= 10;
            }
            while let Some(d) = stack.pop() {
                digits.push(d);
            }
        }
        let int_len = digits.len();
        // Significant digits so far (leading fractional zeros before
        // the first nonzero digit are not significant).
        let mut sig = int_len;
        let mut frac_len = 0usize;
        while rem != 0 && sig < 29 {
            rem *= 10;
            let d = (rem / den) as u8;
            rem %= den;
            if sig == 0 && d == 0 {
                // Leading fractional zero: not significant.
            } else {
                sig += 1;
            }
            digits.push(d);
            frac_len += 1;
        }
        let sticky = rem != 0;
        // First nonzero digit (the value is nonzero, so one exists).
        let first = digits.iter().position(|&d| d != 0).unwrap_or(digits.len());
        let sig_len = digits.len() - first;
        let mut exp = self.e - other.e - frac_len as i32;
        let mut sig_digits: Vec<u8> = digits[first..].to_vec();
        if sig_len > 28 {
            let last = sig_digits[28];
            let rest_has_nonzero = sig_digits[29..].iter().any(|&d| d != 0);
            let mut rest: Vec<u8> = sig_digits[..28].to_vec();
            let round_up =
                last > 5 || (last == 5 && (sticky || rest_has_nonzero || rest[27] % 2 == 1));
            let mut k = (sig_len - 28) as i32;
            if round_up {
                let mut i = 28;
                loop {
                    if i == 0 {
                        rest.insert(0, 1);
                        k += 1;
                        break;
                    }
                    i -= 1;
                    if rest[i] < 9 {
                        rest[i] += 1;
                        break;
                    }
                    rest[i] = 0;
                }
                if rest.len() == 29 {
                    rest.pop();
                }
            }
            sig_digits = rest;
            exp += k;
        }
        let mut m: i128 = 0;
        for &d in &sig_digits {
            m = m * 10 + d as i128;
        }
        if neg {
            m = -m;
        }
        Ok(Self { m, e: exp })
    }

    /// `Decimal.__floordiv__`: floor of the quotient, as an integer.
    pub fn floor_div(&self, other: &Self) -> Result<Self, CalcError> {
        if other.m == 0 {
            return Err(CalcError::internal("تقسیم بر صفر در محاسبه"));
        }
        let overflow = || CalcError::new("bad_rule_number", "عدد خارج از بازهٔ محاسبه");
        let (m1, m2, _) = self.align(other)?;
        let neg = (m1 < 0) != (m2 < 0);
        let (a, b) = (m1.unsigned_abs(), m2.unsigned_abs());
        let mut q = (a / b) as i128;
        if neg {
            q = -q;
            if a % b != 0 {
                q = q.checked_sub(1).ok_or_else(overflow)?;
            }
        }
        Ok(Self { m: q, e: 0 })
    }
}

impl PartialOrd for Dec {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Dec {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self.is_negative(), other.is_negative()) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let mag = self.cmp_mag(other);
        if self.is_negative() {
            mag.reverse()
        } else {
            mag
        }
    }
}

/// Parse a JSON value as a `Decimal` the way Pydantic does: a JSON
/// number (exact literal) or a numeric string. Bools and null are
/// rejected.
pub fn dec_from_json(v: &serde_json::Value) -> Option<Dec> {
    match v {
        serde_json::Value::Number(n) => Dec::parse(&n.to_string()),
        serde_json::Value::String(s) => Dec::parse(s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Fraction
// ---------------------------------------------------------------------------

fn gcd_u128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// An exact rational, always printed as `n/d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Frac {
    pub n: i128,
    pub d: i128, // always > 0, gcd(|n|, d) == 1
}

impl Frac {
    pub fn new(n: i128, d: i128) -> Result<Self, CalcError> {
        if d == 0 {
            return Err(CalcError::new("bad_rule_fraction", "مخرج کسر صفر است"));
        }
        let (mut n, mut d) = (n, d);
        if d < 0 {
            n = -n;
            d = -d;
        }
        if n == 0 {
            return Ok(Self { n: 0, d: 1 });
        }
        let g = gcd_u128(n.unsigned_abs(), d as u128) as i128;
        Ok(Self { n: n / g, d: d / g })
    }

    pub fn zero() -> Self {
        Self { n: 0, d: 1 }
    }

    pub fn one() -> Self {
        Self { n: 1, d: 1 }
    }

    pub fn from_int(n: i64) -> Self {
        Self { n: n as i128, d: 1 }
    }

    /// Parse `"a/b"` or `"a"` (Python `Fraction(str)`).
    pub fn parse(text: &str) -> Result<Self, CalcError> {
        let t = text.trim();
        if let Some((a, b)) = t.split_once('/') {
            let n: i128 = a.trim().parse().map_err(|_| {
                CalcError::new(
                    "bad_rule_fraction",
                    format!("کسر نامعتبر: {}", py_repr(text)),
                )
            })?;
            let d: i128 = b.trim().parse().map_err(|_| {
                CalcError::new(
                    "bad_rule_fraction",
                    format!("کسر نامعتبر: {}", py_repr(text)),
                )
            })?;
            Self::new(n, d)
        } else {
            let n: i128 = t.parse().map_err(|_| {
                CalcError::new(
                    "bad_rule_fraction",
                    format!("کسر نامعتبر: {}", py_repr(text)),
                )
            })?;
            Self::new(n, 1)
        }
    }

    pub fn is_zero(&self) -> bool {
        self.n == 0
    }

    pub fn to_py_string(self) -> String {
        format!("{}/{}", self.n, self.d)
    }

    fn checked(v: Option<i128>) -> Result<i128, CalcError> {
        v.ok_or_else(|| CalcError::new("bad_rule_fraction", "کسر خارج از بازهٔ محاسبه"))
    }

    pub fn add(&self, other: &Self) -> Result<Self, CalcError> {
        let n = Self::checked(
            self.n
                .checked_mul(other.d)
                .and_then(|a| other.n.checked_mul(self.d).and_then(|b| a.checked_add(b))),
        )?;
        let d = Self::checked(self.d.checked_mul(other.d))?;
        Self::new(n, d)
    }

    pub fn sub(&self, other: &Self) -> Result<Self, CalcError> {
        let n = Self::checked(
            self.n
                .checked_mul(other.d)
                .and_then(|a| other.n.checked_mul(self.d).and_then(|b| a.checked_sub(b))),
        )?;
        let d = Self::checked(self.d.checked_mul(other.d))?;
        Self::new(n, d)
    }

    pub fn mul(&self, other: &Self) -> Result<Self, CalcError> {
        let n = Self::checked(self.n.checked_mul(other.n))?;
        let d = Self::checked(self.d.checked_mul(other.d))?;
        Self::new(n, d)
    }

    pub fn div(&self, other: &Self) -> Result<Self, CalcError> {
        if other.n == 0 {
            return Err(CalcError::new("bad_rule_fraction", "تقسیم کسر بر صفر"));
        }
        let n = Self::checked(self.n.checked_mul(other.d))?;
        let d = Self::checked(self.d.checked_mul(other.n))?;
        Self::new(n, d)
    }

    pub fn mul_int(&self, k: i64) -> Result<Self, CalcError> {
        let n = Self::checked(self.n.checked_mul(k as i128))?;
        Self::new(n, self.d)
    }
}

impl PartialOrd for Frac {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Frac {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let a = self.n.checked_mul(other.d);
        let b = other.n.checked_mul(self.d);
        match (a, b) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => {
                // Fallback for huge values: compare via f64 sign only
                // (unreachable for real rule fractions).
                let fa = self.n as f64 / self.d as f64;
                let fb = other.n as f64 / other.d as f64;
                fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dec_str_roundtrip() {
        for s in [
            "0",
            "1200",
            "1200.0000",
            "60.50",
            "0.5522727272",
            "2.30",
            "10.4545454545",
            "1E+2",
            "-0.5",
            "100000",
        ] {
            let d = Dec::parse(s).unwrap();
            // Python str() of the same literal, except scientific
            // inputs normalise the same way.
            assert_eq!(d.to_py_string(), s, "{s}");
        }
        assert_eq!(Dec::parse("0.00").unwrap().to_py_string(), "0.00");
        assert_eq!(Dec::parse("1.20E+3").unwrap().to_py_string(), "1.20E+3");
        assert_eq!(Dec::parse("0.0000001").unwrap().to_py_string(), "1E-7");
    }

    #[test]
    fn dec_quantize_half_up() {
        assert_eq!(
            Dec::parse("2.5")
                .unwrap()
                .quantize(0)
                .unwrap()
                .to_py_string(),
            "3"
        );
        assert_eq!(
            Dec::parse("-2.5")
                .unwrap()
                .quantize(0)
                .unwrap()
                .to_py_string(),
            "-3"
        );
        assert_eq!(
            Dec::parse("16.1")
                .unwrap()
                .quantize(-2)
                .unwrap()
                .to_py_string(),
            "16.10"
        );
        assert_eq!(
            Dec::parse("643.95")
                .unwrap()
                .quantize(-1)
                .unwrap()
                .to_py_string(),
            "644.0"
        );
    }

    #[test]
    fn dec_div28_matches_python() {
        // Decimal(1)/Decimal(3) = 0.333... (28 threes)
        let q = Dec::from_int(1).div28(&Dec::from_int(3)).unwrap();
        assert_eq!(q.to_py_string(), "0.3333333333333333333333333333");
        // Decimal(200000000)/Decimal(10000000) = 20 exactly
        let q = Dec::parse("200000000")
            .unwrap()
            .div28(&Dec::parse("10000000").unwrap())
            .unwrap();
        assert_eq!(q.to_py_string(), "20");
        // Decimal(100)/Decimal(3) → 33.333... (28 sig digits)
        let q = Dec::from_int(100).div28(&Dec::from_int(3)).unwrap();
        assert_eq!(q.to_py_string(), "33.33333333333333333333333333");
        // Half-even: 1/6 = 0.1666...67 (28 sig digits)
        let q = Dec::from_int(1).div28(&Dec::from_int(6)).unwrap();
        assert_eq!(q.to_py_string(), "0.1666666666666666666666666667");
    }

    #[test]
    fn dec_floor_div() {
        let a = Dec::parse("7.5").unwrap();
        let b = Dec::from_int(2);
        assert_eq!(a.floor_div(&b).unwrap().to_py_string(), "3");
        assert_eq!(
            Dec::parse("-7.5")
                .unwrap()
                .floor_div(&b)
                .unwrap()
                .to_py_string(),
            "-4"
        );
    }

    #[test]
    fn frac_arith() {
        let a = Frac::parse("1/2").unwrap();
        let b = Frac::parse("1/3").unwrap();
        assert_eq!(a.add(&b).unwrap().to_py_string(), "5/6");
        assert_eq!(Frac::parse("2/4").unwrap().to_py_string(), "1/2");
        assert_eq!(Frac::zero().to_py_string(), "0/1");
        assert!(Frac::parse("1/6").unwrap() < Frac::parse("1/4").unwrap());
    }
}
