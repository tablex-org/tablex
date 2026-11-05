// Value correctness of what the app shows and writes: the type classification
// behind an edit, NULL as a flag, exact numbers, binary as hex, the JSON editor
// seeded with the stored text. The UI tests drive the real grid (double-click,
// editor, blur) and the real detail pane.

use results::column_type::{base_type_head, value_kind, ValueKind};
use results::editing::{MutationInputError, SqlApplyError};

#[test]
fn value_kind_classifies_the_base_type_not_a_word_inside_it() {
    use ValueKind::*;
    let table = [
        // Whole numbers of any width, wrappers and modifiers stripped.
        ("integer", Integer),
        ("int4", Integer),
        ("bigint unsigned", Integer),
        ("int(11) unsigned zerofill", Integer),
        ("tinyint(1)", Integer),
        ("Nullable(Int128)", Integer),
        ("LowCardinality(Nullable(UInt64))", Integer),
        ("UInt256", Integer),
        ("serial", Integer),
        ("UNSIGNED BIG INT", Integer),
        ("int64", Integer),
        // Exact decimals and floats.
        ("numeric", Decimal),
        ("numeric(10,2)", Decimal),
        ("decimal(10,2) unsigned", Decimal),
        ("Decimal128(4)", Decimal),
        ("float8", Float),
        ("double precision", Float),
        ("real", Float),
        ("number", Float),
        ("float", Float),
        // Booleans, dates and times.
        ("boolean", Boolean),
        ("BOOLEAN", Boolean),
        ("Bool", Boolean),
        ("timestamp(3) with time zone", Temporal),
        ("timestamptz", Temporal),
        ("DateTime('America/Montreal')", Temporal),
        ("DateTime64(3, 'UTC')", Temporal),
        ("date", Temporal),
        // Byte strings and JSON.
        ("blob", Binary),
        ("varbinary(16)", Binary),
        ("binary(16)", Binary),
        ("jsonb", Json),
        ("json", Json),
        // Never a number or boolean: a user type, an enum or a compound type
        // whose NAME merely contains one of those words.
        ("serial_number", Text),
        ("interval", Text),
        ("integer_set", Text),
        ("int4range", Text),
        ("point", Text),
        ("money", Text),
        ("uuid", Text),
        ("bytea", Text),
        ("integer[]", Text),
        ("int4[]", Text),
        ("numeric(10,2)[]", Text),
        ("Array(Int32)", Text),
        ("enum('internal','external')", Text),
        ("Enum8('internal' = 1)", Text),
        ("set('a','b')", Text),
        ("varchar(20)", Text),
        ("character varying", Text),
        ("String", Text),
        ("LowCardinality(String)", Text),
        ("FixedString(16)", Text),
        ("Tuple(Int32, String)", Text),
        ("public.serial_number", Text),
        ("", Text),
    ];
    for (type_name, expected) in table {
        assert_eq!(value_kind(type_name), expected, "{type_name:?}");
    }
    assert_eq!(base_type_head("Nullable(Decimal(10, 2)) "), "decimal");
    assert_eq!(base_type_head("int4[]"), "int4[]");
}

#[test]
fn value_numbers_are_checked_and_travel_as_the_exact_typed_text() {
    let number = |input: &str, ty: &str| mutation_value_from_input(input, ty);
    let exact = |text: &str| Ok(MutationValue::Number(text.into()));
    // Integers of any length, any spelling the server reads.
    assert_eq!(number("000123", "integer"), exact("000123"));
    assert_eq!(number("+5", "integer"), exact("+5"));
    assert_eq!(number(" -42 ", "int8"), exact("-42"));
    assert_eq!(number(&"9".repeat(40), "numeric"), exact(&"9".repeat(40)));
    assert_eq!(
        number(
            "170141183460469231731687303715884105727",
            "Nullable(Int128)"
        ),
        exact("170141183460469231731687303715884105727")
    );
    assert_eq!(
        number("18446744073709551615", "UInt64"),
        exact("18446744073709551615")
    );
    // Decimals: digits as typed, NaN and the infinities, never validated through f64.
    assert_eq!(number("12.50", "decimal(10,2) unsigned"), exact("12.50"));
    assert_eq!(number("NaN", "numeric"), exact("NaN"));
    assert_eq!(number("-Infinity", "numeric"), exact("-Infinity"));
    assert_eq!(
        number("12345678901234567890.123456789012345678", "numeric(40,18)"),
        exact("12345678901234567890.123456789012345678")
    );
    assert_eq!(number(".5", "numeric"), exact(".5"));
    // Floats add exponents and keep the sign of zero.
    assert_eq!(number("-0", "float8"), exact("-0"));
    assert_eq!(number("1e300", "double precision"), exact("1e300"));
    assert_eq!(number("2.5E-3", "real"), exact("2.5E-3"));
    assert_eq!(number("Infinity", "float4"), exact("Infinity"));
    // Wrong shapes are refused with the matching message.
    assert_eq!(number("1.5", "integer"), Err(MutationInputError::Integer));
    assert_eq!(number("abc", "bigint"), Err(MutationInputError::Integer));
    assert_eq!(number("1e5", "numeric"), Err(MutationInputError::Decimal));
    assert_eq!(number("1,5", "numeric"), Err(MutationInputError::Decimal));
    assert_eq!(number("1e", "float8"), Err(MutationInputError::Float));
    assert_eq!(number("0x10", "float8"), Err(MutationInputError::Float));
    for error in [
        MutationInputError::Integer,
        MutationInputError::Decimal,
        MutationInputError::Float,
        MutationInputError::Boolean,
        MutationInputError::Binary,
    ] {
        assert!(!error.to_string().is_empty());
    }
}

#[test]
fn value_booleans_accept_the_server_spellings() {
    for (input, expected) in [
        ("true", true),
        ("T", true),
        ("1", true),
        ("false", false),
        ("f", false),
        ("0", false),
    ] {
        for ty in ["boolean", "BOOLEAN", "bool"] {
            assert_eq!(
                mutation_value_from_input(input, ty),
                Ok(MutationValue::Boolean(expected))
            );
        }
    }
    assert_eq!(
        mutation_value_from_input("maybe", "boolean"),
        Err(MutationInputError::Boolean)
    );
    assert_eq!(
        mutation_value_from_input("2", "bool"),
        Err(MutationInputError::Boolean)
    );
}

#[test]
fn value_text_is_never_null_by_its_spelling() {
    for ty in [
        "varchar(20)",
        "text",
        "enum('a','NULL')",
        "String",
        "LowCardinality(String)",
        "uuid",
        "serial_number",
        "interval",
        "",
        "json",
        "bytea",
    ] {
        for spelling in ["NULL", "null", "Null", " null "] {
            assert_eq!(
                mutation_value_from_input(spelling, ty),
                Ok(MutationValue::Text(spelling.to_string())),
                "{ty:?} {spelling:?}"
            );
            assert_eq!(
                mutation_value_from_cell(spelling, ty),
                Ok(MutationValue::Text(spelling.to_string()))
            );
        }
    }
    // Spelled NULL in a numeric or boolean column is just a wrong number.
    assert_eq!(
        mutation_value_from_input("NULL", "integer"),
        Err(MutationInputError::Integer)
    );
    assert_eq!(
        mutation_value_from_input("null", "boolean"),
        Err(MutationInputError::Boolean)
    );
    // A stored cell is never NULL by its text either, not even an empty one.
    assert_eq!(
        mutation_value_from_cell("", "text"),
        Ok(MutationValue::Text(String::new()))
    );
    assert_eq!(
        mutation_value_from_cell("", "integer"),
        Err(MutationInputError::Integer)
    );
}

#[test]
fn value_an_empty_editor_is_null_only_in_numeric_boolean_and_date_columns() {
    for ty in [
        "integer",
        "numeric(10,2)",
        "float8",
        "boolean",
        "date",
        "timestamp with time zone",
        "DateTime('UTC')",
        "Nullable(Int64)",
    ] {
        assert_eq!(
            mutation_value_from_input("", ty),
            Ok(MutationValue::Null),
            "{ty}"
        );
        assert_eq!(
            mutation_value_from_input("  ", ty),
            Ok(MutationValue::Null),
            "{ty}"
        );
    }
    for ty in [
        "text",
        "varchar(5)",
        "String",
        "json",
        "uuid",
        "interval",
        "enum('a')",
    ] {
        assert_eq!(
            mutation_value_from_input("", ty),
            Ok(MutationValue::Text(String::new())),
            "{ty}"
        );
    }
    // A date is text for the server to coerce.
    assert_eq!(
        mutation_value_from_input("2024-02-29 12:00:00+02", "timestamptz"),
        Ok(MutationValue::Text("2024-02-29 12:00:00+02".into()))
    );
    // A SQLite BOOLEAN shows 1/0.
    assert_eq!(
        mutation_value_from_input("0", "BOOLEAN"),
        Ok(MutationValue::Boolean(false))
    );
}

