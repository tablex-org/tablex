use arrow::datatypes::{DataType, Field, Schema};
use arrow_array::{
    ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray, UInt64Array,
};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::{Row, Value};
use std::sync::Arc;

pub fn arrow_type_for_mysql(type_name: &str) -> DataType {
    let upper = type_name.trim().to_ascii_uppercase();
    match upper.as_str() {
        value
            if value.starts_with("TINYINT(1)")
                || (value.contains("TINY") && value.contains('1')) =>
        {
            DataType::Boolean
        }
        value
            if value.starts_with("TINYINT")
                || value.starts_with("SMALLINT")
                || value.starts_with("MEDIUMINT")
                || value.starts_with("INT")
                || value.starts_with("INTEGER")
                || value.starts_with("BIGINT")
                || value.contains("SHORT")
                || value.contains("LONG")
                || value.contains("INT24")
                || value.contains("LONGLONG") =>
        {
            DataType::Int64
        }
        value
            if value.starts_with("FLOAT")
                || value.starts_with("DOUBLE")
                || value.starts_with("REAL")
                || value.contains("FLOAT")
                || value.contains("DOUBLE") =>
        {
            DataType::Float64
        }
        _ => DataType::Utf8,
    }
}

pub fn arrow_type_for_mysql_column(column_type: ColumnType, flags: ColumnFlags) -> DataType {
    use ColumnType::*;
    match column_type {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_YEAR => DataType::Int64,
        MYSQL_TYPE_LONGLONG if flags.contains(ColumnFlags::UNSIGNED_FLAG) => DataType::UInt64,
        MYSQL_TYPE_LONGLONG => DataType::Int64,
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => DataType::Float64,
        _ => DataType::Utf8,
    }
}

// TODO(native-types): attach `crate::arrow::NATIVE_TYPE_METADATA_KEY` field
// metadata (as postgres_arrow/sqlite do) once MySQL/MariaDB is connectable.
pub fn rows_to_record_batch(rows: Vec<Row>) -> Result<RecordBatch, String> {
    if rows.is_empty() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "result",
            DataType::Utf8,
            true,
        )]));
        return RecordBatch::try_new(
            schema,
            vec![Arc::new(StringArray::from(Vec::<Option<String>>::new()))],
        )
        .map_err(|e| e.to_string());
    }

    let columns = rows[0].columns_ref().to_vec();
    let mut values: Vec<Vec<Value>> = columns
        .iter()
        .map(|_| Vec::with_capacity(rows.len()))
        .collect();
    for row in rows {
        for (index, value) in row.unwrap().into_iter().enumerate() {
            if let Some(column) = values.get_mut(index) {
                column.push(value);
            }
        }
    }

    let mut fields = Vec::with_capacity(columns.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());
    for (column, column_values) in columns.iter().zip(values.into_iter()) {
        let data_type = arrow_type_for_mysql_column(column.column_type(), column.flags());
        fields.push(Field::new(
            column.name_str().as_ref(),
            data_type.clone(),
            true,
        ));
        arrays.push(build_array(data_type, column_values));
    }

    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).map_err(|e| e.to_string())
}

fn build_array(data_type: DataType, values: Vec<Value>) -> ArrayRef {
    match data_type {
        DataType::Boolean => Arc::new(BooleanArray::from(
            values
                .into_iter()
                .map(|value| match value {
                    Value::NULL => None,
                    Value::Int(value) => Some(value != 0),
                    Value::UInt(value) => Some(value != 0),
                    Value::Bytes(value) => {
                        Some(value == b"1" || value.eq_ignore_ascii_case(b"true"))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        DataType::Int64 => Arc::new(Int64Array::from(
            values
                .into_iter()
                .map(|value| match value {
                    Value::NULL => None,
                    Value::Int(value) => Some(value),
                    Value::UInt(value) => i64::try_from(value).ok(),
                    Value::Bytes(value) => std::str::from_utf8(&value).ok()?.parse().ok(),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        DataType::UInt64 => Arc::new(UInt64Array::from(
            values
                .into_iter()
                .map(|value| match value {
                    Value::NULL => None,
                    Value::UInt(value) => Some(value),
                    Value::Int(value) => u64::try_from(value).ok(),
                    Value::Bytes(value) => std::str::from_utf8(&value).ok()?.parse().ok(),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        DataType::Float64 => Arc::new(Float64Array::from(
            values
                .into_iter()
                .map(|value| match value {
                    Value::NULL => None,
                    Value::Float(value) => Some(value as f64),
                    Value::Double(value) => Some(value),
                    Value::Int(value) => Some(value as f64),
                    Value::UInt(value) => Some(value as f64),
                    Value::Bytes(value) => std::str::from_utf8(&value).ok()?.parse().ok(),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        _ => Arc::new(StringArray::from(
            values
                .into_iter()
                .map(|value| mysql_value_to_string(&value))
                .collect::<Vec<_>>(),
        )),
    }
}

fn mysql_value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::NULL => None,
        Value::Bytes(value) => Some(String::from_utf8_lossy(value).into_owned()),
        Value::Int(value) => Some(value.to_string()),
        Value::UInt(value) => Some(value.to_string()),
        Value::Float(value) => Some(value.to_string()),
        Value::Double(value) => Some(value.to_string()),
        Value::Date(year, month, day, hour, minute, second, micros) => Some(format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}"
        )),
        Value::Time(neg, days, hours, minutes, seconds, micros) => {
            let sign = if *neg { "-" } else { "" };
            Some(format!(
                "{sign}{days} {hours:02}:{minutes:02}:{seconds:02}.{micros:06}"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{arrow_type_for_mysql, arrow_type_for_mysql_column};
    use arrow::datatypes::DataType;
    use mysql_async::consts::{ColumnFlags, ColumnType};

    #[test]
    fn maps_mysql_column_types_to_arrow() {
        assert_eq!(arrow_type_for_mysql("BIGINT"), DataType::Int64);
        assert_eq!(arrow_type_for_mysql("VARCHAR"), DataType::Utf8);
        assert_eq!(arrow_type_for_mysql("DOUBLE"), DataType::Float64);
        assert_eq!(arrow_type_for_mysql("TINYINT(1)"), DataType::Boolean);
    }

    #[test]
    fn maps_real_mysql_metadata_types_to_arrow() {
        assert_eq!(
            arrow_type_for_mysql_column(ColumnType::MYSQL_TYPE_TINY, ColumnFlags::empty()),
            DataType::Int64
        );
        assert_eq!(
            arrow_type_for_mysql_column(
                ColumnType::MYSQL_TYPE_LONGLONG,
                ColumnFlags::UNSIGNED_FLAG
            ),
            DataType::UInt64
        );
        assert_eq!(
            arrow_type_for_mysql_column(ColumnType::MYSQL_TYPE_NEWDECIMAL, ColumnFlags::empty()),
            DataType::Utf8
        );
    }
}
