//! Fixed-point prices.
//!
//! Every price in the book is an integer number of ticks. Exchange decimal
//! strings are parsed straight into ticks without going through `f64`, so
//! `"82996.3"` and `"82996.30"` always land on the same level.

use std::fmt::Write as _;

/// Price expressed as a whole number of ticks.
pub type Ticks = i64;

/// Describes how a venue's decimal prices map onto integer ticks.
///
/// A tick size like `"0.50"` normalises to `exp = 1`, `tick_m = 5`: prices are
/// first scaled to `exp` decimals, then divided by `tick_m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceScale {
    exp: u32,
    tick_m: i64,
}

impl PriceScale {
    /// Builds a scale from a tick size string such as `"0.10"`.
    pub fn from_tick(tick: &str) -> Option<Self> {
        let tick = tick.trim();
        let (int, frac) = tick.split_once('.').unwrap_or((tick, ""));
        let frac = frac.trim_end_matches('0');
        if frac.len() > 12 || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
            return None;
        }
        let exp = frac.len() as u32;
        let mut m: i64 = 0;
        for b in int.bytes().chain(frac.bytes()) {
            m = m.checked_mul(10)?.checked_add((b - b'0') as i64)?;
        }
        (m > 0).then_some(Self { exp, tick_m: m })
    }

    /// Number of decimals needed to print any tick-aligned price.
    pub fn decimals(&self) -> u32 {
        self.exp
    }

    /// Parses a decimal string into ticks, rounding half up to the nearest tick.
    #[inline]
    pub fn parse(&self, s: &str) -> Option<Ticks> {
        let b = s.as_bytes();
        let mut i = 0;
        let mut m: i64 = 0;
        let mut any = false;
        while i < b.len() && b[i] != b'.' {
            let d = b[i].wrapping_sub(b'0');
            if d > 9 {
                return None;
            }
            m = m.checked_mul(10)?.checked_add(d as i64)?;
            any = true;
            i += 1;
        }
        let mut frac = 0;
        let mut round_up = false;
        if i < b.len() {
            i += 1; // '.'
            while i < b.len() {
                let d = b[i].wrapping_sub(b'0');
                if d > 9 {
                    return None;
                }
                if frac < self.exp {
                    m = m.checked_mul(10)?.checked_add(d as i64)?;
                    frac += 1;
                } else if frac == self.exp {
                    round_up = d >= 5;
                    frac += 1;
                }
                any = true;
                i += 1;
            }
        }
        if !any {
            return None;
        }
        while frac < self.exp {
            m = m.checked_mul(10)?;
            frac += 1;
        }
        if round_up {
            m += 1;
        }
        Some((m + self.tick_m / 2) / self.tick_m)
    }

    /// Converts ticks back to a float, for display maths only.
    pub fn to_f64(&self, t: Ticks) -> f64 {
        (t * self.tick_m) as f64 / 10f64.powi(self.exp as i32)
    }

    /// Appends the decimal form of `t` to `out`.
    pub fn write(&self, t: Ticks, out: &mut String) {
        let m = t * self.tick_m;
        if self.exp == 0 {
            let _ = write!(out, "{m}");
            return;
        }
        let p = 10i64.pow(self.exp);
        let sign = if m < 0 { "-" } else { "" };
        let m = m.abs();
        let _ = write!(out, "{sign}{}.{:0w$}", m / p, m % p, w = self.exp as usize);
    }

    /// Returns the decimal form of `t` as a new string.
    pub fn format(&self, t: Ticks) -> String {
        let mut s = String::with_capacity(16);
        self.write(t, &mut s);
        s
    }
}

/// Parses a quantity string. Quantities are only displayed and summed, so
/// `f64` is precise enough.
#[inline]
pub fn parse_qty(s: &str) -> Option<f64> {
    s.parse().ok()
}

/// A short decimal string held inline, for values like the mark price that
/// carry more precision than the tick size.
#[derive(Clone, Copy, Default)]
pub struct InlineStr {
    len: u8,
    buf: [u8; 31],
}

impl InlineStr {
    /// Stores `s` cut to at most `min_decimals + 2` decimals, dropping
    /// trailing fractional zeros beyond `min_decimals`.
    pub fn set_decimal(&mut self, s: &str, min_decimals: u32) {
        let mut end = s.len();
        if let Some(dot) = s.find('.') {
            end = end.min(dot + 3 + min_decimals as usize);
            let keep = dot + 1 + min_decimals as usize;
            while end > keep && s.as_bytes()[end - 1] == b'0' {
                end -= 1;
            }
            if end == dot + 1 {
                end = dot;
            }
        }
        let n = end.min(self.buf.len());
        self.buf[..n].copy_from_slice(&s.as_bytes()[..n]);
        self.len = n as u8;
    }

    pub fn as_str(&self) -> &str {
        // Only ever filled from a valid &str, cut at an ASCII boundary.
        std::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl std::fmt::Debug for InlineStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_normalisation() {
        let s = PriceScale::from_tick("0.10").unwrap();
        assert_eq!((s.exp, s.tick_m), (1, 1));
        let s = PriceScale::from_tick("0.50").unwrap();
        assert_eq!((s.exp, s.tick_m), (1, 5));
        let s = PriceScale::from_tick("1").unwrap();
        assert_eq!((s.exp, s.tick_m), (0, 1));
        let s = PriceScale::from_tick("0.0025").unwrap();
        assert_eq!((s.exp, s.tick_m), (4, 25));
        assert!(PriceScale::from_tick("0").is_none());
        assert!(PriceScale::from_tick("abc").is_none());
    }

    #[test]
    fn parse_variants_agree() {
        let s = PriceScale::from_tick("0.10").unwrap();
        assert_eq!(s.parse("82996.30"), Some(829963));
        assert_eq!(s.parse("82996.3"), Some(829963));
        assert_eq!(s.parse("82996.30000000"), Some(829963));
        assert_eq!(s.parse("82996"), Some(829960));
        assert_eq!(s.parse("82996.36"), Some(829964)); // rounds to nearest tick
        assert_eq!(s.parse(""), None);
        assert_eq!(s.parse("1.2.3"), None);
        assert_eq!(s.parse("-1"), None);
    }

    #[test]
    fn parse_non_unit_tick() {
        let s = PriceScale::from_tick("0.5").unwrap();
        assert_eq!(s.parse("100.5"), Some(201));
        assert_eq!(s.parse("100.0"), Some(200));
        assert_eq!(s.format(201), "100.5");
        let s = PriceScale::from_tick("0.0025").unwrap();
        assert_eq!(s.parse("1.0050"), Some(402));
        assert_eq!(s.format(402), "1.0050");
    }

    #[test]
    fn format_roundtrip() {
        let s = PriceScale::from_tick("0.01").unwrap();
        for p in ["0.01", "1.00", "82951.15", "100000.99"] {
            assert_eq!(s.format(s.parse(p).unwrap()), p);
        }
        let s = PriceScale::from_tick("1").unwrap();
        assert_eq!(s.format(s.parse("42").unwrap()), "42");
    }

    #[test]
    fn inline_str_trims() {
        let mut m = InlineStr::default();
        m.set_decimal("82996.30000000", 1);
        assert_eq!(m.as_str(), "82996.3");
        m.set_decimal("83001.01", 1);
        assert_eq!(m.as_str(), "83001.01");
        m.set_decimal("100.000", 0);
        assert_eq!(m.as_str(), "100");
        m.set_decimal("82981.46164493", 1);
        assert_eq!(m.as_str(), "82981.461");
    }
}