#[test]
fn value_binary_input_is_zero_x_hex_or_refused() {
    for ok in ["0x", "0x00", "0xCAFEbabe", " 0x0a0B "] {
        assert_eq!(
            mutation_value_from_input(ok, "binary"),
            Ok(MutationValue::Text(ok.trim().into())),
            "{ok:?}"
        );
    }
    for bad in ["", "ab", "0xabc", "0xzz", "\\x0a", "X'0A'", "hello", "0X0a"] {
        assert_eq!(
            mutation_value_from_input(bad, "blob"),
            Err(MutationInputError::Binary),
            "{bad:?}"
        );
    }
    assert!(MutationInputError::Binary.to_string().contains("0x"));
    // A key of a binary column travels in the same form.
    assert_eq!(
        mutation_value_from_cell("0x0a0b0c0d", "binary(4)"),
        Ok(MutationValue::Text("0x0a0b0c0d".into()))
    );
}

#[test]
fn value_wire_json_keeps_every_number_the_user_typed() {
    let wire =
        |text: &str| results::editing::mutation_value_to_json(&MutationValue::Number(text.into()));
    assert_eq!(wire("42"), serde_json::json!(42));
    assert_eq!(wire("-7"), serde_json::json!(-7));
    assert_eq!(
        wire("18446744073709551615"),
        serde_json::json!(18446744073709551615u64)
    );
    // Every number that is not a whole number travels as its exact text.
    for text in [
        "1.5",
        "12345678.12345678",
        "000123",
        "+5",
        "-0",
        "1.50",
        "1e5",
        "NaN",
        "Infinity",
        "12345678901234567890.123456789",
        "170141183460469231731687303715884105727",
    ] {
        assert_eq!(wire(text), serde_json::json!(text), "{text}");
    }
}

