use bson::Bson;
use std::collections::BTreeMap;

/// MongoDB field types for UI display.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    ObjectId,
    Int32,
    Int64,
    Double,
    Decimal128,
    String,
    Bool,
    Date,
    Null,
    Array,
    Object,
    Binary,
    Timestamp,
    Regex,
    Undefined,
}

impl FieldType {
    pub fn as_str(&self) -> &'static str {
        match self {
            FieldType::ObjectId => "objectId",
            FieldType::Int32 => "int32",
            FieldType::Int64 => "int64",
            FieldType::Double => "double",
            FieldType::Decimal128 => "decimal128",
            FieldType::String => "string",
            FieldType::Bool => "bool",
            FieldType::Date => "date",
            FieldType::Null => "null",
            FieldType::Array => "array",
            FieldType::Object => "object",
            FieldType::Binary => "binary",
            FieldType::Timestamp => "timestamp",
            FieldType::Regex => "regex",
            FieldType::Undefined => "undefined",
        }
    }
}

/// A parsed field ready for FFI conversion.
#[derive(Debug, Clone)]
pub struct ParsedField {
    pub key: String,
    pub field_type: FieldType,
    pub display_value: String,
    pub is_primary_key: bool,
}

/// Detect the type of a BSON value.
fn detect_type(val: &Bson) -> FieldType {
    match val {
        Bson::ObjectId(_) => FieldType::ObjectId,
        Bson::Int32(_) => FieldType::Int32,
        Bson::Int64(_) => FieldType::Int64,
        Bson::Double(_) => FieldType::Double,
        Bson::Decimal128(_) => FieldType::Decimal128,
        Bson::String(_) => FieldType::String,
        Bson::Boolean(_) => FieldType::Bool,
        Bson::DateTime(_) => FieldType::Date,
        Bson::Null => FieldType::Null,
        Bson::Array(_) => FieldType::Array,
        Bson::Document(_) => FieldType::Object,
        Bson::Binary(_) => FieldType::Binary,
        Bson::Timestamp(_) => FieldType::Timestamp,
        Bson::RegularExpression(_) => FieldType::Regex,
        Bson::Undefined => FieldType::Undefined,
        _ => FieldType::String,
    }
}

/// Format a BSON value for display.
fn format_display_value(val: &Bson) -> String {
    match val {
        Bson::ObjectId(oid) => oid.to_hex(),
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(n) => {
            if n.is_nan() {
                "NaN".to_string()
            } else if n.is_infinite() && *n > 0.0 {
                "Infinity".to_string()
            } else if n.is_infinite() {
                "-Infinity".to_string()
            } else {
                n.to_string()
            }
        }
        Bson::Decimal128(d) => d.to_string(),
        Bson::String(s) => s.clone(),
        Bson::Boolean(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Bson::DateTime(dt) => {
            let chrono_dt = chrono::DateTime::from_timestamp_millis(dt.timestamp_millis())
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH);
            chrono_dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        }
        Bson::Null => "NULL".to_string(),
        Bson::Array(arr) => serde_json::to_string(arr).unwrap_or_else(|_| "[]".to_string()),
        Bson::Document(doc) => serde_json::to_string(doc).unwrap_or_else(|_| "{}".to_string()),
        Bson::Binary(bin) => format!("<binary {} bytes>", bin.bytes.len()),
        Bson::Timestamp(ts) => format!("timestamp({}:{})", ts.time, ts.increment),
        Bson::RegularExpression(re) => format!("/{}/{}", re.pattern, re.options),
        Bson::Undefined => "undefined".to_string(),
        _ => format!("{:?}", val),
    }
}

/// Parse a BSON document into a list of fields sorted by key.
pub fn parse_document_fields(doc: &bson::Document) -> Vec<ParsedField> {
    let mut fields: Vec<ParsedField> = doc
        .iter()
        .map(|(key, val)| ParsedField {
            key: key.to_string(),
            field_type: detect_type(val),
            display_value: format_display_value(val),
            is_primary_key: key == "_id",
        })
        .collect();
    fields.sort_by(|a, b| a.key.cmp(&b.key));
    fields
}

