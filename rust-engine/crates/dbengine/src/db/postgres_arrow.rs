use arrow::datatypes::{DataType, Field, Schema};
use arrow_array::{BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray};
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio_postgres::{types::Type, Row};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgExportTypeKind {
    Integer,
    Float,
    Boolean,
    Text,
}

/// Format a `Decimal` for display — strips trailing zeros after decimal point
/// so `42.00` → `"42"` and `12345.67` → `"12345.67"`.
fn decimal_to_display_string(d: Decimal) -> String {
    d.normalize().to_string()
}

/// Convert a list of Postgres rows into an Arrow RecordBatch.
/// Only emits Swift-compatible Arrow types: Int64, Float64, Boolean, Utf8.
pub fn rows_to_record_batch(rows: &[Row]) -> Result<RecordBatch, String> {
    if rows.is_empty() {
        return Err("No rows to convert".to_string());
    }

    let first_row = &rows[0];
    let columns = first_row.columns();
    let num_cols = columns.len();

    let mut fields = Vec::with_capacity(num_cols);
    let mut col_types: Vec<&Type> = Vec::with_capacity(num_cols);

    for col in columns {
        let name = col.name().to_string();
        let pg_type = col.type_();
        let arrow_type = pg_type_to_arrow(pg_type);
        fields.push(native_type_field(&name, arrow_type, pg_type));
        col_types.push(pg_type);
    }

    let schema = Arc::new(Schema::new(fields));
    let mut arrays: Vec<Arc<dyn arrow_array::Array>> = Vec::with_capacity(num_cols);

    for col_idx in 0..num_cols {
        let pg_type = col_types[col_idx];
        let arrow_type = schema.field(col_idx).data_type().clone();
        let array = build_column_array(rows, col_idx, pg_type, &arrow_type)?;
        arrays.push(array);
    }

    RecordBatch::try_new(schema, arrays).map_err(|e| format!("Arrow build error: {}", e))
}

/// Field-metadata key for the native type name. Local copy (this file is
/// `include!`d into `dbengine-postgres`, which has no `arrow` module); a
/// `dbengine` test asserts it equals `arrow::NATIVE_TYPE_METADATA_KEY`.
pub(crate) const PG_NATIVE_TYPE_METADATA_KEY: &str = "tablex.native_type";

/// Arrow field carrying the Postgres native type name under
/// [`PG_NATIVE_TYPE_METADATA_KEY`], so the UI can show the same
/// type label as the catalog (`bigint`, `text`, ...) instead of the widened
/// Arrow logical type. Typmods (`varchar(255)`) are not part of
/// RowDescription, so ad-hoc queries carry the base type only.
fn native_type_field(name: &str, arrow_type: DataType, pg_type: &Type) -> Field {
    Field::new(name, arrow_type, true).with_metadata(std::collections::HashMap::from([(
        PG_NATIVE_TYPE_METADATA_KEY.to_string(),
        pg_native_type_name(pg_type),
    )]))
}

/// Canonical `format_type()` spelling for a Postgres type (without typmod).
/// Arrays render as `<elem>[]`; unknown types fall back to the type name.
pub fn pg_native_type_name(pg_type: &Type) -> String {
    let name = match *pg_type {
        Type::INT2 => "smallint",
        Type::INT4 => "integer",
        Type::INT8 => "bigint",
        Type::FLOAT4 => "real",
        Type::FLOAT8 => "double precision",
        Type::BOOL => "boolean",
        Type::VARCHAR => "character varying",
        Type::BPCHAR => "character",
        Type::VARBIT => "bit varying",
        Type::TIMESTAMP => "timestamp without time zone",
        Type::TIMESTAMPTZ => "timestamp with time zone",
        Type::TIME => "time without time zone",
        Type::TIMETZ => "time with time zone",
        _ => {
            return match pg_type.kind() {
                tokio_postgres::types::Kind::Array(element) => format!("{}[]", pg_native_type_name(element)),
                _ => pg_type.name().to_string(),
            };
        }
    };
    name.to_string()
}