#[test]
fn value_insert_json_keeps_literal_numbers_and_text_null() {
    let columns =
        ["amount", "qty", "note", "flag", "maybe"].map(|name| crate::catalog::CatalogColumn {
            name: name.into(),
            data_type: match name {
                "amount" => "numeric(40,9)",
                "qty" => "bigint",
                "note" => "varchar(20)",
                "flag" => "boolean",
                _ => "text",
            }
            .into(),
            nullable: name != "qty",
            ..Default::default()
        });
    let values = insert_values_from_json(
        r#"{"amount": 12345678901234567890.123456789, "qty": 7, "note": "null", "flag": true, "maybe": null}"#,
        &columns,
    )
    .expect("a valid draft");
    let value = |name: &str| {
        values
            .iter()
            .find(|(field, _)| field == name)
            .unwrap()
            .1
            .clone()
    };
    assert_eq!(
        value("amount"),
        MutationValue::Number("12345678901234567890.123456789".into())
    );
    assert_eq!(value("qty"), MutationValue::Number("7".into()));
    assert_eq!(
        value("note"),
        MutationValue::Text("null".into()),
        "text, not NULL"
    );
    assert_eq!(value("flag"), MutationValue::Boolean(true));
    assert_eq!(
        value("maybe"),
        MutationValue::Null,
        "only JSON null is NULL"
    );
    // The literal reaches the driver as its exact text.
    assert_eq!(
        results::editing::mutation_value_to_json(&value("amount")),
        serde_json::json!("12345678901234567890.123456789")
    );
    // A string value is a value of its column: wrong shapes say which column.
    let error = insert_values_from_json(r#"{"qty": "many"}"#, &columns).unwrap_err();
    assert!(error.to_string().starts_with("qty: "), "{error}");
    assert_eq!(
        insert_values_from_json(r#"{"qty": null}"#, &columns)
            .unwrap_err()
            .to_string(),
        "qty cannot be NULL"
    );
    assert_eq!(
        insert_values_from_json(r#"{"nope": 1}"#, &columns)
            .unwrap_err()
            .to_string(),
        "Unknown column: nope"
    );
    assert!(insert_values_from_json("[1]", &columns)
        .unwrap_err()
        .to_string()
        .starts_with("Insert values must be a JSON object"));
}

fn value_catalog(columns: &[(&str, &str, bool, bool)]) -> Vec<crate::catalog::CatalogColumn> {
    columns
        .iter()
        .map(
            |(name, data_type, nullable, primary_key)| crate::catalog::CatalogColumn {
                name: (*name).into(),
                data_type: (*data_type).into(),
                nullable: *nullable,
                primary_key: *primary_key,
                ..Default::default()
            },
        )
        .collect()
}

fn value_payload(column: &str, data_type: &str) -> InlineEditPayload {
    InlineEditPayload::Sql {
        primary_key: value_catalog(&[("id", "integer", false, true)]).remove(0),
        key_value: MutationValue::Number("1".into()),
        key: Default::default(),
        column_name: column.into(),
        data_type: data_type.into(),
    }
}

#[test]
fn value_null_is_a_flag_in_the_staging_logic() {
    let mut runtime = crate::workspace::TabRuntime::new();
    runtime.structure_columns = value_catalog(&[
        ("id", "integer", false, true),
        ("note", "varchar(20)", true, false),
        ("qty", "integer", true, false),
        ("name", "text", false, false),
    ]);
    let at = |column| CellPosition { row: 0, column };
    let stage = |runtime: &mut crate::workspace::TabRuntime,
                 column: usize,
                 payload: &InlineEditPayload,
                 original: (&str, bool),
                 text: &str| {
        try_stage_inline_sql_edit(runtime, at(column), payload, original, text)
    };
    let note = value_payload("note", "varchar(20)");

    // Typing `NULL` into a NULL cell is a change to the STRING `NULL`.
    assert_eq!(
        stage(&mut runtime, 1, &note, ("NULL", true), "NULL"),
        Ok(())
    );
    assert_eq!(runtime.pending_edits.len(), 1);
    assert_eq!(
        runtime.pending_edits[0].request.value,
        MutationValue::Text("NULL".into())
    );
    assert!(runtime.pending_edits[0].original_is_null);
    assert_eq!(
        runtime.pending_cell_text(at(1)).map(|text| text.as_ref()),
        Some("NULL")
    );
    // Typing it back to empty is the original NULL again: nothing is staged.
    assert_eq!(stage(&mut runtime, 1, &note, ("NULL", true), ""), Ok(()));
    assert_eq!(
        runtime.pending_edits.len(),
        1,
        "an empty varchar is the empty string, a change"
    );
    assert_eq!(
        runtime.pending_edits[0].request.value,
        MutationValue::Text(String::new())
    );

    // A stored string `NULL` typed over itself is no change; `null` is one.
    runtime.pending_edits.clear();
    runtime.clear_pending_cell_texts();
    assert_eq!(
        stage(&mut runtime, 1, &note, ("NULL", false), "NULL"),
        Ok(())
    );
    assert!(runtime.pending_edits.is_empty());
    assert_eq!(
        stage(&mut runtime, 1, &note, ("NULL", false), "null"),
        Ok(())
    );
    assert_eq!(
        runtime.pending_edits[0].request.value,
        MutationValue::Text("null".into())
    );

    // A numeric column: an empty editor is NULL, staged as NULL...
    runtime.pending_edits.clear();
    runtime.clear_pending_cell_texts();
    let qty = value_payload("qty", "integer");
    assert_eq!(stage(&mut runtime, 2, &qty, ("5", false), ""), Ok(()));
    assert_eq!(runtime.pending_edits[0].request.value, MutationValue::Null);
    assert_eq!(
        runtime.pending_cell_text(at(2)).map(|text| text.as_ref()),
        Some("NULL")
    );
    // ...and committing it over an original NULL changes nothing.
    runtime.pending_edits.clear();
    runtime.clear_pending_cell_texts();
    assert_eq!(stage(&mut runtime, 2, &qty, ("NULL", true), ""), Ok(()));
    assert!(runtime.pending_edits.is_empty());
    // The spelling `NULL` is a refused number, with a message.
    assert_eq!(
        stage(&mut runtime, 2, &qty, ("5", false), "NULL"),
        Err(SqlEditError::InvalidInput(MutationInputError::Integer))
    );
    // A NOT NULL column refuses the empty numeric editor, with a message.
    let mut strict = runtime.structure_columns.clone();
    strict[2].nullable = false;
    runtime.structure_columns = strict;
    let error = stage(&mut runtime, 2, &qty, ("5", false), "").unwrap_err();
    assert_eq!(error, SqlEditError::NullNotAllowed("qty".into()));
    assert_eq!(error.to_string(), "qty cannot be NULL");
    assert!(runtime.pending_edits.is_empty());
}

#[test]
fn value_binary_edits_are_refused_unless_hex() {
    let mut runtime = crate::workspace::TabRuntime::new();
    runtime.structure_columns = value_catalog(&[
        ("id", "integer", false, true),
        ("payload", "blob", true, false),
    ]);
    let payload = value_payload("payload", "binary");
    let at = CellPosition { row: 0, column: 1 };
    let error = try_stage_inline_sql_edit(&mut runtime, at, &payload, ("0x00", false), "hello")
        .unwrap_err();
    assert_eq!(
        error,
        SqlEditError::InvalidInput(MutationInputError::Binary)
    );
    assert!(error.to_string().contains("0x"));
    assert!(runtime.pending_edits.is_empty());
    assert_eq!(
        try_stage_inline_sql_edit(&mut runtime, at, &payload, ("0x00", false), "0xCAFE"),
        Ok(())
    );
    assert_eq!(
        runtime.pending_edits[0].request.value,
        MutationValue::Text("0xCAFE".into())
    );
    // Unchanged hex is no change.
    assert_eq!(
        try_stage_inline_sql_edit(&mut runtime, at, &payload, ("0x00", false), "0x00"),
        Ok(())
    );
    assert!(runtime.pending_edits.is_empty());
}

#[test]
fn value_json_is_formatted_without_re_encoding_a_token() {
    let stored = r#"{"id":12345678901234567890,"price":1.10,"e":1E2,"s":"\u00e9","k":1,"k":2}"#;
    let formatted = crate::cell_detail::format_json_value(stored, "jsonb").expect("json");
    for token in [
        "12345678901234567890",
        "1.10",
        "1E2",
        r#""\u00e9""#,
        "\"k\": 1",
        "\"k\": 2",
    ] {
        assert!(formatted.contains(token), "{token} in {formatted}");
    }
    assert!(formatted.starts_with("{\n  \"id\""));
    // Only whitespace differs from the stored text.
    let squeeze = |text: &str| {
        text.split_whitespace()
            .collect::<String>()
            .replace(": ", ":")
    };
    assert_eq!(squeeze(&formatted).replace(": ", ":"), squeeze(stored));
    // Plain text is not formatted; JSON above the limit is not either, and says so.
    assert!(crate::cell_detail::format_json_value("PAY", "text").is_none());
    let huge = format!(
        "[{}]",
        "1,".repeat(crate::cell_detail::MAX_JSON_FORMAT_LENGTH)
    );
    assert!(crate::cell_detail::format_json_value(&huge, "json").is_none());
    let detail = crate::cell_detail::build_cell_detail(
        CellPosition { row: 0, column: 0 },
        0,
        "doc",
        "jsonb",
        huge.clone(),
        false,
        true,
        true,
        "",
    );
    assert!(detail.json_unformatted && detail.formatted_json.is_none());
    assert_eq!(detail.value.len(), huge.len());
}

/// A result tab of an ad-hoc query with `columns` (name, catalog type) over
/// `batch`, ready to be inspected.
fn value_tab(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    columns: &[(&str, &str, bool, bool)],
    batch: arrow_array::RecordBatch,
) -> u64 {
    let catalog = value_catalog(columns);
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.open_ran_query_tab("values");
        this.workspace
            .runtime_mut(tab_id)
            .expect("runtime")
            .structure_columns = catalog;
        this.set_active_grid(GridState::new(GridContentSource::Arrow(
            ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        cx.notify();
        tab_id
    })
}

fn value_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = visual.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("AppRoot")
    });
    visual.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    visual.update(|window, _| window.activate_window());
    visual.run_until_parked();
    (app, visual)
}

fn value_strings(values: &[Option<&str>]) -> arrow_array::ArrayRef {
    std::sync::Arc::new(arrow_array::StringArray::from(values.to_vec()))
}

fn value_ints(values: &[i64]) -> arrow_array::ArrayRef {
    std::sync::Arc::new(arrow_array::Int64Array::from(values.to_vec()))
}

fn value_batch(columns: Vec<(&str, arrow_array::ArrayRef)>) -> arrow_array::RecordBatch {
    let fields: Vec<arrow_schema::Field> = columns
        .iter()
        .map(|(name, array)| arrow_schema::Field::new(*name, array.data_type().clone(), true))
        .collect();
    arrow_array::RecordBatch::try_new(
        std::sync::Arc::new(arrow_schema::Schema::new(fields)),
        columns.into_iter().map(|(_, array)| array).collect(),
    )
    .expect("equal-length columns")
}

#[test]
fn value_row_tab_shows_numerics_and_json_exactly() {
    // The Row tab's document: no SQL numeric, big integer or nested json number
    // goes through f64, and the JSON text is what is stored.
    let exact = row_json_cell("12345678901234567890.123456789", "numeric(40,9)", false);
    assert_eq!(exact.literal, "12345678901234567890.123456789");
    assert_eq!(
        exact.value,
        serde_json::json!("12345678901234567890.123456789")
    );
    let big = row_json_cell("9007199254740993", "bigint", false);
    assert_eq!(big.value, serde_json::json!(9007199254740993u64));
    assert_eq!(big.literal, "9007199254740993");
    assert_eq!(
        row_value_to_json("1.5", "float8", false),
        serde_json::json!(1.5)
    );
    assert_eq!(row_json_cell("1.10", "numeric", false).literal, "1.10");
    // Not numbers in JSON: quoted, never rounded.
    assert_eq!(row_json_cell("NaN", "float8", false).literal, "\"NaN\"");
    assert_eq!(row_json_cell("007", "integer", false).literal, "\"007\"");
    // A domain over varchar named like a number stays text; so does an interval.
    assert_eq!(
        row_json_cell("123", "serial_number", false).literal,
        "\"123\""
    );
    assert_eq!(
        row_json_cell("1 day", "interval", false).literal,
        "\"1 day\""
    );
    let nested = row_json_cell(r#"{"a": 1.10, "b": 12345678901234567890}"#, "jsonb", false);
    assert_eq!(nested.literal, r#"{"a": 1.10, "b": 12345678901234567890}"#);
    assert_eq!(nested.value["a"], serde_json::json!("1.10"));
    assert_eq!(row_json_cell("t", "boolean", false).literal, "true");
    assert_eq!(row_json_cell("anything", "text", true).literal, "null");
}

#[gpui::test]
fn value_row_tab_json_text_is_assembled_from_the_stored_cells(cx: &mut TestAppContext) {
    let (app, visual) = value_app(cx);
    let batch = value_batch(vec![
        ("id", value_ints(&[1])),
        (
            "amount",
            value_strings(&[Some("12345678901234567890.123456789")]),
        ),
        (
            "doc",
            value_strings(&[Some(r#"{"price":1.10,"n":12345678901234567890}"#)]),
        ),
        ("note", value_strings(&[None])),
    ]);
    let tab_id = value_tab(
        &app,
        visual,
        &[
            ("id", "integer", false, true),
            ("amount", "numeric(40,9)", false, false),
            ("doc", "jsonb", false, false),
            ("note", "text", true, false),
        ],
        batch,
    );
    let row = app.read_with(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap().clone();
        this.build_cell_detail_row(&tab, 0).expect("row")
    });
    assert!(
        row.json
            .contains("\"amount\": 12345678901234567890.123456789,"),
        "{}",
        row.json
    );
    assert!(row.json.contains("\"price\": 1.10"), "{}", row.json);
    assert!(
        row.json.contains("\"n\": 12345678901234567890"),
        "{}",
        row.json
    );
    assert!(row.json.contains("\"note\": null"), "{}", row.json);
    // The cell view and the row view agree digit for digit.
    assert_eq!(row.fields[1].value, "12345678901234567890.123456789");
}

#[gpui::test]
fn value_detail_editor_is_seeded_with_the_stored_json_text(cx: &mut TestAppContext) {
    let (app, visual) = value_app(cx);
    let stored = r#"{"id":12345678901234567890,"price":1.10,"e":1E2,"name":"x","k":1,"k":2}"#;
    let batch = value_batch(vec![
        ("id", value_ints(&[1])),
        ("doc", value_strings(&[Some(stored)])),
    ]);
    let tab_id = value_tab(
        &app,
        visual,
        &[
            ("id", "integer", false, true),
            ("doc", "jsonb", false, false),
        ],
        batch,
    );
    app.update_in(visual, |this, window, cx| {
        this.select_grid_cell(tab_id, CellPosition { row: 0, column: 1 });
        this.open_cell_detail_tab(tab_id, CellDetailTab::Details, cx);
        this.open_cell_detail_editor(tab_id, window, cx);
    });
    settle(visual);
    let (seed, formatted) = app.read_with(visual, |this, cx| {
        let edit = this
            .view(tab_id)
            .detail_ui
            .edit
            .as_ref()
            .expect("the editor opens");
        let detail = this.selected_cell_detail(tab_id).expect("detail");
        (
            edit.editor.value(cx).to_string(),
            detail.formatted_json.clone(),
        )
    });
    assert_eq!(
        seed, stored,
        "the editor holds the stored text, not a re-encoded copy"
    );
    assert!(formatted.expect("formatted view").contains("1.10"));

    // Committing it untouched stages nothing.
    app.update(visual, |this, cx| {
        this.commit_cell_detail_editor(tab_id, cx)
    });
    app.read_with(visual, |this, _| {
        assert!(this
            .workspace
            .runtime(tab_id)
            .unwrap()
            .pending_edits
            .is_empty());
    });

    // An unrelated edit leaves the big number, `1.10`, `1E2` and the repeated
    // key byte for byte as they were.
    app.update_in(visual, |this, window, cx| {
        this.open_cell_detail_editor(tab_id, window, cx)
    });
    settle(visual);
    let edited = stored.replace(r#""name":"x""#, r#""name":"y""#);
    app.update_in(visual, |this, window, cx| {
        let edit = this.view(tab_id).detail_ui.edit.as_ref().expect("editor");
        edit.editor.set_value(&edited, window, cx);
        this.commit_cell_detail_editor(tab_id, cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert_eq!(runtime.pending_edits.len(), 1);
        assert_eq!(
            runtime.pending_edits[0].request.value,
            MutationValue::Text(edited.clone())
        );
        assert!(runtime.pending_edits[0]
            .display_text
            .contains("12345678901234567890"));
        assert!(runtime.pending_edits[0].display_text.contains("1.10"));
    });
}

#[gpui::test]
fn value_copy_value_copies_the_whole_value_not_the_preview(cx: &mut TestAppContext) {
    let (app, visual) = value_app(cx);
    let long = "é".repeat(20_000);
    let batch = value_batch(vec![
        ("id", value_ints(&[1])),
        ("body", value_strings(&[Some(long.as_str())])),
    ]);
    let tab_id = value_tab(
        &app,
        visual,
        &[
            ("id", "integer", false, true),
            ("body", "text", true, false),
        ],
        batch,
    );
    app.update(visual, |this, cx| {
        this.select_grid_cell(tab_id, CellPosition { row: 0, column: 1 });
        this.open_cell_detail_tab(tab_id, CellDetailTab::Details, cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        let detail = this.selected_cell_detail(tab_id).expect("detail");
        assert!(
            detail.value_preview_truncated,
            "the pane shows a cut preview"
        );
        assert!(detail.value_preview.chars().count() < long.chars().count());
    });
    app.update(visual, |this, cx| this.copy_cell_detail_value(tab_id, cx));
    let copied = visual.read_from_clipboard().and_then(|item| item.text());
    assert_eq!(copied.as_deref(), Some(long.as_str()));
}

#[test]
fn value_clone_carries_values_from_the_null_flag_alone() {
    // The clone path converts a stored cell with `mutation_value_from_cell`: a
    // stored string `null` stays that string, binary stays hex, numbers stay exact.
    assert_eq!(
        mutation_value_from_cell("null", "varchar(10)"),
        Ok(MutationValue::Text("null".into()))
    );
    assert_eq!(
        mutation_value_from_cell("NULL", "Nullable(String)"),
        Ok(MutationValue::Text("NULL".into()))
    );
    assert_eq!(
        mutation_value_from_cell("0xdeadbeef", "binary"),
        Ok(MutationValue::Text("0xdeadbeef".into()))
    );
    assert_eq!(
        mutation_value_from_cell("12345678901234567890.123456789", "numeric"),
        Ok(MutationValue::Number(
            "12345678901234567890.123456789".into()
        ))
    );
}

fn value_orders_batch(rows: &[(i64, &str, Option<&str>, &str)]) -> arrow_array::RecordBatch {
    use arrow_array::{
        ArrayRef, BooleanArray, Date32Array, Float64Array, StringArray, UInt64Array,
    };
    let names: Vec<&str> = rows.iter().map(|row| row.1).collect();
    let statuses: Vec<Option<&str>> = rows.iter().map(|row| row.2).collect();
    let payloads: Vec<&str> = rows.iter().map(|row| row.3).collect();
    value_batch(vec![
        (
            "id",
            value_ints(&rows.iter().map(|row| row.0).collect::<Vec<_>>()),
        ),
        (
            "name",
            std::sync::Arc::new(StringArray::from(names)) as ArrayRef,
        ),
        (
            "status",
            std::sync::Arc::new(StringArray::from(statuses)) as ArrayRef,
        ),
        (
            "active",
            std::sync::Arc::new(BooleanArray::from(vec![true; rows.len()])) as ArrayRef,
        ),
        (
            "created",
            std::sync::Arc::new(Date32Array::from(vec![19_000; rows.len()])) as ArrayRef,
        ),
        (
            "score",
            std::sync::Arc::new(Float64Array::from(vec![1.5; rows.len()])) as ArrayRef,
        ),
        (
            "big_number",
            std::sync::Arc::new(UInt64Array::from(vec![7; rows.len()])) as ArrayRef,
        ),
        (
            "payload",
            std::sync::Arc::new(StringArray::from(payloads)) as ArrayRef,
        ),
    ])
}

/// The orders fixture's table tab with `rows` in its grid.
fn value_orders_app<'a>(
    cx: &'a mut TestAppContext,
    rows: &[(i64, &str, Option<&str>, &str)],
) -> (gpui::Entity<AppRoot>, &'a mut gpui::VisualTestContext, u64) {
    value_orders_batch_app(cx, value_orders_batch(rows))
}

/// The orders fixture's table tab with `batch` (shaped like the orders rows) in its grid.
fn value_orders_batch_app(
    cx: &mut TestAppContext,
    batch: arrow_array::RecordBatch,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    app.update(visual, |this, cx| {
        this.workspace.runtime_mut(tab_id).expect("runtime").grid =
            GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
                std::sync::Arc::new(batch),
            ])));
        this.reset_tab_view(tab_id);
        cx.notify();
    });
    settle(visual);
    (app, visual, tab_id)
}

fn value_double_click_cell(visual: &mut gpui::VisualTestContext, row: usize, column: usize) {
    let selector: &'static str =
        Box::leak(format!("grid-body-cell-{row}-{column}").into_boxed_str());
    let center = visual
        .debug_bounds(selector)
        .expect("the cell is painted")
        .center();
    double_click(visual, center);
    settle(visual);
}

fn value_type_in_editor(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    text: &str,
    exit: InlineEditExit,
) {
    let editor = app.read_with(visual, |this, _| {
        this.av()
            .grid_ui
            .inline_edit
            .as_ref()
            .expect("the inline editor is open")
            .editor
            .clone()
    });
    editor
        .as_input()
        .expect("a short value uses the single-line input")
        .update_in(visual, |state, window, cx| {
            state.set_value(text, window, cx)
        });
    app.update_in(visual, |this, window, cx| {
        let tab = this.active_id();
        this.commit_inline_edit(tab, exit, Some(window), cx);
    });
    settle(visual);
}

const VALUE_ORDERS: [(i64, &str, Option<&str>, &str); 2] = [
    (1, "ada", None, r#"{"a":1}"#),
    (2, "bob", Some("null"), r#"{"a":2}"#),
];

#[gpui::test]
fn value_editing_after_a_hidden_column_stages_the_column_the_user_edited(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = value_orders_app(cx, &VALUE_ORDERS);
    // Hide `name` (source 1): display column 1 is now `status` (source 2).
    app.update(visual, |this, cx| {
        this.toggle_sql_column(tab_id, "name".into(), cx)
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.source_column_for_display(tab_id, 1), Some(2));
        assert_eq!(this.grid(tab_id).column_name(2), Some("status"));
    });
    value_double_click_cell(visual, 1, 1);
    // The editor shows the cell the user double-clicked (`status` of row 1).
    let (position, seed) = app.read_with(visual, |this, cx| {
        let edit = this
            .av()
            .grid_ui
            .inline_edit
            .as_ref()
            .expect("the editor opens");
        (edit.position, edit.editor.value(cx).to_string())
    });
    assert_eq!(position, CellPosition { row: 1, column: 1 });
    assert_eq!(seed, "null");
    value_type_in_editor(&app, visual, "shipped", InlineEditExit::Submit);
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert_eq!(runtime.pending_edits.len(), 1);
        let edit = &runtime.pending_edits[0];
        assert_eq!(
            edit.request.column, "status",
            "the edited column, not `name`'s neighbour"
        );
        assert_eq!(edit.request.value, MutationValue::Text("shipped".into()));
        assert_eq!(edit.original_text.as_ref(), "null");
        assert_eq!((edit.row, edit.column), (1, 1));
        assert_eq!(edit.key.get("id"), Some(&serde_json::json!(2)));
    });
}

#[gpui::test]
fn value_a_null_cell_opens_an_empty_editor_and_the_text_null_can_be_typed(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = value_orders_app(cx, &VALUE_ORDERS);
    // Row 0 `status` is NULL (display column 2).
    value_double_click_cell(visual, 0, 2);
    let seed = app.read_with(visual, |this, cx| {
        this.av()
            .grid_ui
            .inline_edit
            .as_ref()
            .expect("editor")
            .editor
            .value(cx)
            .to_string()
    });
    assert_eq!(seed, "", "a NULL opens empty");
    // Committing it unchanged is a no-op.
    app.update_in(visual, |this, window, cx| {
        this.commit_inline_edit(tab_id, InlineEditExit::Submit, Some(window), cx);
    });
    app.read_with(visual, |this, _| {
        assert!(this
            .workspace
            .runtime(tab_id)
            .unwrap()
            .pending_edits
            .is_empty());
    });

    // The string NULL typed over a NULL stages the 4-character string.
    value_double_click_cell(visual, 0, 2);
    value_type_in_editor(&app, visual, "NULL", InlineEditExit::Submit);
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert_eq!(runtime.pending_edits.len(), 1);
        assert_eq!(
            runtime.pending_edits[0].request.value,
            MutationValue::Text("NULL".into())
        );
        // A staged string NULL is not a staged NULL, in the grid and in the pane.
        let cell = CellPosition { row: 0, column: 2 };
        assert!(!this.pending_is_null(tab_id, cell));
        assert_eq!(this.cell_edit_text(tab_id, cell).as_deref(), Some("NULL"));
    });

    // Set NULL (the explicit action) is the staged NULL: muted like a stored one.
    app.update_in(visual, |this, window, cx| {
        this.select_grid_cell(tab_id, CellPosition { row: 1, column: 2 });
        this.open_cell_detail_tab(tab_id, CellDetailTab::Details, cx);
        this.set_cell_detail_null(tab_id, window, cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        let cell = CellPosition { row: 1, column: 2 };
        assert!(this.pending_is_null(tab_id, cell));
        assert_eq!(
            this.cell_edit_text(tab_id, cell).as_deref(),
            Some(""),
            "a staged NULL opens empty"
        );
        assert!(this.selected_cell_detail(tab_id).expect("detail").is_null);
        let text_null = this.build_cell_detail(
            this.workspace.tab(tab_id).unwrap(),
            CellPosition { row: 0, column: 2 },
        );
        assert!(!text_null.expect("detail").is_null);
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert!(
            runtime.pending_edits[1].original_text.as_ref() == "null"
                && !runtime.pending_edits[1].original_is_null
        );
        assert_eq!(runtime.pending_edits[1].request.value, MutationValue::Null);
    });
}

#[gpui::test]
fn value_invalid_input_is_refused_with_a_message_and_not_dropped_silently(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = value_orders_app(cx, &VALUE_ORDERS);
    // `big_number` (display column 6) is a bigint.
    value_double_click_cell(visual, 0, 6);
    value_type_in_editor(&app, visual, "seven", InlineEditExit::Submit);
    app.read_with(visual, |this, _| {
        let edit = this
            .av()
            .grid_ui
            .inline_edit
            .as_ref()
            .expect("a refused submit keeps the editor");
        let message = edit.error.clone().expect("the refusal says why");
        assert!(message.contains("whole number"), "{message}");
        assert!(this
            .workspace
            .runtime(tab_id)
            .unwrap()
            .pending_edits
            .is_empty());
    });
    // Losing focus with the same bad text closes the editor but leaves the reason.
    app.update(visual, |this, cx| {
        this.commit_inline_edit(tab_id, InlineEditExit::FocusLoss, None, cx);
    });
    app.read_with(visual, |this, _| {
        assert!(this.av().grid_ui.inline_edit.is_none());
        let message = this
            .view(tab_id)
            .detail_ui
            .error
            .clone()
            .expect("the reason stays visible");
        assert!(message.contains("whole number"), "{message}");
    });
}

#[gpui::test]
fn value_clone_row_copies_null_ness_from_the_null_flag_only(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = value_orders_app(cx, &VALUE_ORDERS);
    let cloned = |visual: &mut gpui::VisualTestContext, row: usize| {
        app.read_with(visual, |this, _| {
            match this.prepare_sql_clone(tab_id, row) {
                Ok(CloneSqlRow::NeedsValues { request, .. } | CloneSqlRow::Ready(request)) => {
                    request.values
                }
                other => panic!("clone of row {row}: {other:?}"),
            }
        })
    };
    let status = |values: &[(String, MutationValue)]| {
        values
            .iter()
            .find(|(name, _)| name == "status")
            .expect("status")
            .1
            .clone()
    };
    // Row 0: a NULL status. Row 1: the stored string `null`.
    assert_eq!(status(&cloned(visual, 0)), MutationValue::Null);
    assert_eq!(
        status(&cloned(visual, 1)),
        MutationValue::Text("null".into())
    );
    // Numbers keep their exact text; the staged value of a pending edit wins.
    let values = cloned(visual, 1);
    let big = values
        .iter()
        .find(|(name, _)| name == "big_number")
        .unwrap();
    assert_eq!(big.1, MutationValue::Number("7".into()));
    app.update(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap().clone();
        let payload = this
            .cell_detail_payload(
                &tab,
                &this
                    .build_cell_detail(&tab, CellPosition { row: 1, column: 2 })
                    .unwrap(),
            )
            .expect("payload");
        assert_eq!(
            this.try_stage_sql_edit(
                tab_id,
                CellPosition { row: 1, column: 2 },
                &payload,
                "null",
                "NULL"
            ),
            Ok(())
        );
    });
    assert_eq!(
        status(&cloned(visual, 1)),
        MutationValue::Text("NULL".into())
    );
}

