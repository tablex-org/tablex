// Value correctness of what the app GENERATES and MEASURES: SQL text for the
// clipboard and files in the connection's own dialect, repeated column names,
// NULL against the text `NULL` in delimited output, the page sort, the
// selection statistics, the per-connection dialect and the rows a partial
// Apply committed. UI tests drive the real interaction.

use results::copy::{CopyCell, CopyData, DsvOptions, SqlTarget};

fn copy_postgres() -> SqlTarget {
    SqlTarget::POSTGRES
}

fn copy_mysql(backslash_escapes: bool) -> SqlTarget {
    SqlTarget {
        dialect: crate::sql_table::SqlDialect {
            backslash_escapes,
            ..crate::sql_table::SqlDialect::MYSQL
        },
        binary: crate::sql_table::BinaryLiteral::HexString,
    }
}

fn copy_sqlite() -> SqlTarget {
    SqlTarget {
        dialect: crate::sql_table::SqlDialect::SQLITE,
        binary: crate::sql_table::BinaryLiteral::HexString,
    }
}

fn copy_clickhouse() -> SqlTarget {
    SqlTarget {
        dialect: crate::sql_table::SqlDialect::MYSQL,
        binary: crate::sql_table::BinaryLiteral::ClickHouse,
    }
}

/// One row with a 30-digit numeric key, a string holding a backslash and a
/// quote, a column literally named `count`, a boolean, a binary value and a NULL.
fn engine_row(target: SqlTarget, postgres_bytea: bool) -> CopyData {
    let key = || CopyCell::new("12345678901234567890123456789", "numeric(30,0)", false);
    CopyData {
        columns: ["id", "name", "count", "flag", "payload", "note"]
            .map(String::from)
            .to_vec(),
        rows: vec![vec![
            key(),
            CopyCell::new("C:\\temp\\it's", "varchar(20)", false),
            CopyCell::new("0", "integer", false),
            CopyCell::new("true", "boolean", false),
            if postgres_bytea {
                CopyCell::new("\\x00ff10", "bytea", false)
            } else {
                CopyCell::new("0x00ff10", "binary", false)
            },
            CopyCell::new("NULL", "text", true),
        ]],
        table: "orders".into(),
        schema: Some("shop".into()),
        key_column: Some("id".into()),
        keys: vec![key()],
        target,
    }
}

#[test]
fn copy_sql_is_written_in_each_engines_own_syntax() {
    // (target, bytea shown as PostgreSQL does, identifier quote, name literal, binary literal)
    let engines: [(&str, SqlTarget, bool, char, &str, &str); 5] = [
        (
            "postgres",
            copy_postgres(),
            true,
            '"',
            r"'C:\temp\it''s'",
            "decode('00ff10', 'hex')",
        ),
        (
            "mysql",
            copy_mysql(true),
            false,
            '`',
            r"'C:\\temp\\it''s'",
            "X'00ff10'",
        ),
        // NO_BACKSLASH_ESCAPES: a backslash is an ordinary character.
        (
            "mysql-no-backslash",
            copy_mysql(false),
            false,
            '`',
            r"'C:\temp\it''s'",
            "X'00ff10'",
        ),
        (
            "sqlite",
            copy_sqlite(),
            false,
            '"',
            r"'C:\temp\it''s'",
            "X'00ff10'",
        ),
        (
            "clickhouse",
            copy_clickhouse(),
            false,
            '`',
            r"'C:\\temp\\it''s'",
            "unhex('00ff10')",
        ),
    ];
    for (engine, target, bytea, q, name, binary) in engines {
        let data = engine_row(target, bytea);
        let id = |column: &str| format!("{q}{column}{q}");
        let table = format!("{q}shop{q}.{q}orders{q}");
        let key = "12345678901234567890123456789";
        let literals = format!("{key}, {name}, 0, TRUE, {binary}, NULL");

        let insert = data.format(CopyFormat::SqlInsert).unwrap();
        let columns = ["id", "name", "count", "flag", "payload", "note"]
            .map(id)
            .join(", ");
        assert_eq!(
            insert,
            format!("INSERT INTO {table} ({columns}) VALUES\n({literals});"),
            "{engine}"
        );

        let update = data.format(CopyFormat::SqlUpdate).unwrap();
        assert_eq!(
            update,
            format!(
                "UPDATE {table} SET {} = {name}, {} = 0, {} = TRUE, {} = {binary}, {} = NULL WHERE {} = {key};",
                id("name"),
                id("count"),
                id("flag"),
                id("payload"),
                id("note"),
                id("id")
            ),
            "{engine}"
        );

        let condition = data.format(CopyFormat::SqlWhere).unwrap();
        assert_eq!(
            condition,
            format!(
                "({} = {key} AND {} = {name} AND {} = 0 AND {} = TRUE AND {} = {binary} AND {} IS NULL)",
                id("id"),
                id("name"),
                id("count"),
                id("flag"),
                id("payload"),
                id("note")
            ),
            "{engine}"
        );
        assert!(
            !insert.contains("e+") && !insert.contains("E+"),
            "{engine}: no float spelling"
        );
    }
}