/// Map PG types to Swift-compatible Arrow types only.
/// All integer types → Int64 (widening), all float types → Float64,
/// timestamps → Utf8 (ISO-8601), BYTEA → Utf8 (hex preview).
fn pg_type_to_arrow(pg_type: &Type) -> DataType {
    match *pg_type {
        Type::INT2 | Type::INT4 | Type::INT8 => DataType::Int64,
        Type::FLOAT4 | Type::FLOAT8 => DataType::Float64,
        Type::BOOL => DataType::Boolean,
        Type::TIMESTAMP
        | Type::TIMESTAMPTZ
        | Type::DATE
        | Type::TIME
        | Type::TIMETZ
        | Type::BYTEA
        | Type::TEXT
        | Type::VARCHAR
        | Type::UUID
        | Type::JSON
        | Type::JSONB
        | Type::BPCHAR
        | Type::NAME
        | Type::NUMERIC => DataType::Utf8,
        _ => DataType::Utf8,
    }
}

fn build_column_array(
    rows: &[Row],
    col_idx: usize,
    pg_type: &Type,
    arrow_type: &DataType,
) -> Result<Arc<dyn arrow_array::Array>, String> {
    match arrow_type {
        DataType::Int64 => {
            let values: Vec<Option<i64>> = rows
                .iter()
                .map(|r| match *pg_type {
                    Type::INT2 => r
                        .try_get::<_, Option<i16>>(col_idx)
                        .unwrap_or(None)
                        .map(|v| v as i64),
                    Type::INT4 => r
                        .try_get::<_, Option<i32>>(col_idx)
                        .unwrap_or(None)
                        .map(|v| v as i64),
                    Type::INT8 => r.try_get::<_, Option<i64>>(col_idx).unwrap_or(None),
                    _ => r.try_get::<_, Option<i64>>(col_idx).unwrap_or(None),
                })
                .collect();
            Ok(Arc::new(Int64Array::from(values)))
        }
        DataType::Float64 => {
            let values: Vec<Option<f64>> = rows
                .iter()
                .map(|r| match *pg_type {
                    Type::FLOAT4 => r
                        .try_get::<_, Option<f32>>(col_idx)
                        .unwrap_or(None)
                        .map(|v| v as f64),
                    Type::FLOAT8 => r.try_get::<_, Option<f64>>(col_idx).unwrap_or(None),
                    _ => r.try_get::<_, Option<f64>>(col_idx).unwrap_or(None),
                })
                .collect();
            Ok(Arc::new(Float64Array::from(values)))
        }
        DataType::Boolean => {
            let values: Vec<Option<bool>> = rows
                .iter()
                .map(|r| r.try_get::<_, Option<bool>>(col_idx).unwrap_or(None))
                .collect();
            Ok(Arc::new(BooleanArray::from(values)))
        }
        DataType::Utf8 => {
            let values: Vec<Option<String>> = rows
                .iter()
                .map(|row| pg_row_col_to_string(row, col_idx, pg_type))
                .collect();
            Ok(Arc::new(StringArray::from(values)))
        }
        _ => {
            let values: Vec<Option<String>> = rows
                .iter()
                .map(|row| pg_row_col_to_string(row, col_idx, pg_type))
                .collect();
            Ok(Arc::new(StringArray::from(values)))
        }
    }
}