#[gpui::test]
fn value_keys_and_edits_of_binary_columns_use_the_hex_form(cx: &mut TestAppContext) {
    use arrow_array::{ArrayRef, BinaryArray};
    let (app, visual) = value_app(cx);
    let key: ArrayRef = std::sync::Arc::new(BinaryArray::from(vec![
        &b"\x0a\x0b\x0c\x0d"[..],
        &b"\xff"[..],
    ]));
    let batch = value_batch(vec![
        ("id", key),
        ("note", value_strings(&[Some("a"), Some("b")])),
    ]);
    let tab_id = value_tab(
        &app,
        visual,
        &[
            ("id", "binary(4)", false, true),
            ("note", "text", true, false),
        ],
        batch,
    );
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.grid(tab_id)
                .cell_text(CellPosition { row: 0, column: 0 })
                .as_deref(),
            Some("0x0a0b0c0d")
        );
        // The row's key travels as the same `0x` hex text the cell shows.
        let key = this
            .sql_row_key(tab_id, 0)
            .expect("a binary key is accepted");
        assert_eq!(key.get("id"), Some(&serde_json::json!("0x0a0b0c0d")));
        assert_eq!(
            this.sql_row_key(tab_id, 1).unwrap().get("id"),
            Some(&serde_json::json!("0xff"))
        );
        // Binary columns of the result are classified as binary, whatever the
        // catalog calls them; a text column of the same result is not.
        assert_eq!(this.sql_edit_data_type(tab_id, 0, 0), "binary(4)");
        assert_eq!(this.sql_edit_data_type(tab_id, 1, 0), "text");
    });
}