#[test]
fn copy_sql_condition_quotes_the_column_for_the_connections_engine() {
    use results::copy::sql_condition;
    let zero = CopyCell::new("0", "integer", false);
    // The bug: MySQL read `"qty" = 0` as a string comparison, true for every row.
    assert_eq!(
        sql_condition(copy_postgres(), "qty", &zero).unwrap(),
        r#""qty" = 0"#
    );
    assert_eq!(
        sql_condition(copy_mysql(true), "qty", &zero).unwrap(),
        "`qty` = 0"
    );
    assert_eq!(
        sql_condition(copy_clickhouse(), "qty", &zero).unwrap(),
        "`qty` = 0"
    );
    assert_eq!(
        sql_condition(copy_sqlite(), "qty", &zero).unwrap(),
        r#""qty" = 0"#
    );
    // A column named like a keyword, an embedded quote, a NULL.
    let null = CopyCell::new("NULL", "integer", true);
    assert_eq!(
        sql_condition(copy_mysql(true), "count", &null).unwrap(),
        "`count` IS NULL"
    );
    assert_eq!(
        sql_condition(copy_mysql(true), "we`ird", &zero).unwrap(),
        "`we``ird` = 0"
    );
    assert_eq!(
        sql_condition(copy_postgres(), "we\"ird", &zero).unwrap(),
        r#""we""ird" = 0"#
    );
    // A string with a backslash follows the session's escape mode.
    let path = CopyCell::new("a\\b", "text", false);
    assert_eq!(
        sql_condition(copy_mysql(true), "p", &path).unwrap(),
        r"`p` = 'a\\b'"
    );
    assert_eq!(
        sql_condition(copy_mysql(false), "p", &path).unwrap(),
        r"`p` = 'a\b'"
    );
    // A 30-digit numeric key stays exact; a float keeps its spelling.
    let key = CopyCell::new("12345678901234567890123456789", "numeric(30,0)", false);
    assert_eq!(
        sql_condition(copy_postgres(), "id", &key).unwrap(),
        r#""id" = 12345678901234567890123456789"#
    );
    // Binary is a real binary literal, never the display text in quotes.
    let bytes = CopyCell::new("0xdeadbeef", "blob", false);
    assert_eq!(
        sql_condition(copy_mysql(true), "b", &bytes).unwrap(),
        "`b` = X'deadbeef'"
    );
    // A truncated preview cannot be written.
    let preview = CopyCell::unwritable("\\x0011... (4096 bytes)", "bytea", "preview only");
    assert_eq!(
        sql_condition(copy_postgres(), "b", &preview).unwrap_err(),
        "preview only"
    );
    assert!(sql_condition(copy_postgres(), "", &zero).is_err());
}

fn dup_columns_data() -> CopyData {
    CopyData {
        columns: ["id", "id", "id_2", "name"].map(String::from).to_vec(),
        rows: vec![vec![
            CopyCell::new("1", "integer", false),
            CopyCell::new("2", "integer", false),
            CopyCell::new("3", "integer", false),
            CopyCell::new("x", "text", false),
        ]],
        table: String::new(),
        schema: None,
        key_column: None,
        keys: vec![],
        target: copy_postgres(),
    }
}

#[test]
fn repeated_column_names_survive_every_json_format_in_order() {
    let data = dup_columns_data();
    // `id_2` is a real column, so the second `id` cannot take that name.
    let expected = r#"{"id":1,"id_3":2,"id_2":3,"name":"x"}"#;
    assert_eq!(
        data.format(CopyFormat::Json).unwrap(),
        format!("[{expected}]")
    );
    assert_eq!(data.format(CopyFormat::JsonLines).unwrap(), expected);
    let pretty = data.format(CopyFormat::Pretty).unwrap();
    let keys: Vec<_> = pretty
        .lines()
        .filter_map(|line| line.trim().split('"').nth(1))
        .collect();
    assert_eq!(keys, ["id", "id_3", "id_2", "name"], "{pretty}");
    let (names, renames) = data.json_columns();
    assert_eq!(names, ["id", "id_3", "id_2", "name"]);
    assert_eq!(renames, [("id".to_string(), "id_3".to_string())]);
    assert_eq!(
        results::copy::renamed_columns_notice(&renames).unwrap(),
        "Repeated column names were made unique in the JSON: id became id_3"
    );
    assert!(results::copy::renamed_columns_notice(&[]).is_none());
    // Three of a kind.
    let (names, renames) = results::copy::unique_column_names(["a", "a", "a", "b"]);
    assert_eq!(names, ["a", "a_2", "a_3", "b"]);
    assert_eq!(renames.len(), 2);
    // Non-JSON formats keep the real names.
    assert!(data
        .format(CopyFormat::CsvWithHeaders)
        .unwrap()
        .starts_with("id,id,id_2,name\n"));
}

#[gpui::test]
fn value_copy_json_and_the_row_tab_keep_both_id_columns(cx: &mut TestAppContext) {
    let (app, visual) = value_app(cx);
    let batch = value_batch(vec![
        ("id", value_ints(&[1])),
        ("id", value_ints(&[2])),
        ("name", value_strings(&[Some("x")])),
    ]);
    let tab_id = value_tab(
        &app,
        visual,
        &[
            ("id", "integer", false, true),
            ("name", "text", true, false),
        ],
        batch,
    );
    app.update(visual, |this, _| {
        this.av()
            .grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 0 });
        this.av()
            .grid_ui
            .selection
            .borrow_mut()
            .extend_to(CellPosition { row: 0, column: 2 });
        let json = this
            .copy_context_selection(tab_id, CopyFormat::JsonLines)
            .unwrap();
        assert_eq!(json, r#"{"id":1,"id_2":2,"name":"x"}"#);

        let tab = this.workspace.tab(tab_id).unwrap().clone();
        let row = this.build_cell_detail_row(&tab, 0).expect("row detail");
        let object = row.document.as_object().expect("an object");
        assert_eq!(
            object.keys().map(String::as_str).collect::<Vec<_>>(),
            ["id", "id_2", "name"],
            "the tree keeps every column"
        );
        assert_eq!(object["id_2"], serde_json::json!(2));
        assert!(row.json.contains("\"id_2\": 2"), "{}", row.json);
        // The field list shows the real names.
        assert_eq!(
            row.fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "id", "name"]
        );
    });
}

