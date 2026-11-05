//! Pure helpers behind the cell-detail "Value" section: display modes for
//! JSON-like values, display-only conversion previews and copy conditions.

use crate::mongo_result::ejson_scalar;
use serde_json::Value;

/// How a JSON-like cell value is presented in the Value box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueMode {
    /// Pretty-printed; a Mongo value is shown in shell syntax (`Long('5')`,
    /// `ISODate('…')`, `5.0`), the text the editors take back.
    #[default]
    Formatted,
    /// Pretty-printed canonical Extended JSON (Mongo only; equals `Formatted`
    /// for other engines).
    Canonical,
    /// The original text, untouched.
    Raw,
}

/// Presentation of Extended JSON in the whole-document JSON views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EjsonMode {
    /// [`shell_text`]: mongosh syntax, every BSON type distinguishable, parses
    /// back to the identical BSON.
    #[default]
    Shell,
    /// The stored canonical Extended JSON, unchanged.
    Canonical,
}

/// Relaxed view of canonical Extended JSON, the single convention used by every
/// read-only JSON view: `$oid` becomes its hex string, `$date` an ISO string and
/// `$numberInt/Long/Double/Decimal` plain numbers. Display-only (lossy: values
/// that do not fit a JSON number keep their wrapper); other wrappers such as
/// `$binary` are kept as they are.
pub fn relaxed_ejson(value: &Value) -> Value {
    match value {
        Value::Object(fields) if fields.len() == 1 => {
            let (key, inner) = fields.iter().next().expect("one field");
            match (key.as_str(), inner) {
                ("$oid", Value::String(hex)) => Value::String(hex.clone()),
                // Decimal128 can exceed f64: only unwrap when the number reads
                // back exactly, otherwise keep the lossless wrapper.
                ("$numberDecimal", Value::String(text)) => text
                    .parse::<serde_json::Number>()
                    .ok()
                    .filter(|number| number.to_string() == *text)
                    .map(Value::Number)
                    .unwrap_or_else(|| value.clone()),
                ("$date", _) => ejson_scalar(value)
                    .map(|(iso, _)| Value::String(iso))
                    .unwrap_or_else(|| value.clone()),
                ("$numberInt" | "$numberLong", Value::String(text)) => text
                    .parse::<i64>()
                    .map(Value::from)
                    .unwrap_or_else(|_| value.clone()),
                ("$numberDouble", Value::String(text)) => text
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
                    .unwrap_or_else(|| value.clone()),
                _ => Value::Object(
                    fields
                        .iter()
                        .map(|(key, inner)| (key.clone(), relaxed_ejson(inner)))
                        .collect(),
                ),
            }
        }
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, inner)| (key.clone(), relaxed_ejson(inner)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(relaxed_ejson).collect()),
        other => other.clone(),
    }
}

/// Shell-syntax text of a canonical Extended JSON value (`None` when `value`
/// is not valid Extended JSON).
pub fn shell_text(value: &Value, pretty: bool) -> Option<String> {
    bson::Bson::try_from(value.clone())
        .ok()
        .map(|bson| engine_mongo_script::shell_value::to_shell(&bson, pretty))
}

/// Text shown for a JSON-like value in `mode`. `canonical_pretty` is the
/// already pretty-printed canonical form (`CellDetail::formatted_json`). A
/// Mongo value's `raw` text is already in shell syntax (what the editor takes).
pub fn value_text(
    raw: &str,
    canonical_pretty: &str,
    mode: ValueMode,
    is_mongo: bool,
) -> String {
    match mode {
        ValueMode::Raw => raw.to_string(),
        ValueMode::Canonical => canonical_pretty.to_string(),
        ValueMode::Formatted if is_mongo => raw.to_string(),
        ValueMode::Formatted => canonical_pretty.to_string(),
    }
}

/// Display-only conversions offered by the "Convert preview" menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertKind {
    UnixSecondsToIso,
    UnixMillisToIso,
    IsoToUnixSeconds,
    IsoToUnixMillis,
    Base64ToText,
    HexToText,
}

impl ConvertKind {
    pub const ALL: [ConvertKind; 6] = [
        Self::UnixSecondsToIso,
        Self::UnixMillisToIso,
        Self::IsoToUnixSeconds,
        Self::IsoToUnixMillis,
        Self::Base64ToText,
        Self::HexToText,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::UnixSecondsToIso => "Unix seconds → ISO datetime",
            Self::UnixMillisToIso => "Unix milliseconds → ISO datetime",
            Self::IsoToUnixSeconds => "ISO date/time → Unix seconds",
            Self::IsoToUnixMillis => "ISO date/time → Unix milliseconds",
            Self::Base64ToText => "Base64 → UTF-8 text",
            Self::HexToText => "Hex → UTF-8 text",
        }
    }
}