#[gpui::test]
fn value_a_binary_result_column_is_binary_even_when_the_catalog_names_another_type(
    cx: &mut TestAppContext,
) {
    use arrow_array::{ArrayRef, BinaryArray};
    let (app, visual) = value_app(cx);
    let blobs: ArrayRef = std::sync::Arc::new(BinaryArray::from(vec![&b"\x01"[..]]));
    let batch = value_batch(vec![("id", value_ints(&[1])), ("data", blobs)]);
    let tab_id = value_tab(
        &app,
        visual,
        &[
            ("id", "integer", false, true),
            ("data", "varchar", true, false),
        ],
        batch,
    );
    app.read_with(visual, |this, _| {
        assert_eq!(this.sql_edit_data_type(tab_id, 1, 0), "binary");
        assert_eq!(
            mutation_value_from_input("hello", &this.sql_edit_data_type(tab_id, 1, 0)),
            Err(MutationInputError::Binary)
        );
    });
}

#[gpui::test]
fn value_mixed_column_cells_are_edited_by_their_own_class(cx: &mut TestAppContext) {
    let (app, visual) = value_app(cx);
    let batch = value_batch(vec![
        ("id", value_ints(&[1, 2, 3, 4, 5, 6])),
        ("v", crate::grid_arrow::tests::mixed_column()),
    ]);
    let tab_id = value_tab(
        &app,
        visual,
        &[
            ("id", "integer", false, true),
            ("v", "integer", true, false),
        ],
        batch,
    );
    app.read_with(visual, |this, _| {
        let kind = |row| this.sql_edit_data_type(tab_id, 1, row);
        assert_eq!(kind(0), "int64");
        assert_eq!(
            kind(1),
            "text",
            "a TEXT cell stays text although it looks numeric"
        );
        assert_eq!(kind(2), "float");
        assert_eq!(kind(3), "binary");
        // The TEXT '1' is sent as a string; the INTEGER 1 as a number.
        let wire = |row, input: &str| {
            results::editing::mutation_value_to_json(
                &mutation_value_from_input(input, &kind(row)).expect("valid"),
            )
        };
        assert_eq!(wire(0, "2"), serde_json::json!(2));
        assert_eq!(wire(1, "2"), serde_json::json!("2"));
        assert_eq!(wire(2, "1.5"), serde_json::json!("1.5"));
        assert_eq!(wire(2, "-inf"), serde_json::json!("-inf"));
        assert_eq!(
            mutation_value_from_input("text", &kind(3)),
            Err(MutationInputError::Binary),
            "a BLOB takes only 0x hex"
        );
        // Keys of a mixed column follow the cell's class too.
        assert_eq!(
            this.sql_cell_read_only_reason(tab_id, 5, 1),
            Some(results::editing::INVALID_UTF8_REASON)
        );
        assert_eq!(this.sql_cell_read_only_reason(tab_id, 1, 1), None);
    });
    // The pane names the class of the selected cell.
    app.update(visual, |this, cx| {
        this.select_grid_cell(tab_id, CellPosition { row: 1, column: 1 });
        this.open_cell_detail_tab(tab_id, CellDetailTab::Details, cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.selected_cell_detail(tab_id).unwrap().cell_class,
            Some("TEXT")
        );
    });
    assert!(visual.debug_bounds("cell-detail-cell-class").is_some());
    // Text that is not valid UTF-8 opens no editor and says why.
    app.update_in(visual, |this, window, cx| {
        this.begin_inline_edit(tab_id, 5, 1, window, cx)
    });
    app.read_with(visual, |this, _| {
        assert!(this.av().grid_ui.inline_edit.is_none());
        assert_eq!(
            this.view(tab_id).detail_ui.error.as_deref(),
            Some(results::editing::INVALID_UTF8_REASON)
        );
    });
}