/// Parses delimited text the way the delimited export writes it: an unquoted empty field is
/// NULL, a quoted one an empty string; `null_text` (when set) is NULL.
fn parse_delimited(
    text: &str,
    column: &str,
    row: &str,
    quote: char,
    null_text: &str,
) -> Vec<Vec<Option<String>>> {
    let mut rows = vec![vec![]];
    let mut rest = text;
    loop {
        let field = if let Some(after) = rest.strip_prefix(quote) {
            let mut value = String::new();
            let mut chars = after.char_indices();
            let mut end = after.len();
            while let Some((index, ch)) = chars.next() {
                if ch == quote {
                    if after[index + 1..].starts_with(quote) {
                        value.push(quote);
                        chars.next();
                        continue;
                    }
                    end = index + 1;
                    break;
                }
                value.push(ch);
            }
            rest = &after[end..];
            Some(value)
        } else {
            let stop = [column, row]
                .iter()
                .filter_map(|separator| rest.find(separator))
                .min()
                .unwrap_or(rest.len());
            let value = &rest[..stop];
            rest = &rest[stop..];
            (!(value.is_empty() || value == null_text && !null_text.is_empty()))
                .then(|| value.to_string())
        };
        rows.last_mut().unwrap().push(field);
        if let Some(after) = rest.strip_prefix(column) {
            rest = after;
        } else if let Some(after) = rest.strip_prefix(row) {
            rest = after;
            rows.push(vec![]);
        } else {
            break;
        }
    }
    rows
}

fn tricky_values() -> Vec<Vec<Option<&'static str>>> {
    vec![
        vec![None, Some(""), Some("NULL")],
        vec![Some("a,b"), Some("say \"hi\""), Some("tab\there")],
        vec![Some("line\nbreak"), Some("cr\rlf"), Some("crlf\r\nend")],
        vec![Some("|pipe|"), Some("'single'"), Some("  spaced  ")],
    ]
}

fn delimited_data(values: &[Vec<Option<&str>>]) -> CopyData {
    CopyData {
        columns: ["a", "b", "c"].map(String::from).to_vec(),
        rows: values
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| match value {
                        Some(text) => CopyCell::new(text, "text", false),
                        None => CopyCell::new("NULL", "text", true),
                    })
                    .collect()
            })
            .collect(),
        table: String::new(),
        schema: None,
        key_column: None,
        keys: vec![],
        target: copy_postgres(),
    }
}

#[test]
fn delimited_copy_keeps_null_empty_and_the_text_null_apart() {
    let values = tricky_values();
    let owned: Vec<Vec<Option<String>>> = values
        .iter()
        .map(|row| row.iter().map(|value| value.map(String::from)).collect())
        .collect();
    let data = delimited_data(&values);
    for (format, column) in [
        (CopyFormat::Csv, ","),
        (CopyFormat::Tsv, "\t"),
        (CopyFormat::Pipe, "|"),
    ] {
        let text = data.format(format).unwrap();
        assert_eq!(
            parse_delimited(&text, column, "\n", '"', ""),
            owned,
            "{format:?} round trip: {text:?}"
        );
    }
    // Spelled out: NULL is an empty field, '' is quoted, 'NULL' is plain text.
    assert!(data
        .format(CopyFormat::Csv)
        .unwrap()
        .starts_with(",\"\",NULL\n"));
    // Headers.
    let with_headers = data.format(CopyFormat::CsvWithHeaders).unwrap();
    assert!(
        with_headers.starts_with("a,b,c\n,\"\",NULL\n"),
        "{with_headers}"
    );
    // A one-cell Raw copy: NULL is empty, the string NULL stays NULL.
    let null = delimited_data(&[vec![None]]);
    let mut single = null;
    single.columns.truncate(1);
    assert_eq!(single.format(CopyFormat::Raw).unwrap(), "");
    let text_null = delimited_data(&[vec![Some("NULL")]]);
    let mut single = text_null;
    single.columns.truncate(1);
    assert_eq!(single.format(CopyFormat::Raw).unwrap(), "NULL");
}

#[test]
fn custom_dsv_round_trips_and_honours_the_configured_null_text() {
    let values = tricky_values();
    let owned: Vec<Vec<Option<String>>> = values
        .iter()
        .map(|row| row.iter().map(|value| value.map(String::from)).collect())
        .collect();
    let data = delimited_data(&values);
    // Default options: NULL is empty.
    let text = data
        .format_with_dsv(CopyFormat::Dsv, &DsvOptions::default())
        .unwrap();
    assert_eq!(parse_delimited(&text, ",", "\n", '"', ""), owned);
    // A configured NULL text: NULL is written as it, and a real string equal
    // to it is quoted so the two stay apart.
    let mut with_text = values.clone();
    with_text.push(vec![None, Some("<null>"), Some("")]);
    let options = DsvOptions {
        column_separator: ";".into(),
        row_separator: "\r\n".into(),
        null_text: "<null>".into(),
        quote: '\'',
        include_headers: false,
    };
    let text = delimited_data(&with_text)
        .format_with_dsv(CopyFormat::Dsv, &options)
        .unwrap();
    let last = text.rsplit("\r\n").next().unwrap();
    assert_eq!(last, "<null>;'<null>';''");
    // Line breaks are quoted whatever the row separator is.
    let single_char_rows = DsvOptions {
        column_separator: ";".into(),
        row_separator: "|".into(),
        ..DsvOptions::default()
    };
    let breaks = delimited_data(&[vec![Some("x\ny"), Some("p\rq"), Some("r")]]);
    assert_eq!(
        breaks
            .format_with_dsv(CopyFormat::Dsv, &single_char_rows)
            .unwrap(),
        "\"x\ny\";\"p\rq\";r"
    );
}

#[test]
fn markdown_html_and_xlsx_text_keeps_null_out_of_the_data() {
    let data = delimited_data(&[vec![None, Some("NULL"), Some("")]]);
    assert!(data
        .format(CopyFormat::Markdown)
        .unwrap()
        .ends_with("|  | NULL |  |"));
    assert!(data
        .format(CopyFormat::Html)
        .unwrap()
        .contains("<td></td><td>NULL</td><td></td>"));
    let [null, text, _] = &data.rows[0][..] else {
        panic!("three cells")
    };
    assert_eq!(null.plain_text(), "");
    assert_eq!(text.plain_text(), "NULL");
    // The grid and snapshots still show the NULL placeholder.
    assert_eq!(null.display(), "NULL");
}

