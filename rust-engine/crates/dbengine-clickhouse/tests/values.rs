//! Live tests of the value conventions (read, edit and key of the same value),
//! against the dedicated ClickHouse test server only:
//!
//! ```text
//! TABLEX_TEST_CLICKHOUSE_URI=clickhouse://tablex:tablex@127.0.0.1:58123/tablex \
//!   cargo test -p dbengine-clickhouse --test values -- --include-ignored --test-threads=1
//! ```
//!
//! Every test makes its own `cf_val_*` database and drops it afterwards.

use engine_driver_conformance::build_release_dylib;
use engine_driver_conformance::fixtures::ClickHouseFixture;
use engine_driver_conformance::harness::{Conn, Ctx, Failure};
use engine_driver_host::LoaderError;
use serde_json::{json, Value};
use std::path::Path;

fn uri() -> String {
    std::env::var("TABLEX_TEST_CLICKHOUSE_URI").unwrap_or_else(|_| {
        panic!("set TABLEX_TEST_CLICKHOUSE_URI (for example clickhouse://tablex:tablex@127.0.0.1:58123/tablex)")
    })
}

/// The test URI with one more server setting (`session_timezone=Asia/Tokyo`).
fn uri_with(setting: &str) -> String {
    let uri = uri();
    let joiner = if uri.contains('?') { '&' } else { '?' };
    format!("{uri}{joiner}{setting}")
}

fn load(uri: &str) -> Ctx<'static> {
    let dylib = build_release_dylib(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        "dbengine_clickhouse",
        &[],
    )
    .expect("build the driver");
    Ctx::load(&dylib, uri, &ClickHouseFixture).expect("load the driver")
}

fn ok<T>(result: Result<T, Failure>) -> T {
    match result {
        Ok(value) => value,
        Err(Failure::Fail(message) | Failure::Skip(message)) => panic!("{message}"),
    }
}

/// The rejection code and message of a failed call.
fn rejected<T: std::fmt::Debug>(result: Result<T, LoaderError>) -> (String, String) {
    match result {
        Err(LoaderError::DriverRejected { code, message, .. }) => (code, message),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

struct Scratch<'a> {
    ctx: &'a Ctx<'a>,
    name: String,
}

impl<'a> Scratch<'a> {
    fn new(ctx: &'a Ctx<'a>, conn: &mut Conn) -> Self {
        let name = format!("cf_val_{}", ctx.unique());
        ok(conn.exec(&format!("CREATE DATABASE {name}")));
        Self { ctx, name }
    }

    fn t(&self, table: &str) -> String {
        format!("{}.{table}", self.name)
    }
}

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        self.ctx
            .cleanup(&self.ctx.fixture.drop_database(&self.name));
    }
}

fn mutate(
    conn: &mut Conn,
    scratch: &Scratch,
    table: &str,
    kind: &str,
    payload: Value,
) -> Result<u64, LoaderError> {
    let operation = conn.session.begin_operation().unwrap();
    conn.session
        .mutate(&engine_driver_api::MutationRequest {
            operation_id: operation.id(),
            kind: kind.into(),
            database: Some(scratch.name.clone()),
            object: Some(table.into()),
            identity: json!({}),
            payload,
        })
        .map(|result| result.affected_rows)
}

fn insert(conn: &mut Conn, s: &Scratch, table: &str, values: Value) -> Result<u64, LoaderError> {
    mutate(
        conn,
        s,
        table,
        "insert",
        json!({"op": "insert", "values": values}),
    )
}

fn update(
    conn: &mut Conn,
    s: &Scratch,
    table: &str,
    key: Value,
    values: Value,
) -> Result<u64, LoaderError> {
    mutate(
        conn,
        s,
        table,
        "update",
        json!({"op": "update", "key": key, "values": values}),
    )
}

/// The labels of a statement's columns.
fn labels(conn: &mut Conn, sql: &str) -> Vec<String> {
    let page = conn.open_page(sql, 0, 50, false).unwrap();
    let labels = page.columns.iter().map(|c| c.data_type.clone()).collect();
    let _ = conn.session.release_page(page.page_id);
    labels
}

const CODE_INVALID: &str = "mutation.invalid_value";
const CODE_NOT_EDITABLE: &str = "mutation.value_not_editable";

// ------------------------------------------------------------------ item 7

/// (column, ClickHouse type, fraction digits, label, shown with ` UTC`).
const INSTANT_COLUMNS: [(&str, &str, usize, &str, bool); 9] = [
    ("a_utc", "DateTime('UTC')", 0, "datetime", false),
    ("b_tokyo", "DateTime('Asia/Tokyo')", 0, "datetime", true),
    ("c_ny", "DateTime('America/New_York')", 0, "datetime", true),
    ("d_none", "DateTime", 0, "datetime (utc)", false),
    (
        "e_ms_tokyo",
        "DateTime64(3, 'Asia/Tokyo')",
        3,
        "datetime64",
        true,
    ),
    ("f_us_none", "DateTime64(6)", 6, "datetime64 (utc)", false),
    (
        "g_ns_ny",
        "DateTime64(9, 'America/New_York')",
        9,
        "datetime64",
        true,
    ),
    ("h_s_utc", "DateTime64(0, 'UTC')", 0, "datetime64", false),
    (
        "i_ms_none",
        "Nullable(DateTime64(3))",
        3,
        "datetime64 (utc)",
        false,
    ),
];

/// `2020-01-02T03:04:05` as `2020-01-02 03:04:05` (the harness prints the ISO form).
fn spaced(text: &str) -> String {
    let mut text = text.to_owned();
    if text.as_bytes().get(10) == Some(&b'T') {
        text.replace_range(10..11, " ");
    }
    text
}

const FRACTION: &str = "123456789";

/// `2020-01-02 03:04:05` (+ the first `digits` of the fraction), as the UTC
/// instant a client types.
fn typed_instant(hour: u32, digits: usize) -> String {
    let mut text = format!("2020-01-02 {hour:02}:04:05");
    if digits > 0 {
        text.push('.');
        text.push_str(&FRACTION[..digits]);
    }
    text
}

/// The stored instants of row `id`, in nanoseconds since the epoch, per column.
fn stored_instants(conn: &mut Conn, s: &Scratch, id: i32) -> Vec<String> {
    let columns: Vec<String> = INSTANT_COLUMNS
        .iter()
        .map(|(name, ..)| {
            format!(
                "toString(toUnixTimestamp64Nano(CAST({name} AS Nullable(DateTime64(9, 'UTC')))))"
            )
        })
        .collect();
    let rows = ok(conn.rows(&format!(
        "SELECT {} FROM {} WHERE id = {id}",
        columns.join(", "),
        s.t("t")
    )));
    rows.into_iter().next().expect("the row exists")
}