/// Conversions that apply to `value`, in menu order. Empty for most values.
pub fn convert_options(value: &str) -> Vec<ConvertKind> {
    // Cheap early exit: none of the conversions apply to long text.
    if value.len() > 4096 {
        return Vec::new();
    }
    ConvertKind::ALL
        .into_iter()
        .filter(|kind| convert_preview(*kind, value).is_some())
        .collect()
}

pub fn convert_preview(kind: ConvertKind, value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    match kind {
        ConvertKind::UnixSecondsToIso => {
            let seconds = integer_with_digits(value, 9..=11)?;
            iso_from_millis(seconds.checked_mul(1000)?)
        }
        ConvertKind::UnixMillisToIso => iso_from_millis(integer_with_digits(value, 12..=14)?),
        ConvertKind::IsoToUnixSeconds => parse_instant(value).map(|millis| (millis.div_euclid(1000)).to_string()),
        ConvertKind::IsoToUnixMillis => parse_instant(value).map(|millis| millis.to_string()),
        ConvertKind::Base64ToText => {
            let unpadded = value.trim_end_matches('=');
            if value.len() < 4
                || !value.len().is_multiple_of(4)
                || value.len() - unpadded.len() > 2
                || !unpadded
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
            {
                return None;
            }
            let bytes = bson::Binary::from_base64(value, None).ok()?.bytes;
            printable_utf8(bytes)
        }
        ConvertKind::HexToText => {
            let digits = value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
                .or_else(|| value.strip_prefix("\\x"))
                .unwrap_or(value);
            if digits.is_empty()
                || !digits.len().is_multiple_of(2)
                || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return None;
            }
            let bytes = (0..digits.len())
                .step_by(2)
                .map(|index| u8::from_str_radix(&digits[index..index + 2], 16).ok())
                .collect::<Option<Vec<u8>>>()?;
            printable_utf8(bytes)
        }
    }
}

fn integer_with_digits(value: &str, digits: std::ops::RangeInclusive<usize>) -> Option<i64> {
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    if !digits.contains(&unsigned.len()) || !unsigned.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn iso_from_millis(millis: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis)
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
}

/// Milliseconds since the Unix epoch; timestamps without an offset are UTC.
fn parse_instant(value: &str) -> Option<i64> {
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(date.timestamp_millis());
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(date) = chrono::NaiveDateTime::parse_from_str(value, format) {
            return Some(date.and_utc().timestamp_millis());
        }
    }
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|date| date.and_utc().timestamp_millis())
}

fn printable_utf8(bytes: Vec<u8>) -> Option<String> {
    let text = String::from_utf8(bytes).ok()?;
    let printable = !text.is_empty()
        && text
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'));
    printable.then_some(text)
}

