//! Minimal allocation-free JSON object scanning.
//!
//! When serde_json skips or captures a *nested* value (`IgnoredAny`,
//! `RawValue`), it tracks bracket depth in a scratch `Vec`, so each message
//! costs an allocation. Envelopes here are walked with a plain depth
//! counter instead. Each value comes back as a raw slice, and flat leaf
//! objects and level arrays still go through serde, which doesn't allocate
//! for them.

/// Iterates the top-level `(key, raw value)` pairs of a JSON object.
/// Keys are returned without quotes and must not contain escapes, which
/// holds for every exchange payload used here.
pub struct Fields<'a> {
    s: &'a str,
    i: usize,
    done: bool,
}

pub fn fields(s: &str) -> Fields<'_> {
    let b = s.as_bytes();
    let mut i = ws(b, 0);
    let done = b.get(i) != Some(&b'{');
    i += 1;
    Fields { s, i, done }
}

impl<'a> Iterator for Fields<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let b = self.s.as_bytes();
        let mut i = ws(b, self.i);
        if b.get(i) == Some(&b',') {
            i = ws(b, i + 1);
        }
        if b.get(i) != Some(&b'"') {
            self.done = true;
            return None;
        }
        let k_end = string_end(b, i)?;
        let key = &self.s[i + 1..k_end - 1];
        i = ws(b, k_end);
        if b.get(i) != Some(&b':') {
            self.done = true;
            return None;
        }
        let v_start = ws(b, i + 1);
        let Some(v_end) = value_end(b, v_start) else {
            self.done = true;
            return None;
        };
        self.i = v_end;
        Some((key, &self.s[v_start..v_end]))
    }
}

#[inline]
fn ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// `b[i]` is an opening quote; returns the index just past the closing one.
#[inline]
fn string_end(b: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Returns the index just past the value starting at `i`.
fn value_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => string_end(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']') && !b[j].is_ascii_whitespace()
            {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// A raw JSON string value without its quotes. `None` if it isn't a string
/// or contains escapes.
#[inline]
pub fn str(raw: &str) -> Option<&str> {
    let s = raw.strip_prefix('"')?.strip_suffix('"')?;
    (!s.contains('\\')).then_some(s)
}

#[inline]
pub fn u64(raw: &str) -> Option<u64> {
    raw.parse().ok()
}

#[inline]
pub fn i64(raw: &str) -> Option<i64> {
    raw.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_top_level_only() {
        let s =
            r#" { "a" : 1, "b":{"x":[1,{"y":"}"}]}, "c":"q\"uote", "d":[["1","2"]] ,"e":true } "#;
        let got: Vec<_> = fields(s).collect();
        assert_eq!(
            got,
            [
                ("a", "1"),
                ("b", r#"{"x":[1,{"y":"}"}]}"#),
                ("c", r#""q\"uote""#),
                ("d", r#"[["1","2"]]"#),
                ("e", "true")
            ]
        );
        assert_eq!(str(got[2].1), None);
        assert_eq!(str(r#""ok""#), Some("ok"));
        assert_eq!(i64("-5"), Some(-5));
    }

    #[test]
    fn malformed_input_stops() {
        assert_eq!(fields("[1,2]").count(), 0);
        assert_eq!(fields(r#"{"a":[1,2"#).count(), 0);
        assert_eq!(fields(r#"{"a":1,"b""#).count(), 1);
        assert_eq!(fields("").count(), 0);
    }
}