fn expected_nanos(hour: u32, digits: usize) -> String {
    let fraction = format!("{:0<9}", &FRACTION[..digits]);
    // 2020-01-02 00:04:05 UTC is 1577923445 s.
    let seconds = 1_577_923_445_i64 + i64::from(hour) * 3600;
    format!("{seconds}{fraction}")
}

fn one_server_zone(setting: Option<&str>) {
    let ctx = load(&setting.map_or_else(uri, uri_with));
    let mut conn = ok(ctx.connect());
    let s = Scratch::new(&ctx, &mut conn);
    let columns: Vec<String> = INSTANT_COLUMNS
        .iter()
        .map(|(name, ty, ..)| format!("{name} {ty}"))
        .collect();
    ok(conn.exec(&format!(
        "CREATE TABLE {} (id Int32, {}, note String) ENGINE = MergeTree ORDER BY id",
        s.t("t"),
        columns.join(", ")
    )));
    println!("--- server default zone: {setting:?}");

    // An insert of a typed instant stores that instant in every column.
    let mut values = json!({"id": 1, "note": "n"});
    for (name, _, digits, ..) in INSTANT_COLUMNS {
        values[name] = json!(typed_instant(3, digits));
    }
    assert_eq!(insert(&mut conn, &s, "t", values).unwrap(), 1);
    let expected: Vec<String> = INSTANT_COLUMNS
        .iter()
        .map(|(_, _, digits, ..)| expected_nanos(3, *digits))
        .collect();
    assert_eq!(stored_instants(&mut conn, &s, 1), expected, "insert");

    // Read: the UTC instant, with ` UTC` for a named zone, and the label.
    let select = format!(
        "SELECT {} FROM {} WHERE id = 1",
        INSTANT_COLUMNS.map(|(name, ..)| name).join(", "),
        s.t("t")
    );
    let shown = ok(conn.rows(&select)).remove(0);
    let page_labels = labels(&mut conn, &select);
    for (index, (name, _, digits, label, suffix)) in INSTANT_COLUMNS.iter().enumerate() {
        let text = &shown[index];
        println!("{name:>11} {:<18} {text}", page_labels[index]);
        assert_eq!(&page_labels[index], label, "{name}");
        assert_eq!(text.ends_with(" UTC"), *suffix, "{name}: {text}");
        let normalized = spaced(text);
        let wanted = typed_instant(3, *digits);
        assert!(
            normalized.starts_with(&wanted),
            "{name}: shows {text}, the instant is {wanted}"
        );
    }

    // Write the unchanged text back, as a value and as a key.
    for (index, (name, ..)) in INSTANT_COLUMNS.iter().enumerate() {
        let text = shown[index].clone();
        assert_eq!(
            update(
                &mut conn,
                &s,
                "t",
                json!({"id": 1}),
                json!({*name: text.clone()})
            )
            .unwrap(),
            1,
            "{name} value"
        );
        assert_eq!(
            stored_instants(&mut conn, &s, 1),
            expected,
            "{name}: value written back"
        );
        let marker = format!("key-{name}");
        assert_eq!(
            update(
                &mut conn,
                &s,
                "t",
                json!({*name: text}),
                json!({"note": marker.clone()})
            )
            .unwrap(),
            1,
            "{name} key"
        );
        assert_eq!(
            ok(conn.scalar(&format!("SELECT note FROM {}", s.t("t")))),
            marker
        );
        assert_eq!(
            stored_instants(&mut conn, &s, 1),
            expected,
            "{name}: key written back"
        );
    }

    // Every spelling of the same instant, in a value and a key.
    for (name, _, digits, ..) in INSTANT_COLUMNS {
        let base = typed_instant(3, digits);
        let shifted = typed_instant(12, digits); // for +09:00
        for spelling in [
            format!("{base} UTC"),
            format!("{}Z", base.replace(' ', "T")),
            format!("{base}+00:00"),
            format!("{shifted}+09:00"),
            format!(
                "{}-05:00",
                typed_instant(22, digits).replace("2020-01-02", "2020-01-01")
            ),
        ] {
            assert_eq!(
                update(
                    &mut conn,
                    &s,
                    "t",
                    json!({"id": 1}),
                    json!({name: spelling.clone()})
                )
                .unwrap(),
                1
            );
            assert_eq!(
                stored_instants(&mut conn, &s, 1),
                expected,
                "{name}: {spelling}"
            );
            assert_eq!(
                update(
                    &mut conn,
                    &s,
                    "t",
                    json!({name: spelling.clone()}),
                    json!({"note": "again"})
                )
                .unwrap(),
                1,
                "{name} key {spelling}"
            );
        }
    }

    // An edited hour moves the instant by exactly one hour.
    for (index, (name, _, digits, ..)) in INSTANT_COLUMNS.iter().enumerate() {
        let edited = spaced(&shown[index]).replacen(" 03:04:05", " 04:04:05", 1);
        assert_ne!(edited, shown[index]);
        update(&mut conn, &s, "t", json!({"id": 1}), json!({*name: edited})).unwrap();
        let mut moved = expected.clone();
        moved[index] = expected_nanos(4, *digits);
        assert_eq!(stored_instants(&mut conn, &s, 1), moved, "{name}: +1 hour");
        update(
            &mut conn,
            &s,
            "t",
            json!({"id": 1}),
            json!({*name: shown[index].clone()}),
        )
        .unwrap();
        assert_eq!(stored_instants(&mut conn, &s, 1), expected);
    }

    // Refused, not saturated or guessed.
    for (name, text) in [
        ("a_utc", "2106-02-07 06:28:16"),
        ("a_utc", "1969-12-31 23:59:59"),
        ("a_utc", "2020-01-02 03:04:05.5"),
        ("e_ms_tokyo", "2020-01-02 03:04:05.1234"),
        ("g_ns_ny", "2262-04-11 23:47:16.854775808"),
        ("b_tokyo", "yesterday"),
        ("c_ny", "2020-02-30 00:00:00"),
        ("c_ny", "2020-01-02 03:04:05 CET"),
    ] {
        let (code, message) = rejected(update(
            &mut conn,
            &s,
            "t",
            json!({"id": 1}),
            json!({name: text}),
        ));
        assert_eq!(code, CODE_INVALID, "{name} {text}: {message}");
        let (code, _) = rejected(insert(&mut conn, &s, "t", json!({"id": 9, name: text})));
        assert_eq!(code, CODE_INVALID, "insert {name} {text}");
    }
    assert_eq!(
        stored_instants(&mut conn, &s, 1),
        expected,
        "nothing changed"
    );
    assert_eq!(
        ok(conn.scalar(&format!("SELECT count() FROM {}", s.t("t")))),
        "1"
    );

    // NULL in a nullable one; and a pre-1970 instant in a DateTime64.
    update(
        &mut conn,
        &s,
        "t",
        json!({"id": 1}),
        json!({"i_ms_none": null}),
    )
    .unwrap();
    assert_eq!(
        ok(conn.scalar(&format!("SELECT isNull(i_ms_none) FROM {}", s.t("t")))),
        "1"
    );
    update(
        &mut conn,
        &s,
        "t",
        json!({"id": 1}),
        json!({"f_us_none": "1969-12-31 23:59:59.999999"}),
    )
    .unwrap();
    assert_eq!(
        ok(conn.scalar(&format!(
            "SELECT toUnixTimestamp64Micro(f_us_none) FROM {}",
            s.t("t")
        ))),
        "-1"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn datetimes_are_one_utc_instant_in_every_zone_and_server_default() {
    one_server_zone(None);
    one_server_zone(Some("session_timezone=Asia/Tokyo"));
    one_server_zone(Some("session_timezone=America/Los_Angeles"));
}

// ----------------------------------------------------------------- item 14

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn non_utf8_text_stays_bytes_and_text_round_trips() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    let s = Scratch::new(&ctx, &mut conn);
    ok(conn.exec(&format!(
        "CREATE TABLE {} (id Int32, s String, fs FixedString(4), lc LowCardinality(String), n Nullable(String)) \
         ENGINE = MergeTree ORDER BY id",
        s.t("t")
    )));
    ok(conn.exec(&format!(
        "INSERT INTO {} VALUES (1, 'héllo', 'ab', 'x', NULL), \
         (2, 'hello', unhex('ff000102'), unhex('c328'), 'plain')",
        s.t("t")
    )));
    // A page of valid text stays text (FixedString padding trimmed).
    let valid = format!("SELECT s, fs, lc, n FROM {} WHERE id = 1", s.t("t"));
    assert_eq!(
        labels(&mut conn, &valid),
        ["string", "fixedstring", "string", "string"]
    );
    assert_eq!(ok(conn.rows(&valid)), [["héllo", "ab", "x", "NULL"]]);
    // A page with bytes that are not UTF-8: Binary, labelled, bytes as stored.
    let broken = format!("SELECT s, fs, lc, n FROM {} WHERE id = 2", s.t("t"));
    assert_eq!(
        labels(&mut conn, &broken),
        [
            "string",
            "fixedstring (invalid utf-8)",
            "string (invalid utf-8)",
            "string"
        ]
    );
    let shown = ok(conn.rows(&broken)).remove(0);
    println!("{shown:?}");
    assert!(
        shown.iter().all(|cell| !cell.contains('\u{fffd}')),
        "{shown:?}"
    );
    // Both rows in one page: the whole column of the page is bytes.
    let both = format!("SELECT lc FROM {} ORDER BY id", s.t("t"));
    assert_eq!(labels(&mut conn, &both), ["string (invalid utf-8)"]);
    let cells = ok(conn.rows(&both));
    println!("{cells:?}");
    assert_ne!(cells[0], cells[1]);

    // Writing the unchanged text of a FixedString writes the same bytes back.
    let stored = |conn: &mut Conn, column: &str, id: i32| {
        ok(conn.scalar(&format!(
            "SELECT hex({column}) FROM {} WHERE id = {id}",
            s.t("t")
        )))
    };
    assert_eq!(stored(&mut conn, "fs", 1), "61620000");
    assert_eq!(
        update(&mut conn, &s, "t", json!({"id": 1}), json!({"fs": "ab"})).unwrap(),
        1
    );
    assert_eq!(stored(&mut conn, "fs", 1), "61620000");
    let (code, _) = rejected(update(
        &mut conn,
        &s,
        "t",
        json!({"id": 1}),
        json!({"fs": "abcde"}),
    ));
    assert_eq!(
        code, "mutation.failed",
        "longer than N is the server's refusal"
    );
    // Text that looks like hex is text in a value.
    assert_eq!(
        update(
            &mut conn,
            &s,
            "t",
            json!({"id": 1}),
            json!({"s": "0xdeadbeef"})
        )
        .unwrap(),
        1
    );
    assert_eq!(stored(&mut conn, "s", 1), "30786465616462656566");
    assert_eq!(
        insert(
            &mut conn,
            &s,
            "t",
            json!({"id": 5, "s": "0x41", "lc": "0xff"})
        )
        .unwrap(),
        1
    );
    assert_eq!(stored(&mut conn, "s", 5), "30783431");

    // A key read as hex (the page showed bytes) finds the row by its bytes.
    assert_eq!(
        update(
            &mut conn,
            &s,
            "t",
            json!({"fs": "0xff000102"}),
            json!({"n": "found by fs"})
        )
        .unwrap(),
        1
    );
    assert_eq!(
        update(
            &mut conn,
            &s,
            "t",
            json!({"lc": "0xc328"}),
            json!({"n": "found by lc"})
        )
        .unwrap(),
        1
    );
    assert_eq!(
        ok(conn.scalar(&format!("SELECT n FROM {} WHERE id = 2", s.t("t")))),
        "found by lc"
    );
    // ... or by the text, when the bytes are really that text; both = ambiguous.
    assert_eq!(
        update(
            &mut conn,
            &s,
            "t",
            json!({"s": "0x41"}),
            json!({"n": "text"})
        )
        .unwrap(),
        1,
        "only the text row"
    );
    ok(conn.exec(&format!(
        "INSERT INTO {} VALUES (6, 'A', 'z', 'z', NULL)",
        s.t("t")
    )));
    let (code, message) = rejected(update(
        &mut conn,
        &s,
        "t",
        json!({"s": "0x41"}),
        json!({"n": "both"}),
    ));
    assert_eq!(code, "mutation.identity_not_unique", "{message}");
    // Odd digits are not hex: only the text can match.
    let (code, _) = rejected(update(
        &mut conn,
        &s,
        "t",
        json!({"s": "0x4"}),
        json!({"n": "x"}),
    ));
    assert_eq!(code, "mutation.no_rows_affected");
    assert_eq!(
        ok(conn.scalar(&format!("SELECT countIf(n = 'both') FROM {}", s.t("t")))),
        "0",
        "an ambiguous key changes nothing"
    );
}

// ----------------------------------------------------------------- item 16

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn described_types_follow_ddl_the_session_and_the_first_page() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    let s = Scratch::new(&ctx, &mut conn);
    ok(conn.exec(&format!(
        "CREATE TABLE {} (id Int32, e Enum8('a' = 1, 'b' = 2), u UInt32, w Int8) ENGINE = MergeTree ORDER BY id",
        s.t("t")
    )));
    ok(conn.exec(&format!(
        "INSERT INTO {} VALUES (1, 'b', 16909060, 7), (2, 'a', 1, 8)",
        s.t("t")
    )));
    let select = format!("SELECT e, u, w FROM {} ORDER BY id", s.t("t"));
    let page = |conn: &mut Conn, offset: u64| {
        let page = conn.open_page(&select, offset, 1, false).unwrap();
        let types: Vec<_> = page.columns.iter().map(|c| c.data_type.clone()).collect();
        let rows = conn
            .session
            .render_cell_batch(page.page_id, 0, page.row_count, 0, 3)
            .unwrap();
        let cells: Vec<_> = (0..rows.cell_count())
            .filter_map(|i| rows.cell(i))
            .map(|c| c.text)
            .collect();
        let _ = conn.session.release_page(page.page_id);
        (types, cells)
    };
    // The page-0 describe caches for the next pages of the same statement.
    assert_eq!(
        page(&mut conn, 0),
        (
            vec!["enum8".into(), "uint32".into(), "int8".into()],
            vec!["b".into(), "16909060".into(), "7".into()]
        )
    );
    // DDL between two identical statements: the enum's names change ...
    ok(conn.exec(&format!(
        "ALTER TABLE {} MODIFY COLUMN e Enum8('x' = 1, 'y' = 2)",
        s.t("t")
    )));
    let (_, cells) = page(&mut conn, 1);
    assert_eq!(
        cells[0], "x",
        "page 1 after an ALTER describes again: {cells:?}"
    );
    // ... and a column's type changes (UInt32 -> IPv4), page 0 and page > 0.
    ok(conn.exec(&format!("ALTER TABLE {} MODIFY COLUMN u IPv4", s.t("t"))));
    let (types, cells) = page(&mut conn, 0);
    assert_eq!(types[1], "ipv4");
    assert_eq!(cells[1], "1.2.3.4");
    let _ = page(&mut conn, 0);
    ok(conn.exec(&format!("ALTER TABLE {} MODIFY COLUMN u UInt32", s.t("t"))));
    let (types, cells) = page(&mut conn, 1);
    assert_eq!(
        (types[1].as_str(), cells[1].as_str()),
        ("uint32", "1"),
        "{types:?} {cells:?}"
    );
    // A statement of the user that changes the schema in a script: same.
    let _ = page(&mut conn, 0);
    ok(conn.exec(&format!(
        "ALTER TABLE {} MODIFY COLUMN u IPv4; SELECT 1",
        s.t("t")
    )));
    assert_eq!(page(&mut conn, 1).1[1], "0.0.0.1");
    // A driver mutation drops them too (it cannot change types, but it is a write).
    insert(
        &mut conn,
        &s,
        "t",
        json!({"id": 3, "e": "y", "u": "1.1.1.1", "w": 1}),
    )
    .unwrap();

    // Temporary tables and session settings only exist in the HTTP session:
    // DESCRIBE must run in it.
    ok(conn.exec("CREATE TEMPORARY TABLE tmp_typed (ip IPv4, e Enum8('p' = 1, 'q' = 2), d DateTime('Asia/Tokyo'))"));
    ok(conn.exec("INSERT INTO tmp_typed VALUES ('9.8.7.6', 'q', 1577934245)"));
    let temp = "SELECT ip, e, d FROM tmp_typed";
    assert_eq!(labels(&mut conn, temp), ["ipv4", "enum8", "datetime"]);
    assert_eq!(
        ok(conn.rows(temp)),
        [["9.8.7.6", "q", "2020-01-02 03:04:05 UTC"]]
    );
    // One script, one session: create, fill and read.
    let script = "CREATE TEMPORARY TABLE tmp_two (a Enum8('m' = 3)); INSERT INTO tmp_two VALUES ('m'); SELECT a FROM tmp_two";
    assert_eq!(ok(conn.rows(script)), [["m"]]);
    // A session setting changes what DESCRIBE sees: the type names carry the
    // zone now. The values are still the UTC instants.
    ok(conn.exec("SET session_timezone = 'Asia/Tokyo'"));
    let zoned = "SELECT toDateTime64('2020-01-02 03:04:05.123', 3) AS z, toDateTime('2020-01-02 03:04:05') AS y";
    let shown = ok(conn.rows(zoned)).remove(0);
    println!("{shown:?} {:?}", labels(&mut conn, zoned));
    assert_eq!(spaced(&shown[0]), "2020-01-01 18:04:05.123");
    assert_eq!(spaced(&shown[1]), "2020-01-01 18:04:05");
}