/// Parse a JSON string into fields (for when BSON is not available).
pub fn parse_json_fields(json_str: &str) -> Option<Vec<ParsedField>> {
    let serde_val: serde_json::Value = serde_json::from_str(json_str).ok()?;
    match serde_val {
        serde_json::Value::Object(map) => {
            let mut fields: Vec<ParsedField> = map
                .iter()
                .map(|(key, val)| {
                    let (field_type, display_value) = detect_json_type_and_value(val, key);
                    ParsedField {
                        key: key.clone(),
                        field_type,
                        display_value,
                        is_primary_key: key == "_id",
                    }
                })
                .collect();
            fields.sort_by(|a, b| a.key.cmp(&b.key));
            Some(fields)
        }
        _ => None,
    }
}

/// Detect type and format display value from a serde_json::Value.
fn detect_json_type_and_value(val: &serde_json::Value, key: &str) -> (FieldType, String) {
    match val {
        serde_json::Value::String(s) => {
            // Check for ObjectId-like strings (24 hex chars) when key is _id
            if key == "_id" && s.len() == 24 && s.chars().all(|c| c.is_ascii_hexdigit()) {
                return (FieldType::ObjectId, s.clone());
            }
            // Check for ISO8601 date strings
            if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
                return (FieldType::Date, s.clone());
            }
            (FieldType::String, s.clone())
        }
        serde_json::Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                (FieldType::Int64, n.to_string())
            } else {
                (FieldType::Double, n.to_string())
            }
        }
        serde_json::Value::Bool(b) => (FieldType::Bool, b.to_string()),
        serde_json::Value::Null => (FieldType::Null, "NULL".to_string()),
        serde_json::Value::Array(arr) => {
            let display = serde_json::to_string(arr).unwrap_or_else(|_| "[]".to_string());
            (FieldType::Array, display)
        }
        serde_json::Value::Object(obj) => {
            let display = serde_json::to_string(obj).unwrap_or_else(|_| "{}".to_string());
            (FieldType::Object, display)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_document() {
        let json = r#"{"_id": "507f1f77bcf86cd799439011", "name": "test", "age": 30, "active": true, "score": 1.5, "born": null}"#;
        let fields = parse_json_fields(json).unwrap();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[0].key, "_id");
        assert_eq!(fields[0].field_type, FieldType::ObjectId);
        assert!(fields[0].is_primary_key);
        assert_eq!(fields[1].key, "active");
        assert_eq!(fields[1].field_type, FieldType::Bool);
        assert_eq!(fields[2].key, "age");
        assert_eq!(fields[2].field_type, FieldType::Int64);
        assert_eq!(fields[3].key, "born");
        assert_eq!(fields[3].field_type, FieldType::Null);
        assert_eq!(fields[4].key, "name");
        assert_eq!(fields[4].field_type, FieldType::String);
        assert_eq!(fields[5].key, "score");
        assert_eq!(fields[5].field_type, FieldType::Double);
    }

    #[test]
    fn test_parse_nested_types() {
        let json = r#"{"tags": ["a", "b"], "meta": {"key": "val"}}"#;
        let fields = parse_json_fields(json).unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].key, "meta");
        assert_eq!(fields[0].field_type, FieldType::Object);
        assert_eq!(fields[1].key, "tags");
        assert_eq!(fields[1].field_type, FieldType::Array);
    }

    #[test]
    fn test_parse_empty_document() {
        let json = r#"{}"#;
        let fields = parse_json_fields(json).unwrap();
        assert_eq!(fields.len(), 0);
    }

    #[test]
    fn test_parse_invalid_json() {
        let json = r#"not json"#;
        let result = parse_json_fields(json);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_iso8601_date() {
        let json = r#"{"created": "2024-01-15T10:30:00Z"}"#;
        let fields = parse_json_fields(json).unwrap();
        assert_eq!(fields[0].field_type, FieldType::Date);
    }

    #[test]
    fn test_parse_non_object() {
        let json = r#"[1, 2, 3]"#;
        let result = parse_json_fields(json);
        assert!(result.is_none());
    }

    #[test]
    fn test_field_sorting() {
        let json = r#"{"z": 1, "a": 2, "m": 3}"#;
        let fields = parse_json_fields(json).unwrap();
        assert_eq!(fields[0].key, "a");
        assert_eq!(fields[1].key, "m");
        assert_eq!(fields[2].key, "z");
    }
}
