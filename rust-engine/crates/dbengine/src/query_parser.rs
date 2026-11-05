//! Filter / sort / projection / update text -> BSON document.
//!
//! One syntax for everything the app can produce or a user can type: JSON5-ish
//! text, the shell constructors (`ObjectId("…")`, `ISODate("…")`,
//! `NumberLong(5)`, `NumberDecimal("…")`, `UUID("…")`, `BinData(…)`, `/re/i` …)
//! and MongoDB Extended JSON v2, canonical and relaxed (`{"$oid": "…"}`,
//! `{"$date": …}`, `{"$numberLong": "5"}` …), mixed freely. See
//! [`engine_mongo_script::shell_value`].

use bson::Document;
use engine_mongo_script::shell_value::{parse_document, NumberTyping};

pub fn parse_query(query_str: &str) -> Result<Document, String> {
    parse_document(query_str, NumberTyping::Filter)
        .map_err(|error| format!("invalid query syntax: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_query() {
        let doc = parse_query("{name: 'John'}").unwrap();
        assert_eq!(doc.get_str("name").unwrap(), "John");
    }

    #[test]
    fn test_objectid() {
        let doc = parse_query("{_id: ObjectId(\"507f191e810c19729de860ea\")}").unwrap();
        assert!(doc.get("_id").is_some());
    }

    #[test]
    fn test_nested_operators() {
        let doc = parse_query("{age: {$gt: 25, $lt: 50}}").unwrap();
        let age_doc = doc.get_document("age").unwrap();
        assert!(age_doc.contains_key("$gt"));
        assert!(age_doc.contains_key("$lt"));
    }

    #[test]
    fn test_or_query() {
        let doc = parse_query("{$or: [{name: 'John'}, {name: 'Jane'}]}").unwrap();
        assert!(doc.contains_key("$or"));
    }

    #[test]
    fn test_number_long() {
        let doc = parse_query("{_id: NumberLong(\"10\")}").unwrap();
        let id_val = doc.get("_id").unwrap();
        assert_eq!(id_val.as_i64().unwrap(), 10);
    }

    #[test]
    fn test_number_int() {
        let doc = parse_query("{_id: NumberInt(\"10\")}").unwrap();
        let id_val = doc.get("_id").unwrap();
        assert_eq!(id_val.as_i32().unwrap(), 10);
    }

    #[test]
    fn test_objectid_marker() {
        let doc = parse_query("{_id: ObjectId(\"507f191e810c19729de860ea\")}").unwrap();
        let id_val = doc.get("_id").unwrap();
        assert!(id_val.as_object_id().is_some());
    }

    #[test]
    fn test_update_with_set() {
        let doc =
            parse_query("{\"$set\": {\"name\": \"John\", \"age\": NumberInt(\"30\")}}").unwrap();
        let set_doc = doc.get_document("$set").unwrap();
        assert_eq!(set_doc.get_str("name").unwrap(), "John");
        assert_eq!(set_doc.get_i32("age").unwrap(), 30);
    }

    #[test]
    fn test_update_with_isodate() {
        let doc =
            parse_query("{\"$set\": {\"created\": ISODate(\"2024-01-15T10:30:00Z\")}}").unwrap();
        let set_doc = doc.get_document("$set").unwrap();
        assert!(set_doc.get("created").is_some());
        assert!(set_doc.get_datetime("created").is_ok());
    }

    #[test]
    fn test_extended_json_typed_values_decode() {
        let doc = parse_query(
            r#"{ _id: {"$oid": "507f191e810c19729de860ea"}, at: {"$gte": {"$date": "2024-09-01T00:00:00Z"}},
                n: {"$numberLong": "10"}, d: {"$numberDecimal": "1.5"}, i: {"$numberInt": "3"},
                f: {"$numberDouble": "2.5"}, big: {"$date": {"$numberLong": "1725148800000"}} }"#,
        )
        .unwrap();
        assert!(doc.get_object_id("_id").is_ok());
        assert!(doc.get_document("at").unwrap().get_datetime("$gte").is_ok());
        assert_eq!(doc.get_i64("n").unwrap(), 10);
        assert!(matches!(doc.get("d"), Some(bson::Bson::Decimal128(_))));
        assert_eq!(doc.get_i32("i").unwrap(), 3);
        assert_eq!(doc.get_f64("f").unwrap(), 2.5);
        assert_eq!(
            doc.get_datetime("big").unwrap().timestamp_millis(),
            1_725_148_800_000
        );
    }

    #[test]
    fn test_shell_constructors_with_numbers_and_decimals() {
        let doc = parse_query(r#"{ a: NumberLong(5), b: Decimal128("2.25"), c: UUID("00010203-0405-0607-0809-0a0b0c0d0e0f"), r: /^a/i }"#).unwrap();
        assert_eq!(doc.get_i64("a").unwrap(), 5);
        assert!(matches!(doc.get("b"), Some(bson::Bson::Decimal128(_))));
        assert!(matches!(doc.get("c"), Some(bson::Bson::Binary(_))));
        assert!(matches!(
            doc.get("r"),
            Some(bson::Bson::RegularExpression(_))
        ));
    }

    #[test]
    fn test_constructor_text_inside_a_string_is_data() {
        let doc = parse_query(r#"{ note: 'ObjectId("507f191e810c19729de860ea")' }"#).unwrap();
        assert_eq!(
            doc.get_str("note").unwrap(),
            r#"ObjectId("507f191e810c19729de860ea")"#
        );
    }
}