#[test]
fn xml_copy_writes_a_null_as_an_empty_element_and_the_string_null_as_text() {
    // The XML export writes a NULL as an empty element.
    let data = delimited_data(&[vec![None, Some("NULL"), Some("a<b")]]);
    let xml = data.format(CopyFormat::Xml).unwrap();
    assert_eq!(
        xml,
        "<rows><row><field name=\"a\"></field><field name=\"b\">NULL</field>\
         <field name=\"c\">a&lt;b</field></row></rows>"
    );
}

#[test]
fn xlsx_export_writes_a_null_as_an_empty_string_not_the_text_null() {
    // The XLSX writer takes strings; an empty one leaves the worksheet cell
    // blank, while the string NULL is written. (No spreadsheet reader is in
    // the build, so the file is checked for size and the text for content.)
    let data = delimited_data(&[vec![None, Some("NULL"), Some("x")]]);
    let path = std::env::temp_dir().join(format!("tablex-null-{}.xlsx", uuid::Uuid::new_v4()));
    let (rows, notice) = results::export::export_context_data(
        &path,
        &data,
        results::export::ContextExportFormat::Xlsx,
        &crate::settings::ExportDefaults::default(),
    )
    .unwrap();
    assert_eq!((rows, notice), (1, None));
    assert!(std::fs::metadata(&path).unwrap().len() > 0);
    std::fs::remove_file(path).unwrap();
    let texts: Vec<_> = data.rows[0].iter().map(CopyCell::plain_text).collect();
    assert_eq!(texts, ["", "NULL", "x"]);
}

/// Makes the tab's connection a MySQL one: backtick quoting, `backslash_escapes`
/// as the session reported it, `X'..'` binary literals.
fn value_make_mysql(this: &mut AppRoot, tab_id: u64, backslash_escapes: bool) {
    let tab = this.workspace.tab(tab_id).unwrap().clone();
    let mut capabilities = engine_driver_api::DriverCapabilities::new("mysql", "MySQL", Vec::new());
    capabilities.sql_dialect = Some(engine_driver_api::SqlDialectInfo {
        identifier_quote: "`".into(),
        string_literal_quote: "'".into(),
        supports_schemas: false,
        database_qualified_names: true,
        like_requires_text_cast: false,
        string_backslash_escapes: backslash_escapes,
        double_quote_identifiers: false,
        bracket_identifiers: false,
    });
    this.driver_runtime
        .capabilities
        .insert(tab.connection_id.clone(), std::sync::Arc::new(capabilities));
    for saved in &mut this.saved_connections {
        if saved.matches_id(&tab.connection_id) {
            saved.kind = ConnectionKind::Mysql;
        }
    }
    let dialect = this.sql_table_dialect(&tab);
    assert_eq!(dialect.identifier_quote, '`');
    let sql = crate::sql_table::base_select(dialect, tab.schema.as_deref(), &tab.collection).unwrap();
    this.workspace.tab_mut(tab_id).unwrap().sql = sql;
}

#[gpui::test]
fn a_mysql_quoted_tab_clones_and_copies_sql_in_mysql_syntax(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        value_make_mysql(this, tab_id, true);
        for column in &mut this
            .workspace
            .runtime_mut(tab_id)
            .unwrap()
            .structure_columns
        {
            column.is_generated = Some(false);
            column.auto_increment = Some(column.name == "id");
        }
        let tab = this.workspace.tab(tab_id).unwrap().clone();
        assert!(tab.sql.contains('`'), "a backtick statement: {}", tab.sql);
        // Both were refused ("cannot be sorted") while the statement was parsed as PostgreSQL.
        assert!(this.sql_clone_available(tab_id), "Clone Row on a MySQL tab");
        assert!(this.prepare_sql_clone(tab_id, 0).is_ok());
        this.av()
            .grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 0 });
        this.av()
            .grid_ui
            .selection
            .borrow_mut()
            .extend_to(CellPosition { row: 1, column: 1 });
        for format in [
            CopyFormat::SqlInsert,
            CopyFormat::SqlInsertNoPk,
            CopyFormat::SqlUpdate,
            CopyFormat::SqlSelect,
            CopyFormat::SqlWhere,
        ] {
            let sql = this
                .copy_context_selection(tab_id, format)
                .unwrap_or_else(|e| {
                    panic!("{format:?} on a MySQL tab: {e}");
                });
            assert!(
                sql.contains('`') && !sql.contains("\"id\""),
                "{format:?}: {sql}"
            );
        }
        let insert = this
            .copy_context_selection(tab_id, CopyFormat::SqlInsert)
            .unwrap();
        assert!(
            insert.starts_with(&format!(
                "INSERT INTO `{}`.`{}` (`id`",
                tab.schema.as_deref().unwrap_or("public"),
                tab.collection
            )),
            "{insert}"
        );
    });
}