/// `batch` with the native type label `native` on each `(column, native)`.
fn value_with_native_types(
    batch: &arrow_array::RecordBatch,
    native: &[(usize, &str)],
) -> arrow_array::RecordBatch {
    let fields: Vec<arrow_schema::Field> = batch
        .schema()
        .fields()
        .iter()
        .enumerate()
        .map(
            |(index, field)| match native.iter().find(|(column, _)| *column == index) {
                Some((_, label)) => {
                    field
                        .as_ref()
                        .clone()
                        .with_metadata(std::collections::HashMap::from([(
                            crate::grid_arrow::NATIVE_TYPE_METADATA_KEY.to_string(),
                            (*label).to_string(),
                        )]))
                }
                None => field.as_ref().clone(),
            },
        )
        .collect();
    arrow_array::RecordBatch::try_new(
        std::sync::Arc::new(arrow_schema::Schema::new(fields)),
        batch.columns().to_vec(),
    )
    .expect("same columns")
}

#[gpui::test]
fn value_a_sorted_page_reads_each_mixed_cell_by_the_class_of_the_row_it_shows(
    cx: &mut TestAppContext,
) {
    use crate::grid_arrow::CellClass;
    // `status` (source column 2) is the SQLite mixed column: rows 0..6 are
    // INTEGER 1, TEXT '1', REAL 2.5, BLOB, NULL, text that is not UTF-8.
    let rows: Vec<(i64, &str, Option<&str>, &str)> =
        (1..=6).map(|id| (id, "n", None, "{}")).collect();
    let base = value_orders_batch(&rows);
    let mut columns = base.columns().to_vec();
    columns[2] = crate::grid_arrow::tests::mixed_column();
    let fields: Vec<arrow_schema::Field> = base
        .schema()
        .fields()
        .iter()
        .zip(&columns)
        .map(|(field, column)| {
            arrow_schema::Field::new(field.name(), column.data_type().clone(), true)
        })
        .collect();
    let batch = arrow_array::RecordBatch::try_new(
        std::sync::Arc::new(arrow_schema::Schema::new(fields)),
        columns,
    )
    .unwrap();
    let (app, visual, tab_id) = value_orders_batch_app(cx, batch);
    // Descending by id: display row d shows source row 5 - d, so no display
    // row sits on the class of its own source row.
    app.update(visual, |this, _| {
        this.workspace
            .runtime_mut(tab_id)
            .unwrap()
            .grid
            .sort_page(0, crate::grid::SortKind::Integer, true)
            .unwrap();
    });
    let shown = |display_row: usize| CellPosition {
        row: display_row,
        column: 2,
    };
    app.read_with(visual, |this, _| {
        let texts: Vec<_> = (0..6)
            .map(|row| this.grid(tab_id).cell_text(shown(row)).unwrap().to_string())
            .collect();
        assert_eq!(texts, ["0xfffe", "NULL", "0x01ff", "2.5", "1", "1"]);
        let classes: Vec<_> = (0..6).map(|row| this.cell_class(tab_id, row, 2)).collect();
        assert_eq!(
            classes,
            [
                Some(CellClass::InvalidUtf8Text),
                None,
                Some(CellClass::Blob),
                Some(CellClass::Real),
                Some(CellClass::Text),
                Some(CellClass::Integer),
            ]
        );
        // Edit type, by display row.
        let kinds: Vec<_> = [5, 4, 3, 2]
            .map(|row| this.sql_edit_data_type(tab_id, 2, row))
            .to_vec();
        assert_eq!(kinds, ["int64", "text", "float", "binary"]);
        // Only the row that shows the non-UTF-8 text is read-only.
        assert_eq!(
            this.sql_cell_read_only_reason(tab_id, 0, 2),
            Some(results::editing::INVALID_UTF8_REASON)
        );
        assert_eq!(this.sql_cell_read_only_reason(tab_id, 5, 2), None);
        // The detail pane names the class of the cell on screen.
        let tab = this.workspace.tab(tab_id).unwrap();
        let class = |row| this.build_cell_detail(tab, shown(row)).unwrap().cell_class;
        assert_eq!(
            (class(5), class(4), class(0)),
            (Some("INTEGER"), Some("TEXT"), Some("TEXT (invalid UTF-8)"))
        );
    });
    // Editing: the INTEGER the sorted page shows last is staged as a number,
    // the TEXT '1' above it as a string.
    app.update(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap().clone();
        for (row, original) in [(5, "1"), (4, "1")] {
            let payload = this
                .cell_detail_payload(&tab, &this.build_cell_detail(&tab, shown(row)).unwrap())
                .expect("payload");
            this.try_stage_sql_edit(tab_id, shown(row), &payload, original, "2")
                .expect("staged");
        }
        let edits = &this.workspace.runtime(tab_id).unwrap().pending_edits;
        let staged = |row| {
            edits
                .iter()
                .find(|edit| edit.row == row)
                .unwrap()
                .request
                .value
                .clone()
        };
        assert_eq!(staged(5), MutationValue::Number("2".into()));
        assert_eq!(staged(4), MutationValue::Text("2".into()));
    });
    // The non-UTF-8 cell opens no editor (it would have opened one on the row
    // that holds an INTEGER in the source order).
    app.update_in(visual, |this, window, cx| {
        this.begin_inline_edit(tab_id, 0, 2, window, cx)
    });
    app.read_with(visual, |this, _| {
        assert!(this.av().grid_ui.inline_edit.is_none());
        assert_eq!(
            this.view(tab_id).detail_ui.error.as_deref(),
            Some(results::editing::INVALID_UTF8_REASON)
        );
    });
    // Clone: the cloned INTEGER is a number, the TEXT a string, the non-UTF-8
    // row is refused.
    app.update(visual, |this, _| {
        let runtime = this.workspace.runtime_mut(tab_id).unwrap();
        runtime.pending_edits.clear();
        runtime.clear_pending_cell_texts();
        let status = |row: usize| match this.prepare_sql_clone(tab_id, row) {
            Ok(CloneSqlRow::NeedsValues { request, .. } | CloneSqlRow::Ready(request)) => {
                Ok(request
                    .values
                    .into_iter()
                    .find(|(name, _)| name == "status")
                    .expect("status")
                    .1)
            }
            Err(message) => Err(message),
        };
        assert_eq!(status(5), Ok(MutationValue::Number("1".into())));
        assert_eq!(status(4), Ok(MutationValue::Text("1".into())));
        assert_eq!(status(3), Ok(MutationValue::Number("2.5".into())));
        assert_eq!(status(1), Ok(MutationValue::Null));
        assert!(
            status(0)
                .unwrap_err()
                .contains(results::editing::INVALID_UTF8_REASON),
            "{:?}",
            status(0)
        );
    });
    // Copy: SQL text follows the class of the displayed cell.
    app.read_with(visual, |this, _| {
        let sql = |row: usize| {
            let range = crate::grid::CellRange {
                start: shown(row),
                end: shown(row),
            };
            let data = this.collect_copy_context_range(tab_id, range).unwrap();
            data.rows[0][0].sql(data.target).unwrap()
        };
        assert_eq!(sql(5), "1");
        assert_eq!(sql(4), "'1'");
        assert_eq!(sql(3), "2.5");
    });
    // Keys: with the mixed column as the primary key, each displayed row keys
    // by its own class, and the non-UTF-8 one cannot address a row.
    app.update(visual, |this, _| {
        for column in &mut this
            .workspace
            .runtime_mut(tab_id)
            .unwrap()
            .structure_columns
        {
            column.primary_key = column.name == "status";
        }
    });
    app.read_with(visual, |this, _| {
        let key = |row: usize| {
            this.sql_row_key(tab_id, row)
                .map(|key| key["status"].clone())
        };
        assert_eq!(key(5), Ok(serde_json::json!(1)));
        assert_eq!(key(4), Ok(serde_json::json!("1")));
        assert_eq!(key(2), Ok(serde_json::json!("0x01ff")));
        assert_eq!(
            key(0),
            Err(SqlEditError::KeyNotWritable(
                results::editing::INVALID_UTF8_REASON
            ))
        );
    });
}