/// Mongo copy condition: `{ "field": <value in shell syntax> }`, which the find
/// bar parses back to the same typed value.
pub fn mongo_condition(field: &str, value: &Value) -> String {
    format!(
        "{{ {}: {} }}",
        serde_json::to_string(field).unwrap_or_default(),
        shell_text(value, false).unwrap_or_else(|| serde_json::to_string(value).unwrap_or_default())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn relaxed_ejson_flattens_dates_and_numbers_only() {
        let canonical = json!({
            "_id": {"$oid": "68a1"},
            "bin": {"$binary": {"base64": "aGk=", "subType": "00"}},
            "createdAt": {"$date": {"$numberLong": "1752033379687"}},
            "count": {"$numberLong": "42"},
            "small": {"$numberInt": "7"},
            "ratio": {"$numberDouble": "1.5"},
            "price": {"$numberDecimal": "9.25"},
            "huge": {"$numberDecimal": "NaN"},
            "precise": {"$numberDecimal": "12345678901234567890.25"},
            "items": [{"$numberInt": "1"}, {"nested": {"$date": "2025-01-01T00:00:00Z"}}],
        });

        assert_eq!(
            relaxed_ejson(&canonical),
            json!({
                "_id": "68a1",
                "bin": {"$binary": {"base64": "aGk=", "subType": "00"}},
                "createdAt": "2025-07-09T03:56:19.687Z",
                "count": 42,
                "small": 7,
                "ratio": 1.5,
                "price": 9.25,
                "huge": {"$numberDecimal": "NaN"},
                "precise": {"$numberDecimal": "12345678901234567890.25"},
                "items": [1, {"nested": "2025-01-01T00:00:00Z"}],
            })
        );
    }

    #[test]
    fn value_text_follows_the_selected_mode() {
        let raw = "{\n  \"n\": Long('5')\n}";
        let canonical = "{\n  \"n\": {\n    \"$numberLong\": \"5\"\n  }\n}";

        assert_eq!(value_text(raw, canonical, ValueMode::Raw, true), raw);
        assert_eq!(value_text(raw, canonical, ValueMode::Canonical, true), canonical);
        assert_eq!(value_text(raw, canonical, ValueMode::Formatted, true), raw);
        // Non-Mongo values never get EJSON treatment.
        assert_eq!(value_text(raw, canonical, ValueMode::Formatted, false), canonical);
    }

    #[test]
    fn unix_timestamps_convert_to_iso_datetimes() {
        assert_eq!(
            convert_preview(ConvertKind::UnixSecondsToIso, "1752033379").as_deref(),
            Some("2025-07-09T03:56:19Z")
        );
        assert_eq!(
            convert_preview(ConvertKind::UnixMillisToIso, "1752033379687").as_deref(),
            Some("2025-07-09T03:56:19.687Z")
        );
        // Wrong magnitude for the requested unit.
        assert!(convert_preview(ConvertKind::UnixSecondsToIso, "1752033379687").is_none());
        assert!(convert_preview(ConvertKind::UnixMillisToIso, "42").is_none());
        assert!(convert_preview(ConvertKind::UnixSecondsToIso, "17520x3379").is_none());
    }

    #[test]
    fn iso_dates_convert_to_unix_time() {
        assert_eq!(
            convert_preview(ConvertKind::IsoToUnixMillis, "2025-07-09T03:56:19.687Z").as_deref(),
            Some("1752033379687")
        );
        assert_eq!(
            convert_preview(ConvertKind::IsoToUnixSeconds, "2025-07-09 03:56:19").as_deref(),
            Some("1752033379")
        );
        assert_eq!(
            convert_preview(ConvertKind::IsoToUnixMillis, "2025-07-09").as_deref(),
            Some("1752019200000")
        );
        assert!(convert_preview(ConvertKind::IsoToUnixMillis, "not a date").is_none());
    }

    #[test]
    fn base64_and_hex_decode_only_to_printable_text() {
        assert_eq!(
            convert_preview(ConvertKind::Base64ToText, "aGVsbG8gd29ybGQ=").as_deref(),
            Some("hello world")
        );
        assert_eq!(
            convert_preview(ConvertKind::HexToText, "68656c6c6f").as_deref(),
            Some("hello")
        );
        assert_eq!(
            convert_preview(ConvertKind::HexToText, "0x68656c6c6f").as_deref(),
            Some("hello")
        );
        // Binary payloads, bad padding and plain numbers are not offered.
        assert!(convert_preview(ConvertKind::Base64ToText, "//79/A==").is_none());
        assert!(convert_preview(ConvertKind::Base64ToText, "hello").is_none());
        assert!(convert_preview(ConvertKind::HexToText, "1234").is_none());
        assert!(convert_preview(ConvertKind::HexToText, "abc").is_none());
    }

    #[test]
    fn only_applicable_conversions_are_listed() {
        assert_eq!(
            convert_options("1752033379"),
            vec![ConvertKind::UnixSecondsToIso]
        );
        assert_eq!(
            convert_options("2025-07-09T03:56:19Z"),
            vec![ConvertKind::IsoToUnixSeconds, ConvertKind::IsoToUnixMillis]
        );
        assert_eq!(
            convert_options("68656c6c6f"),
            vec![ConvertKind::HexToText]
        );
        assert!(convert_options("plain text value").is_empty());
        assert!(convert_options("").is_empty());
    }

    #[test]
    fn mongo_condition_quotes_the_field_and_pastes_back_as_the_same_typed_value() {
        let id = "66d4f84d0000000000000003";
        assert_eq!(
            mongo_condition("_id", &json!({"$oid": id})),
            format!(r#"{{ "_id": ObjectId('{id}') }}"#)
        );
        assert_eq!(mongo_condition("na\"me", &json!("Ada")), r#"{ "na\"me": "Ada" }"#);
        assert_eq!(mongo_condition("x", &json!(null)), r#"{ "x": null }"#);
        // Every type pastes back into the find bar as the very same BSON.
        let values = [
            json!({"$numberLong": "5"}), json!({"$numberInt": "5"}), json!({"$numberDouble": "5.0"}),
            json!({"$numberDouble": "NaN"}), json!({"$numberDecimal": "5"}),
            json!({"$date": {"$numberLong": "1725148800123"}}),
            json!({"$binary": {"base64": "AAH/", "subType": "00"}}),
            json!({"$regularExpression": {"pattern": "^a", "options": "i"}}),
            json!({"$timestamp": {"t": 1, "i": 2}}),
        ];
        for value in values {
            let condition = mongo_condition("f", &value);
            let parsed = engine_mongo_script::shell_value::parse_document(
                &condition,
                engine_mongo_script::shell_value::NumberTyping::Filter,
            )
            .expect(&condition);
            assert_eq!(
                bson::Bson::Document(parsed).into_canonical_extjson(),
                json!({"f": value}),
                "{condition}"
            );
        }
    }
}