#[gpui::test]
fn copy_sql_condition_on_a_mysql_tab_uses_backticks_and_the_sessions_escapes(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    for (backslash_escapes, literal) in [(true, r"'a\\b'"), (false, r"'a\b'")] {
        app.update(visual, |this, cx| {
            let tab_id = this.workspace.active_tab().unwrap().id;
            value_make_mysql(this, tab_id, backslash_escapes);
            // The `name` column of the first row holds a backslash.
            let batch = value_batch(vec![
                ("id", value_ints(&[0])),
                ("name", value_strings(&[Some(r"a\b")])),
            ]);
            this.workspace.runtime_mut(tab_id).unwrap().grid =
                GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
                    std::sync::Arc::new(batch),
                ])));
            this.reset_tab_view(tab_id);
            this.open_cell_detail_tab(tab_id, CellDetailTab::Details, cx);
        });
        for (column, expected) in [
            (0, "`id` = 0".to_string()),
            (1, format!("`name` = {literal}")),
        ] {
            app.update(visual, |this, _| {
                this.select_grid_cell(this.active_id(), CellPosition { row: 0, column });
            });
            settle(visual);
            let condition = visual
                .debug_bounds("cell-detail-copy-sql-condition")
                .expect("copy SQL condition action");
            visual.simulate_click(condition.center(), Default::default());
            assert_eq!(
                visual
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some(expected.as_str()),
                "session escapes {backslash_escapes}, column {column}"
            );
        }
    }
}

#[test]
fn the_connect_replys_dialect_replaces_the_static_one_for_that_connection_only() {
    let mut driver = engine_driver_api::DriverCapabilities::new("mysql", "MySQL", Vec::new());
    let dialect = |backslash_escapes| engine_driver_api::SqlDialectInfo {
        identifier_quote: "`".into(),
        string_literal_quote: "'".into(),
        supports_schemas: false,
        database_qualified_names: true,
        like_requires_text_cast: false,
        string_backslash_escapes: backslash_escapes,
        double_quote_identifiers: false,
        bracket_identifiers: false,
    };
    driver.sql_dialect = Some(dialect(true));
    // A reply without the field keeps today's behaviour: the static dialect.
    let unchanged = connection_ui::connection_capabilities(&driver, None);
    assert_eq!(unchanged.sql_dialect, Some(dialect(true)));
    // A session running with NO_BACKSLASH_ESCAPES says so.
    let strict = connection_ui::connection_capabilities(&driver, Some(dialect(false)));
    assert_eq!(strict.sql_dialect, Some(dialect(false)));
    assert_eq!(
        driver.sql_dialect,
        Some(dialect(true)),
        "the driver's own is untouched"
    );
    // The app dialect built from either escapes a filter value `a\b` accordingly.
    let literal = |capabilities: &engine_driver_api::DriverCapabilities| {
        let app_dialect = crate::sql_table::SqlDialect::from_capabilities(
            capabilities.sql_dialect.as_ref().unwrap(),
        );
        crate::sql_filter::sql_string_literal(app_dialect, r"a\b")
    };
    assert_eq!(literal(&unchanged), r"'a\\b'");
    assert_eq!(literal(&strict), r"'a\b'");
}

#[gpui::test]
fn each_connection_filters_with_its_own_reported_backslash_mode(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let connection = this.workspace.tab(tab_id).unwrap().connection_id.clone();
        for (reported, literal) in [(true, r"'a\\b'"), (false, r"'a\b'")] {
            value_make_mysql(this, tab_id, reported);
            let dialect = this.sql_dialect_for_connection(&connection).unwrap();
            assert_eq!(
                crate::sql_filter::sql_string_literal(dialect, r"a\b"),
                literal
            );
            let where_clause = crate::sql_filter::build_where(
                dialect,
                &[crate::sql_filter::SqlFilterRule {
                    id: 1,
                    column: "path".into(),
                    mode: crate::sql_filter::SqlFilterMode::Equals,
                    value: r"a\b".into(),
                    ..Default::default()
                }],
                |_| crate::sql_filter::SqlFilterColumnKind::Text,
            )
            .unwrap();
            assert_eq!(where_clause, format!("`path` = {literal}"));
            // The WHERE bar lexes the typed text in the same mode.
            let hidden = r"path = 'x\' OR 1 = 1 UNION SELECT 1 -- '";
            assert_eq!(
                crate::sql_table::validate_fragment(dialect, "WHERE", hidden).is_ok(),
                reported,
                "mode {reported}"
            );
        }
    });
}

fn value_native_batch(
    columns: Vec<(&str, arrow_array::ArrayRef, Option<&str>)>,
) -> arrow_array::RecordBatch {
    let fields: Vec<arrow_schema::Field> = columns
        .iter()
        .map(|(name, array, native)| {
            let field = arrow_schema::Field::new(*name, array.data_type().clone(), true);
            match native {
                Some(native) => field.with_metadata(std::collections::HashMap::from([(
                    crate::grid_arrow::NATIVE_TYPE_METADATA_KEY.to_string(),
                    native.to_string(),
                )])),
                None => field,
            }
        })
        .collect();
    arrow_array::RecordBatch::try_new(
        std::sync::Arc::new(arrow_schema::Schema::new(fields)),
        columns.into_iter().map(|(_, array, _)| array).collect(),
    )
    .expect("equal-length columns")
}

fn value_column_texts(this: &AppRoot, tab_id: u64, column: usize) -> Vec<String> {
    let grid = this.grid(tab_id);
    (0..grid.row_count())
        .map(|row| {
            let position = CellPosition { row, column };
            if grid.cell_is_null(position) {
                "NULL".to_string()
            } else {
                grid.full_cell_text(position).unwrap()
            }
        })
        .collect()
}

