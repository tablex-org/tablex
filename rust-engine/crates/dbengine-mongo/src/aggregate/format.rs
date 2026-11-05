//! Order-preserving pretty printer for pipelines, in mongosh shell syntax.
//!
//! The old `normalize` round-tripped through `serde_json::Value`, whose map
//! type sorts keys unless the `preserve_order` feature happens to be enabled
//! by some other crate in the build. Here the key order of every
//! [`bson::Document`] is written exactly as stored, so formatting a pipeline
//! never reorders `$sort` / `$group` / compound-key documents (see the
//! `key_order_is_preserved` test).
//!
//! Output is accepted back by [`super::parse`] (round trip is tested).
use bson::{Bson, Document};

const INDENT: &str = "  ";
/// Containers whose single-line form is at most this wide stay on one line.
const INLINE_WIDTH: usize = 72;

/// Pretty text for a whole pipeline: `[` / one stage per entry / `]`.
pub fn format_pipeline(stages: &[Document]) -> String {
    if stages.is_empty() {
        return "[]".into();
    }
    let mut out = String::from("[\n");
    for (index, stage) in stages.iter().enumerate() {
        out.push_str(INDENT);
        write_pretty(&Bson::Document(stage.clone()), 1, &mut out);
        if index + 1 < stages.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push(']');
    out
}

/// Pretty text for one stage (a stage editor's content).
pub fn format_stage(stage: &Document) -> String {
    let mut out = String::new();
    write_pretty(&Bson::Document(stage.clone()), 0, &mut out);
    out
}

/// Pretty text for any value (a stage editor's body).
pub fn format_value(value: &Bson) -> String {
    let mut out = String::new();
    write_pretty(value, 0, &mut out);
    out
}

/// Single-line text for any value.
pub fn format_compact(value: &Bson) -> String {
    let mut out = String::new();
    write_compact(value, &mut out);
    out
}

/// Compact, order-preserving **relaxed Extended JSON** of a pipeline: strict
/// JSON that any reader (including the previous app's json5 reader) accepts,
/// unlike the shell syntax of [`format_pipeline`]. Types survive through
/// `$oid` / `$date` / `$numberLong` / ... wrappers where relaxed EJSON has
/// them; this parser reads the result back (`parse` understands the wrappers).
pub fn format_ejson_pipeline(stages: &[Document]) -> String {
    let mut out = String::from("[");
    for (index, stage) in stages.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_ejson(&Bson::Document(stage.clone()), &mut out);
    }
    out.push(']');
    out
}

fn write_ejson(value: &Bson, out: &mut String) {
    match value {
        Bson::Document(document) => {
            out.push('{');
            for (index, (key, inner)) in document.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_ejson(inner, out);
            }
            out.push('}');
        }
        Bson::Array(items) => {
            out.push('[');
            for (index, inner) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_ejson(inner, out);
            }
            out.push(']');
        }
        Bson::String(text) => write_string(text, out),
        Bson::Boolean(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Bson::Null | Bson::Undefined => out.push_str("null"),
        // Relaxed EJSON: numbers that JSON can carry stay plain numbers.
        Bson::Int32(number) => out.push_str(&number.to_string()),
        Bson::Int64(number) => out.push_str(&number.to_string()),
        Bson::Double(number) if number.is_finite() => out.push_str(&serde_json::Value::from(*number).to_string()),
        other => out.push_str(&other.clone().into_relaxed_extjson().to_string()),
    }
}

fn is_bare_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' || first == '$' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

fn write_key(key: &str, out: &mut String) {
    if is_bare_key(key) {
        out.push_str(key);
    } else {
        write_string(key, out);
    }
}

fn write_string(text: &str, out: &mut String) {
    // serde_json's escaping is a strict subset of what the parser accepts.
    out.push_str(&serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into()));
}

fn is_container(value: &Bson) -> bool {
    matches!(value, Bson::Document(_) | Bson::Array(_))
}