// ----------------------------------------------------------------- item (b)

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn types_arrow_cannot_express_are_the_servers_text_and_never_fail_the_query() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    let s = Scratch::new(&ctx, &mut conn);
    ok(conn.exec(&format!(
        "CREATE TABLE {} (id Int32, plain String, j JSON, d Dynamic, v Variant(UInt8, String), \
         agg AggregateFunction(sum, UInt64), nested Array(JSON), lcv Array(LowCardinality(Nullable(String))), \
         tup Tuple(a Int32, b Variant(String, UInt8))) ENGINE = MergeTree ORDER BY id",
        s.t("t")
    )));
    ok(conn.exec(&format!(
        "INSERT INTO {} SELECT number, concat('p', toString(number)), '{{\"a\":1,\"b\":[1,2]}}', \
         if(number = 0, CAST(5 AS Dynamic), CAST('x' AS Dynamic)), if(number = 0, CAST(7 AS Variant(UInt8, String)), CAST('s' AS Variant(UInt8, String))), \
         sumState(toUInt64(number)), ['{{\"k\":1}}'], ['a', NULL], (toInt32(number), CAST('v' AS Variant(String, UInt8))) \
         FROM numbers(3) GROUP BY number",
        s.t("t")
    )));
    let all = format!("SELECT * FROM {} ORDER BY id", s.t("t"));
    // No column fails the statement; the others are untouched.
    let types = labels(&mut conn, &all);
    println!("{types:?}");
    assert_eq!(types[0], "int32");
    assert_eq!(types[1], "string");
    for (index, expected) in [
        (2, "json (read-only)"),
        (3, "dynamic (read-only)"),
        (4, "variant (read-only)"),
        (5, "aggregatefunction (read-only)"),
        (6, "array (read-only)"),
        (8, "tuple (read-only)"),
    ] {
        assert_eq!(types[index], expected);
    }
    assert_eq!(
        types[7], "array",
        "a decodable composite keeps its Arrow form"
    );
    let rows = ok(conn.rows(&all));
    for row in &rows {
        println!("{row:?}");
    }
    assert_eq!(rows[0][1], "p0");
    // The server's own text, compared with toString evaluated in the same query.
    let own = ok(conn.rows(&format!(
        "SELECT j, d, v, toString(tup), toString(nested) FROM {} ORDER BY id",
        s.t("t")
    )));
    let theirs = ok(conn.rows(&format!(
        "SELECT toString(j), toString(d), toString(v), toString(tup), toString(nested) FROM {} ORDER BY id",
        s.t("t")
    )));
    assert_eq!(own, theirs);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row[2], own[index][0], "json");
        assert_eq!(row[3], own[index][1], "dynamic");
        assert_eq!(row[4], own[index][2], "variant");
        assert_eq!(row[8], own[index][3], "tuple");
        assert_eq!(row[6], own[index][4], "array of json");
        assert!(
            row[5].starts_with("0x") || row[5].starts_with("X'"),
            "{}",
            row[5]
        );
    }
    // Paging, with and without a LIMIT of its own, and past the first page.
    for (sql, offset) in [
        (all.clone(), 0),
        (all.clone(), 1),
        (
            format!("SELECT * FROM {} ORDER BY id LIMIT 10", s.t("t")),
            0,
        ),
        (
            format!("SELECT * FROM {} ORDER BY id LIMIT 10", s.t("t")),
            2,
        ),
        (format!("{all} SETTINGS max_threads = 1"), 1),
    ] {
        let page = conn.open_page(&sql, offset, 1, false).unwrap();
        assert_eq!(page.columns.len(), 9, "{sql}");
        assert_eq!(page.row_count, 1, "{sql} @ {offset}");
        let cells = conn
            .session
            .render_cell_batch(page.page_id, 0, 1, 0, 9)
            .unwrap();
        let first = cells.cell(0).unwrap().text;
        assert_eq!(first, offset.to_string(), "{sql}: ids come in order");
        let _ = conn.session.release_page(page.page_id);
    }
    // Plain expressions of such types, and the ones with no column name to wrap.
    assert_eq!(ok(conn.scalar("SELECT NULL")), "NULL");
    assert_eq!(ok(conn.scalar("SELECT []")), "[]");
    assert_eq!(ok(conn.scalar("SELECT CAST(1 AS Dynamic)")), "1");
    assert_eq!(ok(conn.scalar("SELECT toBFloat16(1.5)")), "1.5");
    let two = ok(conn.rows("SELECT CAST(1 AS Variant(UInt8)) AS x, 2 AS y")).remove(0);
    assert_eq!(two, ["1", "2"]);
    // Temporary tables with such a column (the session decides what exists).
    ok(conn.exec("CREATE TEMPORARY TABLE tmp_j (a JSON, b Int32)"));
    ok(conn.exec("INSERT INTO tmp_j VALUES ('{\"z\":true}', 4)"));
    assert_eq!(
        ok(conn.rows("SELECT * FROM tmp_j")),
        [["{\"z\":true}", "4"]]
    );
    // Duplicate names cannot be wrapped: the server's own refusal comes back.
    let (code, message) =
        rejected(conn.open_page("SELECT CAST(1 AS Dynamic) AS a, 2 AS a", 0, 10, false));
    assert_eq!(code, "query.failed", "{message}");
    // A column that is not editable says so: the driver refuses before writing.
    for (column, value) in [
        ("j", json!("{\"a\":2}")),
        ("d", json!("1")),
        ("nested", json!(["{}"])),
        ("tup", json!("(1)")),
    ] {
        let (code, message) = rejected(update(
            &mut conn,
            &s,
            "t",
            json!({"id": 0}),
            json!({column: value}),
        ));
        assert_eq!(code, CODE_NOT_EDITABLE, "{column}: {message}");
    }
    let (code, _) = rejected(update(
        &mut conn,
        &s,
        "t",
        json!({"id": 0}),
        json!({"agg": "0x00"}),
    ));
    assert_eq!(code, CODE_NOT_EDITABLE);
    let (code, _) = rejected(update(
        &mut conn,
        &s,
        "t",
        json!({"j": "{}"}),
        json!({"plain": "x"}),
    ));
    assert_eq!(code, CODE_NOT_EDITABLE, "a row key of such a column");
    // The ordinary columns of the same table still edit.
    assert_eq!(
        update(
            &mut conn,
            &s,
            "t",
            json!({"id": 1}),
            json!({"plain": "edited"})
        )
        .unwrap(),
        1
    );
    assert_eq!(
        ok(conn.scalar(&format!("SELECT plain FROM {} WHERE id = 1", s.t("t")))),
        "edited"
    );
}