#[gpui::test]
fn value_page_sort_orders_numeric_text_by_the_columns_resolved_type(cx: &mut TestAppContext) {
    use arrow_array::{ArrayRef, Float64Array, StringArray, UInt64Array};
    let (app, visual) = value_app(cx);
    let batch = value_native_batch(vec![
        // PostgreSQL `numeric` arrives as text; MySQL `DECIMAL` the same way.
        (
            "amount",
            std::sync::Arc::new(StringArray::from(vec![
                Some("9"),
                Some("10"),
                None,
                Some("100.5"),
                Some("-3.25"),
            ])) as ArrayRef,
            Some("numeric"),
        ),
        // An unsigned 64-bit integer: its Arrow label is just `number`.
        (
            "big",
            std::sync::Arc::new(UInt64Array::from(vec![
                9_007_199_254_740_993,
                9_007_199_254_740_992,
                18_446_744_073_709_551_615,
                1,
                9_007_199_254_740_994,
            ])) as ArrayRef,
            None,
        ),
        (
            "ratio",
            std::sync::Arc::new(Float64Array::from(vec![
                f64::NAN,
                1.5,
                f64::INFINITY,
                f64::NEG_INFINITY,
                -0.5,
            ])) as ArrayRef,
            Some("float8"),
        ),
        (
            "name",
            std::sync::Arc::new(StringArray::from(vec![
                Some("b"),
                Some("B"),
                Some("a"),
                None,
                Some("A"),
            ])) as ArrayRef,
            Some("text"),
        ),
    ]);
    let tab_id = value_tab(&app, visual, &[], batch);
    let sort = |visual: &mut gpui::VisualTestContext, column: usize, descending: bool| {
        app.update_in(visual, |this, window, cx| {
            this.view_mut(tab_id).grid_ui.context_cell = Some(CellPosition { row: 0, column });
            this.sort_context_page(tab_id, descending, window, cx);
        });
        app.read_with(visual, |this, _| value_column_texts(this, tab_id, column))
    };
    // 9, 10, 100.5 sort as numbers, not as the text 10, 100.5, 9. NULL is the
    // greatest value: last ascending, first descending.
    assert_eq!(
        sort(visual, 0, false),
        ["-3.25", "9", "10", "100.5", "NULL"]
    );
    assert_eq!(sort(visual, 0, true), ["NULL", "100.5", "10", "9", "-3.25"]);
    // Neighbours above 2^53 stay apart (they are one f64).
    assert_eq!(
        sort(visual, 1, false),
        [
            "1",
            "9007199254740992",
            "9007199254740993",
            "9007199254740994",
            "18446744073709551615"
        ]
    );
    // -Infinity < finite < Infinity < NaN.
    assert_eq!(
        sort(visual, 2, false),
        ["-Infinity", "-0.5", "1.5", "Infinity", "NaN"]
    );
    // Text keeps its byte order.
    assert_eq!(sort(visual, 3, false), ["A", "B", "a", "b", "NULL"]);
}

#[gpui::test]
fn value_selection_stats_are_exact_for_integers_and_decimal_text(cx: &mut TestAppContext) {
    use arrow_array::{ArrayRef, Float64Array, StringArray, UInt64Array};
    let (app, visual) = value_app(cx);
    let batch = value_native_batch(vec![
        (
            "n",
            std::sync::Arc::new(UInt64Array::from(vec![9_007_199_254_740_993, 1])) as ArrayRef,
            Some("bigint"),
        ),
        (
            "amount",
            std::sync::Arc::new(StringArray::from(vec![
                Some("12345678901234567890.25"),
                Some("0.75"),
            ])) as ArrayRef,
            Some("numeric"),
        ),
        (
            "ratio",
            std::sync::Arc::new(Float64Array::from(vec![0.1, 0.2])) as ArrayRef,
            Some("float8"),
        ),
        (
            "label",
            std::sync::Arc::new(StringArray::from(vec![Some("5"), Some("6")])) as ArrayRef,
            Some("varchar(10)"),
        ),
    ]);
    let tab_id = value_tab(&app, visual, &[], batch);
    let label = |visual: &mut gpui::VisualTestContext, column: usize| {
        app.update(visual, |this, _| {
            let selection = this.av().grid_ui.selection.clone();
            let mut selection = selection.borrow_mut();
            selection.select(CellPosition { row: 0, column });
            selection.extend_to(CellPosition { row: 1, column });
            drop(selection);
            this.selection_stats(tab_id).expect("stats").label()
        })
    };
    assert_eq!(
        label(visual, 0),
        "SUM: 9007199254740994  AVG: 4503599627370497  MIN: 1  MAX: 9007199254740993  2 cells 2 rows"
    );
    // Text-typed `numeric` is INCLUDED, digit for digit.
    assert_eq!(
        label(visual, 1),
        "SUM: 12345678901234567891  AVG: 6172839450617283945.5  MIN: 0.75  MAX: 12345678901234567890.25  2 cells 2 rows"
    );
    // A float column is approximate; a varchar of digits never counts.
    assert_eq!(
        label(visual, 2),
        "SUM: ≈0.3  AVG: ≈0.15  MIN: ≈0.1  MAX: ≈0.2  2 cells 2 rows"
    );
    assert_eq!(label(visual, 3), "2 cells 2 rows");
}

#[gpui::test]
fn value_rows_committed_by_a_partial_apply_are_marked_until_the_page_reloads(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = value_orders_app(cx, &VALUE_ORDERS);
    // Row 0 (change 0) and row 1 (change 1) each get an edit.
    app.update(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap().clone();
        for (row, original, text) in [(0, "NULL", "a"), (1, "null", "b")] {
            let position = CellPosition { row, column: 2 };
            let payload = this
                .cell_detail_payload(&tab, &this.build_cell_detail(&tab, position).unwrap())
                .expect("payload");
            this.try_stage_sql_edit(tab_id, position, &payload, original, text)
                .expect("staged");
        }
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
    app.update(visual, |this, cx| {
        this.workspace
            .runtime_mut(tab_id)
            .unwrap()
            .sql_apply_running = true;
        this.finish_sql_apply(tab_id, &submitted, &[], Err(error), cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        // Row 1's change stays staged; row 0's was committed but its cells
        // still show the old value, so the row is marked.
        assert_eq!(runtime.pending_edits.len(), 1);
        assert!(runtime.row_is_applied(0));
        assert!(!runtime.row_is_applied(1));
        let message = this
            .view(tab_id)
            .detail_ui
            .error
            .clone()
            .expect("a message");
        assert!(
            message.contains('\u{21bb}') && message.contains("reload"),
            "{message}"
        );
    });
    // The mark is drawn in the row's gutter (row 1 has none).
    let gutter_mark = |visual: &mut gpui::VisualTestContext, row: usize| {
        let selector: &'static str =
            Box::leak(format!("pending-delete-row-{row}").into_boxed_str());
        visual
            .debug_bounds(selector)
            .expect("the row is painted")
            .size
            .width
    };
    assert_eq!(gutter_mark(visual, 0), gpui::px(10.0));
    assert_eq!(gutter_mark(visual, 1), gpui::px(0.0));
    // A reloaded page is another grid: nothing is marked any more.
    app.update(visual, |this, cx| {
        this.workspace
            .runtime_mut(tab_id)
            .unwrap()
            .pending_edits
            .clear();
        this.workspace
            .runtime_mut(tab_id)
            .unwrap()
            .clear_pending_cell_texts();
        this.workspace.runtime_mut(tab_id).unwrap().grid =
            GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
                std::sync::Arc::new(value_orders_batch(&VALUE_ORDERS)),
            ])));
        cx.notify();
    });
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert!(!runtime.row_is_applied(0));
        assert!(
            runtime.applied_rows.is_some(),
            "stale until replaced, never matching"
        );
    });
}