/// Convert a single Postgres column value to an `Option<String>` for Utf8 columns.
/// Handles TEXT, VARCHAR, NUMERIC, DATE, TIME, JSON/JSONB, UUID, NAME, BPCHAR,
/// and unknown types via Debug-style fallback.
pub fn pg_row_col_to_string(row: &Row, col_idx: usize, pg_type: &Type) -> Option<String> {
    match *pg_type {
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => {
            row.try_get::<_, Option<String>>(col_idx).unwrap_or(None)
        }
        Type::UUID => row
            .try_get::<_, Option<uuid::Uuid>>(col_idx)
            .unwrap_or(None)
            .map(|u| u.to_string()),
        Type::NUMERIC => row
            .try_get::<_, Option<Decimal>>(col_idx)
            .unwrap_or(None)
            .map(decimal_to_display_string),
        Type::DATE => row
            .try_get::<_, Option<chrono::NaiveDate>>(col_idx)
            .unwrap_or(None)
            .map(|d| d.format("%Y-%m-%d").to_string()),
        Type::TIME => row
            .try_get::<_, Option<chrono::NaiveTime>>(col_idx)
            .unwrap_or(None)
            .map(|t| t.format("%H:%M:%S%.f").to_string()),
        Type::TIMETZ => row
            .try_get::<_, Option<chrono::NaiveTime>>(col_idx)
            .unwrap_or(None)
            .map(|t| t.format("%H:%M:%S%.f").to_string()),
        Type::TIMESTAMP => row
            .try_get::<_, Option<chrono::NaiveDateTime>>(col_idx)
            .unwrap_or(None)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S%.f").to_string()),
        Type::TIMESTAMPTZ => row
            .try_get::<_, Option<chrono::DateTime<chrono::Utc>>>(col_idx)
            .unwrap_or(None)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S%.f %Z").to_string()),
        Type::BYTEA => row
            .try_get::<_, Option<Vec<u8>>>(col_idx)
            .unwrap_or(None)
            .map(|bytes| {
                let hex: Vec<String> = bytes
                    .iter()
                    .take(64)
                    .map(|b| format!("{:02x}", b))
                    .collect();
                let preview = hex.join("");
                if bytes.len() > 64 {
                    format!("{}... ({} bytes)", preview, bytes.len())
                } else {
                    preview
                }
            }),
        Type::JSON | Type::JSONB => row
            .try_get::<_, Option<serde_json::Value>>(col_idx)
            .unwrap_or(None)
            .map(|v| v.to_string()),
        _ => row.try_get::<_, Option<String>>(col_idx).unwrap_or(None),
    }
}

pub fn pg_row_col_to_export_string(row: &Row, col_idx: usize, pg_type: &Type) -> Option<String> {
    match pg_export_type_kind(pg_type) {
        PgExportTypeKind::Integer => match *pg_type {
            Type::INT2 => row
                .try_get::<_, Option<i16>>(col_idx)
                .unwrap_or(None)
                .map(|v| v.to_string()),
            Type::INT4 => row
                .try_get::<_, Option<i32>>(col_idx)
                .unwrap_or(None)
                .map(|v| v.to_string()),
            Type::INT8 => row
                .try_get::<_, Option<i64>>(col_idx)
                .unwrap_or(None)
                .map(|v| v.to_string()),
            _ => None,
        },
        PgExportTypeKind::Float => match *pg_type {
            Type::FLOAT4 => row
                .try_get::<_, Option<f32>>(col_idx)
                .unwrap_or(None)
                .map(|v| v.to_string()),
            Type::FLOAT8 => row
                .try_get::<_, Option<f64>>(col_idx)
                .unwrap_or(None)
                .map(|v| v.to_string()),
            _ => None,
        },
        PgExportTypeKind::Boolean => row
            .try_get::<_, Option<bool>>(col_idx)
            .unwrap_or(None)
            .map(|v| v.to_string()),
        PgExportTypeKind::Text => pg_row_col_to_string(row, col_idx, pg_type),
    }
}

pub fn pg_export_type_kind(pg_type: &Type) -> PgExportTypeKind {
    match *pg_type {
        Type::INT2 | Type::INT4 | Type::INT8 => PgExportTypeKind::Integer,
        Type::FLOAT4 | Type::FLOAT8 => PgExportTypeKind::Float,
        Type::BOOL => PgExportTypeKind::Boolean,
        _ => PgExportTypeKind::Text,
    }
}

