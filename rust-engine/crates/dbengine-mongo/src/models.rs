use bson::{Bson, Document};
use engine_driver_api::RowField;

/// The fields of one document for the inspector. `RowField::value` is canonical
/// Extended JSON v2 (`Bson::into_canonical_extjson`): lossless for every BSON
/// type (Int32 / Int64 / Double incl. NaN and infinities / Decimal128 / Date /
/// Binary subtype ...) and in the document's key order, so a client can write
/// back exactly what it read. Human-readable forms are derived from it.
///
/// A field is editable only when the row has a usable `_id` (its write filter),
/// is not `_id` itself, and its name can be addressed by `$set` (a name with a
/// `.` or a leading `$` would be taken as a path or an operator).
pub(crate) fn document_row_fields(document: &Document) -> Vec<RowField> {
    let has_identity = mutation_identity(document).is_some();
    document
        .iter()
        .map(|(name, value)| RowField {
            name: name.into(),
            native_type: bson_native_type(value).into(),
            display_text: crate::render::format_cell_display_value(
                Some(value),
                crate::render::DEFAULT_MAX_BYTES_PER_CELL,
            )
            .0,
            value: bson_canonical(value),
            editable: has_identity && name != "_id" && field_name_is_settable(name),
        })
        .collect()
}

/// `{ "_id": <canonical> }` when the document has an `_id` that can address
/// exactly one document; `None` otherwise (a projection that dropped `_id`).
pub(crate) fn mutation_identity(document: &Document) -> Option<serde_json::Value> {
    let id = document.get("_id").filter(|id| id_can_identify(id))?;
    Some(serde_json::json!({ "_id": bson_canonical(id) }))
}

/// An `_id` that, used as an equality filter, selects that document and no
/// other: not an array, a regex or an operator document.
pub(crate) fn id_can_identify(id: &Bson) -> bool {
    match id {
        Bson::Array(_) | Bson::RegularExpression(_) | Bson::Undefined => false,
        Bson::Document(document) => !document.keys().any(|key| key.starts_with('$')),
        _ => true,
    }
}

/// A top-level field `$set` addresses by its literal name.
pub(crate) fn field_name_is_settable(name: &str) -> bool {
    !name.is_empty() && !name.contains('.') && !name.starts_with('$')
}

pub(crate) fn bson_native_type(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "document",
        Bson::Boolean(_) => "boolean",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::JavaScriptCodeWithScope(_) => "javascript_scope",
        Bson::Array(_) => "array",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binary",
        Bson::ObjectId(_) => "object_id",
        Bson::DateTime(_) => "date_time",
        Bson::Decimal128(_) => "decimal128",
        Bson::Symbol(_) => "symbol",
        Bson::DbPointer(_) => "db_pointer",
        Bson::Undefined => "undefined",
        Bson::Null => "null",
        Bson::Int32(_) => "int32",
        Bson::Int64(_) => "int64",
        Bson::MinKey => "min_key",
        Bson::MaxKey => "max_key",
    }
}