#[gpui::test]
fn value_a_batch_that_fully_applies_marks_nothing(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = value_orders_app(cx, &VALUE_ORDERS);
    app.update(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap().clone();
        let position = CellPosition { row: 0, column: 2 };
        let payload = this
            .cell_detail_payload(&tab, &this.build_cell_detail(&tab, position).unwrap())
            .expect("payload");
        this.try_stage_sql_edit(tab_id, position, &payload, "NULL", "a")
            .expect("staged");
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
        "applied: [0]; failed: []; not attempted: []",
    ));
    app.update(visual, |this, cx| {
        this.finish_sql_apply(tab_id, &submitted, &[], Err(error), cx);
    });
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert!(runtime.pending_edits.is_empty());
        assert!(!runtime.row_is_applied(0), "the page reloads instead");
    });
}

#[gpui::test]
fn value_a_where_fragment_with_a_union_is_rejected_inline_without_running(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    let generation = app.read_with(visual, |this, _| {
        this.workspace.runtime(tab_id).unwrap().query.generation()
    });
    // It would return another table's rows under this table's key columns.
    sql_table_type(
        visual,
        "sql-where-field",
        "1 = 0 UNION SELECT * FROM customers",
    );
    visual.simulate_keystrokes("enter");
    settle(visual);
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        let error = runtime
            .sql_table
            .error
            .as_deref()
            .expect("validation error");
        assert!(error.contains("UNION"), "{error}");
        assert_eq!(runtime.query.generation(), generation, "nothing was run");
        assert!(this.workspace.tab(tab_id).unwrap().applied_where.is_empty());
    });
    assert!(
        visual.debug_bounds("sql-query-error").is_some(),
        "danger state in the bar"
    );
}

#[gpui::test]
fn value_a_json_copy_says_once_in_the_status_row_that_columns_were_renamed(
    cx: &mut TestAppContext,
) {
    let (app, visual) = value_app(cx);
    let batch = value_batch(vec![
        ("id", value_ints(&[1])),
        ("id", value_ints(&[2])),
        ("name", value_strings(&[Some("x")])),
    ]);
    let tab_id = value_tab(&app, visual, &[], batch);
    app.update(visual, |this, cx| {
        let selection = this.av().grid_ui.selection.clone();
        let mut selection = selection.borrow_mut();
        selection.select(CellPosition { row: 0, column: 0 });
        selection.extend_to(CellPosition { row: 0, column: 2 });
        drop(selection);
        // A format that keeps the real names says nothing.
        this.copy_format_to_clipboard(tab_id, CopyFormat::CsvWithHeaders, cx);
        assert_eq!(this.view(tab_id).detail_ui.error, None);
        this.copy_format_to_clipboard(tab_id, CopyFormat::Json, cx);
        assert_eq!(
            this.view(tab_id).detail_ui.error.as_deref(),
            Some("Repeated column names were made unique in the JSON: id became id_2")
        );
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(r#"[{"id":1,"id_2":2,"name":"x"}]"#)
        );
    });
}

#[test]
fn value_numbers_of_a_decimal_column_are_always_their_exact_text() {
    use results::editing::mutation_value_to_json_for as wire;
    let number = |text: &str| MutationValue::Number(text.into());
    // The PostgreSQL driver refuses a JSON number above 15 significant digits
    // for numeric / money: these must be strings.
    for (text, ty) in [
        ("12345678.12345678", "numeric(20,8)"),
        ("1234567890123456789", "numeric(30,0)"),
        ("5", "numeric"),
        ("5", "decimal(10,2)"),
        ("5", "money"),
    ] {
        assert_eq!(
            wire(&number(text), ty),
            serde_json::json!(text),
            "{text} in {ty}"
        );
    }
    // A whole number of any other column stays a JSON number when it reads back.
    assert_eq!(wire(&number("5"), "integer"), serde_json::json!(5));
    assert_eq!(
        wire(&number("9007199254740993"), "bigint"),
        serde_json::json!(9007199254740993u64)
    );
    assert_eq!(
        wire(&number("1.5"), "double precision"),
        serde_json::json!("1.5")
    );
    assert_eq!(
        wire(&MutationValue::Text("5".into()), "numeric"),
        serde_json::json!("5")
    );
}