/// Extract the raw string representation of a primary-key column for cursor use.
/// Handles integer, UUID, and string PKs without triggering type errors.
pub fn pk_value_to_string(row: &Row, col_idx: usize, pg_type: &Type) -> Option<String> {
    match *pg_type {
        Type::INT2 => row
            .try_get::<_, Option<i16>>(col_idx)
            .unwrap_or(None)
            .map(|v| v.to_string()),
        Type::INT4 => row
            .try_get::<_, Option<i32>>(col_idx)
            .unwrap_or(None)
            .map(|v| v.to_string()),
        Type::INT8 => row
            .try_get::<_, Option<i64>>(col_idx)
            .unwrap_or(None)
            .map(|v| v.to_string()),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => {
            row.try_get::<_, Option<String>>(col_idx).unwrap_or(None)
        }
        Type::UUID => row
            .try_get::<_, Option<uuid::Uuid>>(col_idx)
            .unwrap_or(None)
            .map(|u| u.to_string()),
        Type::NUMERIC => row
            .try_get::<_, Option<Decimal>>(col_idx)
            .unwrap_or(None)
            .map(decimal_to_display_string),
        _ => row.try_get::<_, Option<String>>(col_idx).unwrap_or(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_type_to_arrow_int_types() {
        assert_eq!(pg_type_to_arrow(&Type::INT2), DataType::Int64);
        assert_eq!(pg_type_to_arrow(&Type::INT4), DataType::Int64);
        assert_eq!(pg_type_to_arrow(&Type::INT8), DataType::Int64);
    }

    #[test]
    fn pg_type_to_arrow_float_types() {
        assert_eq!(pg_type_to_arrow(&Type::FLOAT4), DataType::Float64);
        assert_eq!(pg_type_to_arrow(&Type::FLOAT8), DataType::Float64);
    }

    #[test]
    fn pg_type_to_arrow_string_types() {
        assert_eq!(pg_type_to_arrow(&Type::TEXT), DataType::Utf8);
        assert_eq!(pg_type_to_arrow(&Type::VARCHAR), DataType::Utf8);
        assert_eq!(pg_type_to_arrow(&Type::UUID), DataType::Utf8);
        assert_eq!(pg_type_to_arrow(&Type::JSON), DataType::Utf8);
        assert_eq!(pg_type_to_arrow(&Type::JSONB), DataType::Utf8);
        assert_eq!(pg_type_to_arrow(&Type::NUMERIC), DataType::Utf8);
    }

    #[test]
    fn numeric_decimal_roundtrip_preserves_precision() {
        use rust_decimal::Decimal;
        use std::str::FromStr;
        let d = Decimal::from_str("12345.67").unwrap();
        assert_eq!(decimal_to_display_string(d), "12345.67");
    }

    #[test]
    fn numeric_decimal_integer_has_no_trailing_dot() {
        use rust_decimal::Decimal;
        use std::str::FromStr;
        let d = Decimal::from_str("42").unwrap();
        assert_eq!(decimal_to_display_string(d), "42");
    }

    #[test]
    fn pg_type_to_arrow_other_types() {
        assert_eq!(pg_type_to_arrow(&Type::BOOL), DataType::Boolean);
        assert_eq!(pg_type_to_arrow(&Type::BYTEA), DataType::Utf8);
        assert_eq!(pg_type_to_arrow(&Type::TIMESTAMP), DataType::Utf8);
        assert_eq!(pg_type_to_arrow(&Type::TIMESTAMPTZ), DataType::Utf8);
        assert_eq!(pg_type_to_arrow(&Type::DATE), DataType::Utf8);
        assert_eq!(pg_type_to_arrow(&Type::TIME), DataType::Utf8);
    }

    #[test]
    fn export_type_kind_covers_numeric_and_boolean_types() {
        assert_eq!(pg_export_type_kind(&Type::INT4), PgExportTypeKind::Integer);
        assert_eq!(pg_export_type_kind(&Type::INT8), PgExportTypeKind::Integer);
        assert_eq!(pg_export_type_kind(&Type::FLOAT8), PgExportTypeKind::Float);
        assert_eq!(pg_export_type_kind(&Type::BOOL), PgExportTypeKind::Boolean);
    }

    #[test]
    fn pg_type_to_arrow_unknown_fallback() {
        assert_eq!(pg_type_to_arrow(&Type::MONEY), DataType::Utf8);
    }

    #[test]
    fn native_type_names_match_format_type_spelling() {
        let cases: [(Type, &str); 18] = [
            (Type::INT2, "smallint"),
            (Type::INT4, "integer"),
            (Type::INT8, "bigint"),
            (Type::FLOAT4, "real"),
            (Type::FLOAT8, "double precision"),
            (Type::BOOL, "boolean"),
            (Type::VARCHAR, "character varying"),
            (Type::BPCHAR, "character"),
            (Type::TIMESTAMP, "timestamp without time zone"),
            (Type::TIMESTAMPTZ, "timestamp with time zone"),
            (Type::TIME, "time without time zone"),
            (Type::TIMETZ, "time with time zone"),
            (Type::NUMERIC, "numeric"),
            (Type::TEXT, "text"),
            (Type::JSONB, "jsonb"),
            (Type::UUID, "uuid"),
            (Type::BYTEA, "bytea"),
            (Type::DATE, "date"),
        ];
        for (pg_type, expected) in cases {
            assert_eq!(pg_native_type_name(&pg_type), expected);
        }
        assert_eq!(pg_native_type_name(&Type::INTERVAL), "interval");
        assert_eq!(pg_native_type_name(&Type::JSON), "json");
    }

    #[test]
    fn native_type_names_cover_arrays_and_unknown_types() {
        assert_eq!(pg_native_type_name(&Type::INT4_ARRAY), "integer[]");
        assert_eq!(pg_native_type_name(&Type::TEXT_ARRAY), "text[]");
        assert_eq!(
            pg_native_type_name(&Type::TIMESTAMPTZ_ARRAY),
            "timestamp with time zone[]"
        );
        assert_eq!(pg_native_type_name(&Type::MONEY), "money");
        assert_eq!(pg_native_type_name(&Type::INET), "inet");
    }

    #[test]
    fn native_type_metadata_survives_batch_concat_slice_and_ffi() {
        use arrow::compute::concat_batches;
        use arrow_array::ffi::FFI_ArrowSchema;

        let fields = vec![
            native_type_field("id", DataType::Int64, &Type::INT8),
            native_type_field("at", DataType::Utf8, &Type::TIMESTAMPTZ),
        ];
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![Some(1), Some(2), Some(3)])),
                Arc::new(StringArray::from(vec![Some("a"), None, Some("c")])),
            ],
        )
        .unwrap();
        let key = PG_NATIVE_TYPE_METADATA_KEY;
        let native = |batch: &RecordBatch, column: usize| {
            batch.schema().field(column).metadata().get(key).cloned()
        };

        let concat = concat_batches(&batch.schema(), &[batch.clone(), batch.clone()]).unwrap();
        let sliced = batch.slice(1, 1);
        for candidate in [&batch, &concat, &sliced] {
            assert_eq!(native(candidate, 0).as_deref(), Some("bigint"));
            assert_eq!(
                native(candidate, 1).as_deref(),
                Some("timestamp with time zone")
            );
        }

        let ffi = FFI_ArrowSchema::try_from(batch.schema().as_ref()).unwrap();
        let imported = Schema::try_from(&ffi).unwrap();
        assert_eq!(
            imported.field(1).metadata().get(key).map(String::as_str),
            Some("timestamp with time zone")
        );
    }
}