pub(crate) fn bson_canonical(value: &Bson) -> serde_json::Value {
    value.clone().into_canonical_extjson()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::{doc, oid::ObjectId, DateTime};

    #[test]
    fn preserves_object_id_and_datetime_as_extended_json() {
        let id = ObjectId::parse_str("66d4f84d0000000000000003").expect("valid object id");
        let timestamp = DateTime::parse_rfc3339_str("2024-09-01T00:00:00Z").expect("valid date");
        let fields = document_row_fields(&doc! { "_id": id, "createdAt": timestamp });

        assert_eq!(fields[0].native_type, "object_id");
        assert_eq!(fields[0].value["$oid"], id.to_hex());
        assert!(!fields[0].editable);
        assert_eq!(fields[1].native_type, "date_time");
        assert_eq!(
            fields[1].value["$date"]["$numberLong"],
            timestamp.timestamp_millis().to_string()
        );
    }

    #[test]
    fn keeps_non_id_fields_editable_with_stable_native_types() {
        let fields = document_row_fields(&doc! {
            "_id": 1,
            "name": "Ada",
            "active": true,
            "count": 7_i64,
            "nested": { "value": 1 },
            "items": [1, 2],
            "empty": Bson::Null,
        });

        let types: Vec<_> = fields
            .iter()
            .map(|field| field.native_type.as_str())
            .collect();
        assert_eq!(
            types,
            ["int32", "string", "boolean", "int64", "document", "array", "null"]
        );
        assert!(fields.iter().skip(1).all(|field| field.editable));
        assert!(!fields[0].editable);
    }

    #[test]
    fn values_are_canonical_so_numeric_types_and_non_finite_doubles_survive() {
        let fields = document_row_fields(&doc! {
            "_id": 1_i32,
            "i32": 5_i32, "i64": 5_i64, "neg": -5_i32, "d": 5.0, "nan": f64::NAN,
            "inf": f64::INFINITY, "nested": { "n": 12345_i64, "a": [1_i64, 2.5] },
        });
        let value = |name: &str| {
            fields
                .iter()
                .find(|f| f.name == name)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(value("i32"), serde_json::json!({"$numberInt": "5"}));
        assert_eq!(value("i64"), serde_json::json!({"$numberLong": "5"}));
        assert_eq!(value("neg"), serde_json::json!({"$numberInt": "-5"}));
        assert_eq!(value("d"), serde_json::json!({"$numberDouble": "5.0"}));
        assert_eq!(value("nan"), serde_json::json!({"$numberDouble": "NaN"}));
        assert_eq!(
            value("inf"),
            serde_json::json!({"$numberDouble": "Infinity"})
        );
        assert_eq!(
            value("nested"),
            serde_json::json!({"n": {"$numberLong": "12345"}, "a": [{"$numberLong": "1"}, {"$numberDouble": "2.5"}]})
        );
        let back = Bson::try_from(value("nested")).unwrap();
        assert_eq!(
            back,
            Bson::Document(doc! { "n": 12345_i64, "a": [1_i64, 2.5] })
        );
    }

    #[test]
    fn rows_without_a_usable_id_are_read_only() {
        let without_id = document_row_fields(&doc! { "name": "Ada" });
        assert!(without_id.iter().all(|field| !field.editable));
        assert!(mutation_identity(&doc! { "name": "Ada" }).is_none());
        for bad in [
            Bson::Array(vec![]),
            Bson::Document(doc! { "$ne": 1 }),
            Bson::RegularExpression(bson::Regex {
                pattern: "a".into(),
                options: "".into(),
            }),
        ] {
            assert!(mutation_identity(&doc! { "_id": bad }).is_none());
        }
    }

    #[test]
    fn only_addressable_field_names_are_editable() {
        let fields = document_row_fields(&doc! { "_id": 1, "ok": 1, "a.b": 2, "$set": 3, "": 4 });
        let editable: Vec<_> = fields
            .iter()
            .map(|f| (f.name.as_str(), f.editable))
            .collect();
        assert_eq!(
            editable,
            [
                ("_id", false),
                ("ok", true),
                ("a.b", false),
                ("$set", false),
                ("", false)
            ]
        );
    }

    #[test]
    fn identity_of_every_id_type_is_the_canonical_value() {
        let ids = [
            Bson::ObjectId(ObjectId::parse_str("66d4f84d0000000000000003").unwrap()),
            Bson::String("x".into()),
            Bson::Int32(7),
            Bson::Int64(7),
            Bson::Double(7.5),
            Bson::Double(f64::NAN),
            Bson::DateTime(DateTime::from_millis(5)),
            Bson::Binary(bson::Binary {
                subtype: bson::spec::BinarySubtype::Uuid,
                bytes: vec![1; 16],
            }),
            Bson::Document(doc! { "b": 1_i64, "a": 2 }),
            Bson::Null,
        ];
        for id in ids {
            let identity = mutation_identity(&doc! { "_id": id.clone() }).expect("identity");
            let back = Bson::try_from(identity["_id"].clone()).unwrap();
            assert_eq!(
                back.clone().into_canonical_extjson(),
                id.into_canonical_extjson(),
                "{identity}"
            );
        }
    }
}