fn write_scalar(value: &Bson, out: &mut String) {
    match value {
        Bson::Double(number) => {
            if number.is_nan() {
                out.push_str("NaN");
            } else if number.is_infinite() {
                out.push_str(if *number > 0.0 { "Infinity" } else { "-Infinity" });
            } else {
                let text = number.to_string();
                out.push_str(&text);
                if !text.contains(['.', 'e', 'E']) {
                    // Keep doubles distinguishable from int32 on re-parse.
                    out.push_str(".0");
                }
            }
        }
        Bson::String(text) => write_string(text, out),
        Bson::Boolean(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Bson::Null | Bson::Undefined => out.push_str("null"),
        Bson::Int32(number) => out.push_str(&number.to_string()),
        Bson::Int64(number) => {
            out.push_str("NumberLong(\"");
            out.push_str(&number.to_string());
            out.push_str("\")");
        }
        Bson::ObjectId(oid) => {
            out.push_str("ObjectId(\"");
            out.push_str(&oid.to_hex());
            out.push_str("\")");
        }
        Bson::DateTime(date) => match date.try_to_rfc3339_string() {
            Ok(text) => {
                out.push_str("ISODate(\"");
                out.push_str(&text);
                out.push_str("\")");
            }
            // Out of the RFC 3339 range: keep the exact instant.
            Err(_) => {
                out.push_str("new Date(");
                out.push_str(&date.timestamp_millis().to_string());
                out.push(')');
            }
        },
        Bson::Decimal128(decimal) => {
            out.push_str("NumberDecimal(\"");
            out.push_str(&decimal.to_string());
            out.push_str("\")");
        }
        Bson::RegularExpression(regex) => {
            out.push('/');
            let mut in_class = false;
            let mut chars = regex.pattern.chars().peekable();
            while let Some(ch) = chars.next() {
                match ch {
                    '\\' => {
                        out.push('\\');
                        if let Some(next) = chars.next() {
                            out.push(next);
                        }
                    }
                    '[' => {
                        in_class = true;
                        out.push(ch);
                    }
                    ']' => {
                        in_class = false;
                        out.push(ch);
                    }
                    '/' if !in_class => out.push_str("\\/"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    other => out.push(other),
                }
            }
            out.push('/');
            out.push_str(&regex.options);
        }
        Bson::Timestamp(timestamp) => {
            out.push_str(&format!(
                "Timestamp({}, {})",
                timestamp.time, timestamp.increment
            ));
        }
        Bson::Binary(binary) => {
            use bson::spec::BinarySubtype;
            let base64 = match Bson::Binary(binary.clone()).into_relaxed_extjson() {
                serde_json::Value::Object(map) => map
                    .get("$binary")
                    .and_then(|inner| inner.get("base64"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                _ => None,
            }
            .unwrap_or_default();
            if binary.subtype == BinarySubtype::Uuid && binary.bytes.len() == 16 {
                let hex: String = binary.bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                out.push_str(&format!(
                    "UUID(\"{}-{}-{}-{}-{}\")",
                    &hex[0..8],
                    &hex[8..12],
                    &hex[12..16],
                    &hex[16..20],
                    &hex[20..32]
                ));
            } else {
                out.push_str(&format!(
                    "BinData({}, \"{}\")",
                    u8::from(binary.subtype),
                    base64
                ));
            }
        }
        Bson::MinKey => out.push_str("MinKey()"),
        Bson::MaxKey => out.push_str("MaxKey()"),
        Bson::Symbol(symbol) => write_string(symbol, out),
        // Rare: JS code, DbPointer, scoped code. Extended JSON stays valid
        // input for the parser only for the wrappers it knows, so this is a
        // display-only fallback.
        other => out.push_str(&other.clone().into_relaxed_extjson().to_string()),
    }
}

fn write_compact(value: &Bson, out: &mut String) {
    match value {
        Bson::Document(document) => {
            if document.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{ ");
            for (index, (key, inner)) in document.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_key(key, out);
                out.push_str(": ");
                write_compact(inner, out);
            }
            out.push_str(" }");
        }
        Bson::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, inner) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_compact(inner, out);
            }
            out.push(']');
        }
        scalar => write_scalar(scalar, out),
    }
}

fn write_pretty(value: &Bson, depth: usize, out: &mut String) {
    if !is_container(value) {
        write_scalar(value, out);
        return;
    }
    let compact = format_compact(value);
    if compact.len() + depth * INDENT.len() <= INLINE_WIDTH && !compact.contains('\n') {
        out.push_str(&compact);
        return;
    }
    let pad = INDENT.repeat(depth + 1);
    let close_pad = INDENT.repeat(depth);
    match value {
        Bson::Document(document) => {
            out.push_str("{\n");
            let last = document.len().saturating_sub(1);
            for (index, (key, inner)) in document.iter().enumerate() {
                out.push_str(&pad);
                write_key(key, out);
                out.push_str(": ");
                write_pretty(inner, depth + 1, out);
                if index != last {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&close_pad);
            out.push('}');
        }
        Bson::Array(items) => {
            out.push_str("[\n");
            let last = items.len().saturating_sub(1);
            for (index, inner) in items.iter().enumerate() {
                out.push_str(&pad);
                write_pretty(inner, depth + 1, out);
                if index != last {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&close_pad);
            out.push(']');
        }
        _ => unreachable!("containers only"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::parse_pipeline;
    use super::*;
    use bson::doc;

    #[test]
    fn key_order_is_preserved() {
        let stages = vec![
            doc! { "$sort": { "zeta": 1, "alpha": -1, "mid": 1 } },
            doc! { "$group": { "_id": "$k", "z": { "$sum": 1 }, "a": { "$max": "$a" } } },
        ];
        let text = format_pipeline(&stages);
        let zeta = text.find("zeta").unwrap();
        let alpha = text.find("alpha").unwrap();
        let mid = text.find("mid").unwrap();
        assert!(zeta < alpha && alpha < mid, "{text}");
        let z = text.find("z: {").unwrap();
        let a = text.find(", a: {").unwrap();
        assert!(z < a, "{text}");
        // And the order survives a full parse round trip.
        assert_eq!(parse_pipeline(&text).unwrap().stages, stages);
    }

    #[test]
    fn short_containers_inline_long_ones_break() {
        let text = format_pipeline(&[
            doc! { "$match": { "status": "paid" } },
            doc! { "$project": {
                "customerName": "$customer.firstName",
                "customerEmail": "$customer.email",
                "totalWithTax": { "$multiply": ["$total", 1.2] },
            } },
        ]);
        assert!(text.contains("{ $match: { status: \"paid\" } }"), "{text}");
        assert!(text.contains("$project: {\n"), "{text}");
    }

    #[test]
    fn every_bson_scalar_round_trips_through_text() {
        let stages = vec![doc! { "$match": {
            "oid": bson::oid::ObjectId::parse_str("507f191e810c19729de860ea").unwrap(),
            "date": bson::DateTime::from_millis(1_700_000_000_123),
            "i32": 5, "i64": 9_007_199_254_740_993i64, "dbl": 2.0, "frac": 0.25,
            "neg": -3.5e-7, "text": "quote \" back \\ nl \n tab \t é 😀",
            "re": bson::Regex { pattern: "a/b[/]\\d+".into(), options: "i".into() },
            "ts": bson::Timestamp { time: 1, increment: 2 },
            "bin": bson::Binary { subtype: bson::spec::BinarySubtype::Generic, bytes: vec![1, 2, 3, 250] },
            "uuid": bson::Binary { subtype: bson::spec::BinarySubtype::Uuid, bytes: (0..16).collect() },
            "dec": "1.50".parse::<bson::Decimal128>().unwrap(),
            "n": Bson::Null, "t": true, "min": Bson::MinKey, "max": Bson::MaxKey,
            "weird key": 1, "9lives": 2, "": 3,
            "arr": [1, [2, [3]], { "k": "v" }],
            "emptyd": {}, "emptya": [],
        } }];
        let text = format_pipeline(&stages);
        let parsed = parse_pipeline(&text).unwrap_or_else(|error| panic!("{error}\n{text}"));
        assert_eq!(parsed.stages, stages, "{text}");
    }

    #[test]
    fn nan_and_infinity_survive() {
        let stages = vec![doc! { "$match": { "a": f64::INFINITY, "b": f64::NEG_INFINITY } }];
        let parsed = parse_pipeline(&format_pipeline(&stages)).unwrap();
        assert_eq!(parsed.stages, stages);
        let text = format_compact(&Bson::Double(f64::NAN));
        assert_eq!(text, "NaN");
    }

    #[test]
    fn ejson_pipeline_is_strict_json_keeps_key_order_and_reads_back() {
        let stages = vec![
            doc! { "$match": { "z": 1, "a": { "$gte": bson::DateTime::from_millis(1_700_000_000_000) },
                   "id": bson::oid::ObjectId::parse_str("507f191e810c19729de860ea").unwrap(), "t": "q\"x\n" } },
            doc! { "$limit": 5 },
        ];
        let text = format_ejson_pipeline(&stages);
        let value: serde_json::Value = serde_json::from_str(&text).expect("strict JSON");
        assert_eq!(value.as_array().unwrap().len(), 2);
        assert!(text.find("\"z\"").unwrap() < text.find("\"a\"").unwrap(), "key order kept: {text}");
        assert!(text.contains("\"$oid\":\"507f191e810c19729de860ea\""), "{text}");
        assert!(!text.contains("ObjectId("), "no shell syntax: {text}");
        assert_eq!(parse_pipeline(&text).unwrap().stages, stages);
        assert_eq!(format_ejson_pipeline(&[]), "[]");
    }

    #[test]
    fn empty_pipeline_formats_as_brackets() {
        assert_eq!(format_pipeline(&[]), "[]");
    }
}