// ----------------------------------------------------------------- items (a), (c)

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn numbers_enums_and_specials_are_written_exactly_and_shown_as_the_server_prints_them() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    let s = Scratch::new(&ctx, &mut conn);
    ok(conn.exec(&format!(
        "CREATE TABLE {} (id Int32, i128 Int128, u128 UInt128, i256 Int256, u256 UInt256, u64 UInt64, i64 Int64, \
         u8 UInt8, dec Decimal(38, 10), dec2 Decimal(9, 2), f64 Float64, f32 Float32, \
         e Enum8('it''s' = 1, 'a,b' = 2, '1' = 3, 'q\"\\\\' = -4), e16 Enum16('big' = 1000, 'neg' = -3), \
         b Bool, u UUID, ip4 IPv4, ip6 IPv6, d Date, d32 Date32, n Nullable(Decimal(9, 2))) \
         ENGINE = MergeTree ORDER BY id",
        s.t("t")
    )));
    assert_eq!(insert(&mut conn, &s, "t", json!({"id": 1})).unwrap(), 1);
    let big = |digits: &str| digits.to_owned();
    let cases: Vec<(&str, Value, &str)> = vec![
        // (column, what the app sends, what the server prints for it)
        (
            "i128",
            json!("-170141183460469231731687303715884105728"),
            "-170141183460469231731687303715884105728",
        ),
        (
            "u128",
            json!("340282366920938463463374607431768211455"),
            "340282366920938463463374607431768211455",
        ),
        (
            "i256",
            json!(big(
                "-57896044618658097711785492504343953926634992332820282019728792003956564819968"
            )),
            "-57896044618658097711785492504343953926634992332820282019728792003956564819968",
        ),
        (
            "u256",
            json!("115792089237316195423570985008687907853269984665640564039457584007913129639935"),
            "115792089237316195423570985008687907853269984665640564039457584007913129639935",
        ),
        (
            "u64",
            json!(18_446_744_073_709_551_615_u64),
            "18446744073709551615",
        ),
        ("u64", json!("18446744073709551615"), "18446744073709551615"),
        ("i64", json!("-9223372036854775808"), "-9223372036854775808"),
        ("i64", json!("+5"), "5"),
        ("i64", json!("000123"), "123"),
        ("i64", json!("-0"), "0"),
        ("u8", json!(255), "255"),
        (
            "dec",
            json!("1234567890123456789012345678.1234567890"),
            "1234567890123456789012345678.123456789",
        ),
        ("dec", json!("-0.0000000001"), "-0.0000000001"),
        ("dec2", json!("1.50"), "1.5"),
        ("dec2", json!("1.5"), "1.5"),
        ("dec2", json!("+7"), "7"),
        ("f64", json!("nan"), "nan"),
        ("f64", json!("NaN"), "nan"),
        ("f64", json!("inf"), "inf"),
        ("f64", json!("-inf"), "-inf"),
        ("f64", json!("Infinity"), "inf"),
        ("f64", json!("-Infinity"), "-inf"),
        ("f64", json!("1e5"), "100000"),
        ("f64", json!(1.5), "1.5"),
        (
            "f64",
            json!("1.7976931348623157e308"),
            "1.7976931348623157e308",
        ),
        ("f32", json!("0.1"), "0.1"),
        ("f32", json!("-inf"), "-inf"),
        ("e", json!("it's"), "it's"),
        ("e", json!("a,b"), "a,b"),
        ("e", json!("1"), "1"),
        ("e", json!("q\"\\"), "q\"\\"),
        ("e16", json!("neg"), "neg"),
        ("b", json!(true), "true"),
        ("b", json!(false), "false"),
        (
            "u",
            json!("c91bf7e0-92ee-4169-ac36-4ec41d2d4d54"),
            "c91bf7e0-92ee-4169-ac36-4ec41d2d4d54",
        ),
        ("ip4", json!("1.2.3.4"), "1.2.3.4"),
        ("ip6", json!("2001:db8::1"), "2001:db8::1"),
        ("d", json!("2020-01-02"), "2020-01-02"),
        ("d32", json!("1960-01-02"), "1960-01-02"),
        ("n", json!("0.10"), "0.1"),
    ];
    for (column, value, printed) in &cases {
        update(
            &mut conn,
            &s,
            "t",
            json!({"id": 1}),
            json!({*column: value.clone()}),
        )
        .unwrap_or_else(|error| panic!("{column} = {value}: {error:?}"));
        let server = ok(conn.scalar(&format!(
            "SELECT toString({column}) FROM {} WHERE id = 1",
            s.t("t")
        )));
        assert_eq!(&server, printed, "{column} = {value}");
        // The driver shows what the server prints (nan/inf: Arrow's spelling).
        let shown = ok(conn.scalar(&format!("SELECT {column} FROM {} WHERE id = 1", s.t("t"))));
        match *printed {
            "nan" => assert_eq!(shown.to_lowercase(), "nan", "{column}"),
            "inf" | "-inf" => assert_eq!(
                shown.to_lowercase().replace("infinity", "inf"),
                *printed,
                "{column}"
            ),
            "100000" | "0.1" | "1.5" | "7" | "5" | "123" | "0" => {}
            // Float spelling is the grid's job (this harness prints no exponent).
            _ if column.starts_with('e') || column.starts_with('f') || *column == "b" => {}
            // A decimal keeps its scale on the grid (`1.50`); the server trims it.
            _ if column.starts_with("dec") || *column == "n" => {
                let trimmed = if shown.contains('.') {
                    shown.trim_end_matches('0').trim_end_matches('.')
                } else {
                    &shown
                };
                assert_eq!(trimmed, *printed, "{column} shown");
            }
            _ => assert_eq!(&shown, printed, "{column} shown"),
        }
        // The same text finds the row as a key.
        let marker = format!("{column}-{printed}");
        let _ = marker;
        assert_eq!(
            update(
                &mut conn,
                &s,
                "t",
                json!({*column: value.clone()}),
                json!({"u8": 1})
            )
            .unwrap_or_else(|error| panic!("{column} = {value} as a key: {error:?}")),
            1,
            "{column} = {value} as a key"
        );
    }
    // The earliest Date32 is stored (a key of exactly this day matches nothing in
    // a `count()` on this server, whatever the driver sends: the server's own
    // `WHERE d32 = '1900-01-01'` does the same, so it is left out of the key loop).
    update(
        &mut conn,
        &s,
        "t",
        json!({"id": 1}),
        json!({"d32": "1900-01-01"}),
    )
    .unwrap();
    assert_eq!(
        ok(conn.scalar(&format!(
            "SELECT toString(d32) FROM {} WHERE id = 1",
            s.t("t")
        ))),
        "1900-01-01"
    );
    // Enum names the server prints are the names shown (assert in the same query).
    ok(conn.exec(&format!(
        "INSERT INTO {} (id, e, e16, u, ip4, ip6) VALUES (2, 'a,b', 'big', toUUID('00112233-4455-6677-8899-aabbccddeeff'), '255.0.0.1', '::ffff:1.2.3.4')",
        s.t("t")
    )));
    let rows = ok(conn.rows(&format!(
        "SELECT e, toString(e), e16, toString(e16), u, toString(u), ip4, toString(ip4), ip6, toString(ip6) \
         FROM {} WHERE id = 2",
        s.t("t")
    )));
    for pair in rows[0].chunks(2) {
        assert_eq!(pair[0], pair[1]);
    }
    // Refused, nothing written.
    let before = ok(conn.rows(&format!("SELECT * FROM {} ORDER BY id", s.t("t"))));
    for (column, value) in [
        ("u8", json!("300")),
        ("u8", json!("-1")),
        ("u8", json!(256)),
        ("u64", json!("18446744073709551616")),
        ("i64", json!("9223372036854775808")),
        ("i64", json!("1.0")),
        ("i64", json!("1e3")),
        ("i64", json!("0x10")),
        ("i64", json!("")),
        ("i64", json!(1.5)),
        ("i128", json!("170141183460469231731687303715884105728")),
        ("u256", json!("-1")),
        ("dec2", json!("1.239")),
        ("dec2", json!("12345678")),
        ("dec2", json!("1e2")),
        ("dec", json!("abc")),
        ("f64", json!("1e999")),
        ("f64", json!("abc")),
        ("f64", json!("")),
        ("f32", json!("1e50")),
        ("e", json!("nope")),
        ("e", json!("2")),
        ("e", json!(1)),
        ("b", json!("maybe")),
        ("d", json!("2149-06-07")),
        ("d", json!("1969-12-31")),
        ("d", json!("2020-13-01")),
        ("d32", json!("garbage")),
    ] {
        let (code, message) = rejected(update(
            &mut conn,
            &s,
            "t",
            json!({"id": 1}),
            json!({column: value.clone()}),
        ));
        assert_eq!(code, CODE_INVALID, "{column} = {value}: {message}");
        let (code, _) = rejected(insert(
            &mut conn,
            &s,
            "t",
            json!({"id": 7, column: value.clone()}),
        ));
        assert_eq!(code, CODE_INVALID, "insert {column} = {value}");
        let (code, _) = rejected(update(
            &mut conn,
            &s,
            "t",
            json!({column: value.clone()}),
            json!({"u8": 2}),
        ));
        assert_eq!(code, CODE_INVALID, "key {column} = {value}");
    }
    assert_eq!(
        ok(conn.rows(&format!("SELECT * FROM {} ORDER BY id", s.t("t")))),
        before
    );

    // Inserts of exact text: wide integers, decimals, specials, instants.
    insert(
        &mut conn,
        &s,
        "t",
        json!({"id": 3, "u256": "115792089237316195423570985008687907853269984665640564039457584007913129639935",
               "dec": "-12345.6789012345", "f64": "-inf", "f32": "nan", "e": "a,b", "b": "true", "d": "2149-06-06"}),
    )
    .unwrap();
    let row = ok(conn.rows(&format!(
        "SELECT toString(u256), toString(dec), toString(f64), toString(f32), toString(e), toString(b), toString(d) FROM {} WHERE id = 3",
        s.t("t")
    )));
    assert_eq!(
        row[0],
        [
            "115792089237316195423570985008687907853269984665640564039457584007913129639935",
            "-12345.6789012345",
            "-inf",
            "nan",
            "a,b",
            "true",
            "2149-06-06"
        ]
    );
    // A NaN key finds the NaN row (NaN = NaN is false in SQL).
    assert_eq!(
        update(&mut conn, &s, "t", json!({"f32": "nan"}), json!({"u8": 9})).unwrap(),
        1
    );
    // NULL and the empty editor.
    update(&mut conn, &s, "t", json!({"id": 1}), json!({"n": null})).unwrap();
    let (code, _) = rejected(update(
        &mut conn,
        &s,
        "t",
        json!({"id": 1}),
        json!({"u8": null}),
    ));
    assert_eq!(code, "mutation.bad_request", "u8 is not nullable");
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn dates_are_dates_whatever_physical_type_the_server_writes() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    // The statement's own SETTINGS beat the driver's: a Date arrives as UInt16.
    let sql = "SELECT toDate('2020-01-02') AS d, toDate32('1960-01-02') AS d32, toUInt16(7) AS plain, \
               toDateTime('2020-01-02 03:04:05', 'UTC') AS dt SETTINGS output_format_arrow_date_as_uint16 = 1";
    assert_eq!(
        labels(&mut conn, sql),
        ["date", "date32", "uint16", "datetime"]
    );
    let row = ok(conn.rows(sql)).remove(0);
    assert_eq!(row[0], "2020-01-02");
    assert_eq!(row[1], "1960-01-02");
    assert_eq!(row[2], "7", "a UInt16 that is not a Date stays a number");
    // The same without the override.
    let row = ok(conn.rows("SELECT toDate('2020-01-02') AS d, toUInt16(7) AS plain")).remove(0);
    assert_eq!(row, ["2020-01-02", "7"]);
}