#[gpui::test]
fn value_server_text_and_clickhouse_composite_columns_are_read_only_up_front(
    cx: &mut TestAppContext,
) {
    use results::editing::{CLICKHOUSE_COMPOSITE_REASON, SERVER_TEXT_REASON};
    // name = a JSON column the driver delivers as server text, status = an
    // aggregate (NULL in row 0), payload = a native Array; big_number's
    // catalog type is a Map.
    let batch = value_with_native_types(
        &value_orders_batch(&VALUE_ORDERS),
        &[
            (1, "json (read-only)"),
            (2, "aggregatefunction(sum, uint64) (read-only)"),
            (7, "array"),
        ],
    );
    let (app, visual, tab_id) = value_orders_batch_app(cx, batch);
    app.update(visual, |this, _| {
        // A new catalog allocation, as a refreshed structure is (the header
        // type cache is keyed by it).
        let runtime = this.workspace.runtime_mut(tab_id).unwrap();
        let mut catalog = runtime.structure_columns.clone();
        for column in &mut catalog {
            if column.name == "big_number" {
                column.data_type = "Map(String, Int32)".into();
            }
        }
        runtime.structure_columns = catalog;
    });
    app.read_with(visual, |this, _| {
        // The `(read-only)` suffix counts on any engine; the composite kinds
        // only on ClickHouse (a PostgreSQL array or json column is editable).
        let reasons = |this: &AppRoot| -> Vec<_> {
            [0, 1, 2, 5, 6, 7]
                .map(|column| this.sql_column_read_only_reason(tab_id, column))
                .to_vec()
        };
        assert_eq!(
            reasons(this),
            [
                None,
                Some(SERVER_TEXT_REASON),
                Some(SERVER_TEXT_REASON),
                None,
                None,
                None
            ]
        );
    });
    app.update(visual, |this, _| {
        let connection = this.workspace.tab(tab_id).unwrap().connection_id.clone();
        for saved in &mut this.saved_connections {
            if saved.matches_id(&connection) {
                saved.kind = ConnectionKind::Clickhouse;
            }
        }
    });
    app.read_with(visual, |this, _| {
        let reasons: Vec<_> = [0, 1, 2, 5, 6, 7]
            .map(|column| this.sql_column_read_only_reason(tab_id, column))
            .to_vec();
        assert_eq!(
            reasons,
            [
                None,
                Some(SERVER_TEXT_REASON),
                Some(SERVER_TEXT_REASON),
                None,
                Some(CLICKHOUSE_COMPOSITE_REASON),
                Some(CLICKHOUSE_COMPOSITE_REASON),
            ]
        );
        // Key use.
        assert_eq!(
            this.key_cell_value(tab_id, 0, 7, "[1]", "text"),
            Err(SqlEditError::KeyNotWritable(CLICKHOUSE_COMPOSITE_REASON))
        );
        assert!(this.key_cell_value(tab_id, 0, 0, "1", "bigint").is_ok());
        // The detail pane and Set NULL / bulk edit: not editable, with the reason.
        let tab = this.workspace.tab(tab_id).unwrap();
        for (column, reason) in [
            (1, SERVER_TEXT_REASON),
            (2, SERVER_TEXT_REASON),
            (7, CLICKHOUSE_COMPOSITE_REASON),
        ] {
            let detail = this
                .build_cell_detail(tab, CellPosition { row: 0, column })
                .unwrap();
            assert!(!detail.editable, "column {column}");
            assert_eq!(detail.read_only_reason, Some(reason), "column {column}");
        }
        // Clone: the first read-only column names itself.
        let error = this.prepare_sql_clone(tab_id, 0).unwrap_err();
        assert!(
            error.starts_with("name: ") && error.contains(SERVER_TEXT_REASON),
            "{error}"
        );
    });
    // Set NULL on the selected cell is refused before anything is staged.
    for column in [1, 2, 7] {
        app.update(visual, |this, _| {
            this.select_grid_cell(tab_id, CellPosition { row: 0, column })
        });
        app.read_with(visual, |this, _| {
            let Err(error) = this.prepare_context_edits(tab_id, None) else {
                panic!("column {column} must not be editable")
            };
            assert!(
                error.ends_with("cannot be edited"),
                "column {column}: {error}"
            );
        });
    }
    // The editor: refused for a non-null cell and for the NULL cell of the aggregate.
    for (row, column, reason) in [
        (0, 1, SERVER_TEXT_REASON),
        (0, 2, SERVER_TEXT_REASON),
        (1, 7, CLICKHOUSE_COMPOSITE_REASON),
    ] {
        app.update(visual, |this, _| {
            this.view_mut(tab_id).detail_ui.error = None
        });
        app.update_in(visual, |this, window, cx| {
            this.begin_inline_edit(tab_id, row, column, window, cx)
        });
        app.read_with(visual, |this, _| {
            assert!(this.av().grid_ui.inline_edit.is_none(), "({row}, {column})");
            assert_eq!(this.view(tab_id).detail_ui.error.as_deref(), Some(reason));
        });
    }
    assert!(app.read_with(visual, |this, _| this
        .workspace
        .runtime(tab_id)
        .unwrap()
        .pending_edits
        .is_empty()));
}