#[gpui::test]
fn value_a_staged_decimal_edit_reaches_the_driver_batch_as_a_string(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = value_orders_app(cx, &VALUE_ORDERS);
    app.update(visual, |this, _| {
        let runtime = this.workspace.runtime_mut(tab_id).unwrap();
        for column in &mut runtime.structure_columns {
            if column.name == "score" {
                column.data_type = "numeric(20,8)".into();
            }
        }
        let tab = this.workspace.tab(tab_id).unwrap().clone();
        let score = (0..this.grid(tab_id).column_count())
            .find(|c| this.grid(tab_id).column_name(*c) == Some("score"))
            .expect("score column");
        let position = CellPosition {
            row: 0,
            column: score,
        };
        let payload = this
            .cell_detail_payload(&tab, &this.build_cell_detail(&tab, position).unwrap())
            .expect("payload");
        this.try_stage_sql_edit(tab_id, position, &payload, "1.5", "12345678.12345678")
            .expect("staged");
        let edits = this
            .workspace
            .runtime(tab_id)
            .unwrap()
            .pending_edits
            .clone();
        assert_eq!(
            edits[0].request.value,
            MutationValue::Number("12345678.12345678".into())
        );
        // The batch request itself is built only for a connected driver; the
        // wire value is what it puts in the change.
        let wire =
            results::editing::mutation_value_to_json_for(&edits[0].request.value, "numeric(20,8)");
        assert_eq!(wire, serde_json::json!("12345678.12345678"));
    });
}

fn partial_message(server_text: &str) -> String {
    format!("{server_text}applied: [0]; failed: [1]; unknown: [2]; not attempted: [3]")
}

#[test]
fn partially_applied_lists_are_read_from_the_end_of_the_message() {
    use results::editing::PartialApply;
    let parse = |message: &str| match SqlApplyError::from(value_rejected(
        "mutation.partially_applied",
        message,
    )) {
        SqlApplyError::PartiallyApplied(partial) => partial,
        other => panic!("not recognised: {other:?} for {message:?}"),
    };
    let lists = |p: &PartialApply| {
        (
            p.applied.clone(),
            p.failed.clone(),
            p.unknown.clone(),
            p.not_attempted.clone(),
        )
    };
    // The four-list tail, and the older three-list one.
    assert_eq!(
        lists(&parse(&partial_message("batch stopped: "))),
        (vec![0], vec![1], vec![2], vec![3])
    );
    assert_eq!(
        lists(&parse("applied: [0, 1]; failed: [2]; not attempted: []")),
        (vec![0, 1], vec![2], vec![], vec![])
    );
    // The server's text in front may hold the same words and brackets.
    let hostile = partial_message(
        "Duplicate entry 'applied: [9]; failed: [8]; unknown: [7]; not attempted: [6]' for key 'PRIMARY'; ",
    );
    assert_eq!(
        lists(&parse(&hostile)),
        (vec![0], vec![1], vec![2], vec![3])
    );
    let hostile_old =
        "Duplicate entry 'applied: [9]' for key 'x'; applied: [0]; failed: [1]; not attempted: [2]";
    assert_eq!(
        lists(&parse(hostile_old)),
        (vec![0], vec![1], vec![], vec![2])
    );
    // An older-shape tail whose server text mentions `unknown: [5]` is not read as one.
    let decoy = "Unknown column 'unknown: [5]'; applied: [0]; failed: [1]; not attempted: [2]";
    assert_eq!(lists(&parse(decoy)), (vec![0], vec![1], vec![], vec![2]));
    // A tail that is not a tail is an ordinary failure.
    for garbled in [
        "garbled",
        "applied: [0]; failed: [1]",
        "applied: [x]; failed: []; not attempted: []",
    ] {
        assert!(matches!(
            SqlApplyError::from(value_rejected("mutation.partially_applied", garbled)),
            SqlApplyError::Failed(_)
        ));
    }
}

#[gpui::test]
fn value_unknown_changes_stay_staged_marked_and_never_count_as_applied(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = value_orders_app(cx, &VALUE_ORDERS);
    // Three rows' worth of changes: row 0 (change 0), row 1 (change 1).
    app.update(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap().clone();
        for (row, original, text) in [(0, "NULL", "a"), (1, "null", "b")] {
            let position = CellPosition { row, column: 2 };
            let payload = this
                .cell_detail_payload(&tab, &this.build_cell_detail(&tab, position).unwrap())
                .expect("payload");
            this.try_stage_sql_edit(tab_id, position, &payload, original, text)
                .expect("staged");
        }
    });
    let submitted = app.read_with(visual, |this, _| {
        this.workspace
            .runtime(tab_id)
            .unwrap()
            .pending_edits
            .clone()
    });
    // The server text holds `applied: [1]`; the real tail says change 1 is unknown.
    let error = SqlApplyError::from(value_rejected(
        "mutation.partially_applied",
        "Duplicate entry 'applied: [1]' for key 'PRIMARY'; applied: [0]; failed: []; unknown: [1]; not attempted: []",
    ));
    app.update(visual, |this, cx| {
        this.workspace
            .runtime_mut(tab_id)
            .unwrap()
            .sql_apply_running = true;
        this.finish_sql_apply(tab_id, &submitted, &[], Err(error), cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        // Row 1's change is neither applied nor failed: it stays staged.
        assert_eq!(runtime.pending_edits.len(), 1);
        assert_eq!(runtime.pending_edits[0].row, 1);
        assert!(runtime.row_is_applied(0) && !runtime.row_is_unknown(0));
        assert!(runtime.row_is_unknown(1) && !runtime.row_is_applied(1));
        let message = this
            .view(tab_id)
            .detail_ui
            .error
            .clone()
            .expect("a message");
        assert!(message.contains("unknown"), "{message}");
        assert!(
            message.contains("twice") && message.contains("reload") && message.contains('?'),
            "{message}"
        );
    });
    // The two marks look different in the gutter.
    let width = |visual: &mut gpui::VisualTestContext, row: usize| {
        let selector: &'static str =
            Box::leak(format!("pending-delete-row-{row}").into_boxed_str());
        visual.debug_bounds(selector).expect("painted").size.width
    };
    assert_eq!(width(visual, 0), gpui::px(10.0));
    assert_eq!(width(visual, 1), gpui::px(10.0));
}