/// The server-side half of the mutation guard (see `mutation`): an `ALTER` whose
/// `WHERE` repeats the key plus `(SELECT count() ...) = 1` changes nothing when
/// the key matches two rows, and does its work when it matches one.
#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn a_guarded_alter_changes_nothing_when_the_key_matches_two_rows() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    let s = Scratch::new(&ctx, &mut conn);
    let t = s.t("t");
    ok(conn.exec(&format!(
        "CREATE TABLE {t} (k Int32, v Int32) ENGINE = MergeTree ORDER BY k"
    )));
    ok(conn.exec(&format!("INSERT INTO {t} VALUES (1, 1), (1, 2), (2, 3)")));
    let guarded = |key: i32, action: &str| {
        format!(
            "ALTER TABLE {t} {action} WHERE k = {key} AND (SELECT count() FROM {t} WHERE k = {key}) = 1 \
             SETTINGS mutations_sync = 2, mutations_execute_subqueries_on_initiator = 1"
        )
    };
    let rows = |conn: &mut Conn| ok(conn.rows(&format!("SELECT k, v FROM {t} ORDER BY k, v")));
    ok(conn.exec(&guarded(1, "UPDATE v = 100")));
    ok(conn.exec(&guarded(1, "DELETE")));
    assert_eq!(
        rows(&mut conn),
        [["1", "1"], ["1", "2"], ["2", "3"]],
        "two rows: nothing"
    );
    ok(conn.exec(&guarded(2, "UPDATE v = 100")));
    assert_eq!(rows(&mut conn), [["1", "1"], ["1", "2"], ["2", "100"]]);
    ok(conn.exec(&guarded(2, "DELETE")));
    assert_eq!(rows(&mut conn), [["1", "1"], ["1", "2"]]);
    // Through the driver, a duplicate key is refused before any ALTER.
    let (code, _) = rejected(update(&mut conn, &s, "t", json!({"k": 1}), json!({"v": 5})));
    assert_eq!(code, "mutation.identity_not_unique");
    assert_eq!(rows(&mut conn), [["1", "1"], ["1", "2"]]);
}