#[gpui::test]
fn value_a_binary_column_labelled_invalid_utf8_is_read_only(cx: &mut TestAppContext) {
    use arrow_array::{ArrayRef, BinaryArray};
    let (app, visual) = value_app(cx);
    let bytes: ArrayRef = std::sync::Arc::new(BinaryArray::from(vec![&b"\xff\xfe"[..]]));
    let mut fields = vec![
        arrow_schema::Field::new("id", arrow_schema::DataType::Int64, false),
        arrow_schema::Field::new("note", arrow_schema::DataType::Binary, true).with_metadata(
            std::collections::HashMap::from([(
                crate::grid_arrow::NATIVE_TYPE_METADATA_KEY.to_string(),
                "text (invalid utf-8)".to_string(),
            )]),
        ),
    ];
    let batch = arrow_array::RecordBatch::try_new(
        std::sync::Arc::new(arrow_schema::Schema::new(std::mem::take(&mut fields))),
        vec![value_ints(&[1]), bytes],
    )
    .unwrap();
    let tab_id = value_tab(
        &app,
        visual,
        &[
            ("id", "integer", false, true),
            ("note", "text", true, false),
        ],
        batch,
    );
    app.update_in(visual, |this, window, cx| {
        assert_eq!(
            this.sql_cell_read_only_reason(tab_id, 0, 1),
            Some(results::editing::INVALID_UTF8_REASON)
        );
        this.select_grid_cell(tab_id, CellPosition { row: 0, column: 1 });
        this.open_cell_detail_tab(tab_id, CellDetailTab::Details, cx);
        this.begin_inline_edit(tab_id, 0, 1, window, cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(this.av().grid_ui.inline_edit.is_none());
        let detail = this.selected_cell_detail(tab_id).unwrap();
        assert!(!detail.editable);
        assert_eq!(
            detail.read_only_reason,
            Some(results::editing::INVALID_UTF8_REASON)
        );
        // Clone refuses the row with the reason, never sending `0x…`.
    });
}

fn value_rejected(code: &str, message: &str) -> DriverQueryError {
    DriverQueryError::Driver(engine_driver_host::LoaderError::DriverRejected {
        code: code.into(),
        message: message.into(),
        retryable: false,
    })
}

#[gpui::test]
fn value_a_partially_applied_batch_keeps_only_the_unapplied_changes_staged(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = value_orders_app(cx, &VALUE_ORDERS);
    // Three edits on two rows: the first row's two edits are ONE update (change 0).
    app.update(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap().clone();
        for (row, column, original, text) in [
            (0, 2, "NULL", "a"),
            (1, 2, "null", "b"),
            (0, 1, "ada", "ADA"),
        ] {
            let position = CellPosition { row, column };
            let payload = this
                .cell_detail_payload(&tab, &this.build_cell_detail(&tab, position).unwrap())
                .expect("payload");
            this.try_stage_sql_edit(tab_id, position, &payload, original, text)
                .expect("staged");
        }
        assert_eq!(
            this.workspace.runtime(tab_id).unwrap().pending_edits.len(),
            3
        );
    });
    let submitted = app.read_with(visual, |this, _| {
        this.workspace
            .runtime(tab_id)
            .unwrap()
            .pending_edits
            .clone()
    });
    let error = SqlApplyError::from(value_rejected(
        "mutation.partially_applied",
        "batch stopped part way; applied: [0]; failed: [1]; not attempted: []",
    ));
    let SqlApplyError::PartiallyApplied(partial) = &error else {
        panic!("a partially_applied rejection is recognised: {error:?}");
    };
    assert_eq!(
        (
            &partial.applied[..],
            &partial.failed[..],
            &partial.not_attempted[..]
        ),
        (&[0][..], &[1][..], &[][..])
    );
    app.update(visual, |this, cx| {
        this.workspace
            .runtime_mut(tab_id)
            .unwrap()
            .sql_apply_running = true;
        this.finish_sql_apply(tab_id, &submitted, &[], Err(error), cx);
    });
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert!(!runtime.sql_apply_running);
        // Row 0's two edits (change 0) are applied and gone; row 1's stays.
        assert_eq!(runtime.pending_edits.len(), 1);
        assert_eq!(
            (
                runtime.pending_edits[0].row,
                runtime.pending_edits[0].request.column.as_str()
            ),
            (1, "status")
        );
        assert!(runtime
            .pending_cell_text(CellPosition { row: 0, column: 2 })
            .is_none());
        assert!(runtime
            .pending_cell_text(CellPosition { row: 1, column: 2 })
            .is_some());
        let message = this
            .view(tab_id)
            .detail_ui
            .error
            .clone()
            .expect("says what happened");
        assert!(
            message.contains("Not rolled back") && message.contains("1 failed"),
            "{message}"
        );
    });
    // Everything applied: the queue empties.
    let submitted = app.read_with(visual, |this, _| {
        this.workspace
            .runtime(tab_id)
            .unwrap()
            .pending_edits
            .clone()
    });
    let all = SqlApplyError::from(value_rejected(
        "mutation.partially_applied",
        "applied: [0, 1]; failed: []; not attempted: []",
    ));
    app.update(visual, |this, cx| {
        this.finish_sql_apply(tab_id, &submitted, &[], Err(all), cx);
    });
    app.read_with(visual, |this, _| {
        assert!(this
            .workspace
            .runtime(tab_id)
            .unwrap()
            .pending_edits
            .is_empty());
    });
    // A message that cannot be read is an ordinary failure.
    assert!(matches!(
        SqlApplyError::from(value_rejected("mutation.partially_applied", "garbled")),
        SqlApplyError::Failed(_)
    ));
}

#[gpui::test]
fn value_driver_value_refusals_read_in_plain_words_and_keep_the_changes_staged(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = value_orders_app(cx, &VALUE_ORDERS);
    app.update(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap().clone();
        let position = CellPosition { row: 0, column: 2 };
        let payload = this
            .cell_detail_payload(&tab, &this.build_cell_detail(&tab, position).unwrap())
            .expect("payload");
        this.try_stage_sql_edit(tab_id, position, &payload, "NULL", "x")
            .expect("staged");
    });
    for (code, expected) in [
        ("mutation.invalid_value", "cannot be written to its column"),
        ("mutation.value_not_editable", "cannot be edited"),
    ] {
        let submitted = app.read_with(visual, |this, _| {
            this.workspace
                .runtime(tab_id)
                .unwrap()
                .pending_edits
                .clone()
        });
        let error = SqlApplyError::from(value_rejected(code, "BIT takes an unsigned integer"));
        app.update(visual, |this, cx| {
            this.finish_sql_apply(tab_id, &submitted, &[], Err(error), cx);
        });
        app.read_with(visual, |this, _| {
            let message = this
                .view(tab_id)
                .detail_ui
                .error
                .clone()
                .expect("a message");
            assert!(
                message.contains(expected) && message.contains("BIT takes"),
                "{message}"
            );
            assert!(!message.contains("driver rejected"), "{message}");
            assert_eq!(
                this.workspace.runtime(tab_id).unwrap().pending_edits.len(),
                1,
                "stays staged"
            );
        });
    }
}
