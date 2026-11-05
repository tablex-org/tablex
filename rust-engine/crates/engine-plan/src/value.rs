//! Helpers to read loosely typed JSON / text values.

use serde_json::{Map, Value};

/// Longest property value kept, in characters (longer ones are cut with `…`).
pub(crate) const PROP_VALUE_CAP: usize = 4096;

/// A finite number from a JSON number or a numeric string (MySQL quotes its costs).
pub(crate) fn number(v: &Value) -> Option<f64> {
    let n = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    n.filter(|n| n.is_finite())
}

/// A non-negative whole count (saturating for huge values).
pub(crate) fn count(v: &Value) -> Option<u64> {
    number(v).filter(|n| *n >= 0.0).map(|n| n as u64)
}

pub(crate) fn text(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

pub(crate) fn field_number(o: &Map<String, Value>, key: &str) -> Option<f64> {
    o.get(key).and_then(number)
}

pub(crate) fn field_count(o: &Map<String, Value>, key: &str) -> Option<u64> {
    o.get(key).and_then(count)
}

pub(crate) fn field_text<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    o.get(key).and_then(text)
}

pub(crate) fn cap(s: String) -> String {
    crate::plan::cap_chars(s, PROP_VALUE_CAP)
}

/// Display text for a property value; `None` for null / empty containers.
/// Scalars print plainly, arrays of scalars join with `, `, anything else is
/// compact JSON.
pub(crate) fn prop_string(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(cap(s.clone())),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(a) if a.is_empty() => None,
        Value::Object(o) if o.is_empty() => None,
        Value::Array(a) if a.iter().all(|e| !e.is_array() && !e.is_object()) => {
            let parts: Vec<String> = a.iter().filter_map(prop_string).collect();
            Some(cap(parts.join(", ")))
        }
        other => Some(cap(other.to_string())),
    }
}

/// Count formatted for messages: `12000` -> `12,000`.
pub(crate) fn fmt_count(n: f64) -> String {
    if !(0.0..1e15).contains(&n) {
        return format!("{n:.3e}");
    }
    let digits = (n.round() as u64).to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub(crate) fn join_nonempty(parts: impl IntoIterator<Item = String>, sep: &str) -> Option<String> {
    let v: Vec<String> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    (!v.is_empty()).then(|| v.join(sep))
}