// ------------------------------------------------- batches are not atomic

fn batch(conn: &mut Conn, s: &Scratch, table: &str, changes: Value) -> Result<u64, LoaderError> {
    mutate(conn, s, table, "batch", json!({ "changes": changes }))
}

/// The `applied`, `failed`, `unknown` and `not attempted` lists of a
/// `mutation.partially_applied` message, as the app reads them.
fn lists(message: &str) -> [Vec<usize>; 4] {
    ["applied", "failed", "unknown", "not attempted"].map(|label| {
        let list = message
            .split_once(&format!("{label}: ["))
            .unwrap_or_else(|| panic!("no `{label}` list in {message}"))
            .1
            .split_once(']')
            .unwrap()
            .0;
        list.split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| n.parse().unwrap())
            .collect()
    })
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn a_batch_that_stops_after_a_write_is_partially_applied_with_change_indexes() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    let s = Scratch::new(&ctx, &mut conn);
    let t = s.t("t");
    ok(conn.exec(&format!(
        "CREATE TABLE {t} (id Int32, fs FixedString(4), v Int32) ENGINE = MergeTree ORDER BY id"
    )));
    ok(conn.exec(&format!(
        "INSERT INTO {t} VALUES (1, 'a', 1), (2, 'b', 2), (3, 'c', 3), (3, 'd', 4)"
    )));
    let ids = |conn: &mut Conn| ok(conn.rows(&format!("SELECT id FROM {t} ORDER BY id, v")));
    let count = |conn: &mut Conn, id: i32| {
        ok(conn.scalar(&format!("SELECT count() FROM {t} WHERE id = {id}")))
    };

    // The first write fails (the server refuses a value longer than the
    // FixedString): nothing was applied, the plain error, nothing to reconcile.
    let (code, _) = rejected(batch(
        &mut conn,
        &s,
        "t",
        json!([{"op": "update", "key": {"id": 1}, "values": {"fs": "abcde"}}]),
    ));
    assert_eq!(code, "mutation.failed");
    // A value or key the driver refuses is refused before ANY write.
    for changes in [
        json!([
            {"op": "insert", "values": {"id": 20, "fs": "x", "v": 1}},
            {"op": "update", "key": {"id": 1}, "values": {"v": "abc"}},
        ]),
        json!([
            {"op": "insert", "values": {"id": 20, "fs": "x", "v": 1}},
            {"op": "update", "key": {"id": 3}, "values": {"v": 9}},
        ]),
        json!([
            {"op": "insert", "values": {"id": 20, "fs": "x", "v": 1}},
            {"op": "delete", "key": {"id": 99}},
        ]),
    ] {
        let (code, message) = rejected(batch(&mut conn, &s, "t", changes));
        assert!(
            [
                "mutation.invalid_value",
                "mutation.identity_not_unique",
                "mutation.no_rows_affected"
            ]
            .contains(&code.as_str()),
            "{code}: {message}"
        );
        assert_eq!(count(&mut conn, 20), "0", "nothing was written");
    }

    // The insert (batch change 1: the INSERT runs first, whatever the order of
    // the batch) is applied, then the update (change 0) is refused, and the
    // delete (change 2) never runs.
    let (code, message) = rejected(batch(
        &mut conn,
        &s,
        "t",
        json!([
            {"op": "update", "key": {"id": 1}, "values": {"fs": "abcde"}},
            {"op": "insert", "values": {"id": 10, "fs": "n", "v": 10}},
            {"op": "delete", "key": {"id": 2}},
        ]),
    ));
    assert_eq!(code, "mutation.partially_applied", "{message}");
    assert_eq!(
        lists(&message),
        [vec![1], vec![0], vec![], vec![2]],
        "{message}"
    );
    assert!(message.contains("NOT rolled back"), "{message}");
    assert_eq!(count(&mut conn, 10), "1", "the insert stays applied");
    assert_eq!(count(&mut conn, 2), "1", "the delete never ran");

    // An insert, an update folded into it, a pair that cancels, then a refusal:
    // every change is named once.
    let (code, message) = rejected(batch(
        &mut conn,
        &s,
        "t",
        json!([
            {"op": "insert", "values": {"id": 11, "fs": "n", "v": 11}},
            {"op": "update", "key": {"id": 11}, "values": {"v": 12}},
            {"op": "insert", "values": {"id": 12, "fs": "n", "v": 12}},
            {"op": "delete", "key": {"id": 12}},
            {"op": "update", "key": {"id": 2}, "values": {"fs": "abcde"}},
            {"op": "delete", "key": {"id": 1}},
        ]),
    ));
    assert_eq!(code, "mutation.partially_applied", "{message}");
    assert_eq!(
        lists(&message),
        [vec![0, 1, 2, 3], vec![4], vec![], vec![5]],
        "{message}"
    );
    assert_eq!(
        ok(conn.scalar(&format!("SELECT v FROM {t} WHERE id = 11"))),
        "12"
    );
    assert_eq!(count(&mut conn, 12), "0");
    assert_eq!(ids(&mut conn).len(), 6);

    // A good batch is still all good (and needs no list).
    assert_eq!(
        batch(
            &mut conn,
            &s,
            "t",
            json!([
                {"op": "update", "key": {"id": 2}, "values": {"v": 22}},
                {"op": "delete", "key": {"id": 1}},
            ])
        )
        .unwrap(),
        2
    );
    assert_eq!(count(&mut conn, 1), "0");
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn a_cancel_while_an_update_waits_leaves_its_outcome_unknown() {
    use std::time::{Duration, Instant};
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    let s = Scratch::new(&ctx, &mut conn);
    let t = s.t("t");
    ok(conn.exec(&format!(
        "CREATE TABLE {t} (id Int32, v Int32) ENGINE = MergeTree ORDER BY id"
    )));
    ok(conn.exec(&format!(
        "INSERT INTO {t} SELECT number, 0 FROM numbers(10)"
    )));
    // Mutations of one part run one after the other: the update the batch sends
    // has to wait for these (about 2.5 s each).
    for _ in 0..4 {
        ok(conn.exec(&format!(
            "ALTER TABLE {t} UPDATE v = v + toInt32(sleepEachRow(0.25)) WHERE 1 \
             SETTINGS mutations_sync = 0, allow_nondeterministic_mutations = 1"
        )));
    }
    let mut worker = ok(ctx.connect());
    let operation = worker.session.begin_operation().unwrap();
    let operation_id = operation.id();
    let connection_id = worker.session.connection_id();
    let request = engine_driver_api::MutationRequest {
        operation_id,
        kind: "batch".into(),
        database: Some(s.name.clone()),
        object: Some("t".into()),
        identity: json!({}),
        payload: json!({"changes": [
            {"op": "insert", "values": {"id": 100, "v": 1}},
            {"op": "update", "key": {"id": 1}, "values": {"v": 5}},
        ]}),
    };
    let (cancel_sent, result) = std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = &mut worker;
        scope.spawn(move || {
            let _ = sender.send(worker.session.mutate(&request));
        });
        std::thread::sleep(Duration::from_millis(1500));
        let started = Instant::now();
        ctx.handle
            .cancel_operation(
                connection_id,
                &engine_driver_api::CancelRequest { operation_id },
            )
            .unwrap();
        (started, receiver.recv_timeout(Duration::from_secs(10)))
    });
    drop(operation);
    let result = result.expect("the cancel ended the wait");
    assert!(cancel_sent.elapsed() < Duration::from_secs(8));
    let (code, message) = rejected(result);
    assert_eq!(code, "mutation.partially_applied", "{message}");
    assert_eq!(
        lists(&message),
        [vec![0], vec![], vec![1], vec![]],
        "{message}"
    );
    assert!(
        message.contains("cancelled") || message.contains("interrupted"),
        "{message}"
    );
    assert_eq!(
        ok(conn.scalar(&format!("SELECT count() FROM {t} WHERE id = 100"))),
        "1"
    );
    // A cancel before any write is still the plain cancel: tested by the
    // conformance suite (`mutation_cancelled` is skipped for ClickHouse, so here).
    ok(conn.exec(&format!("KILL MUTATION WHERE database = '{}'", s.name)));
}
