//! Live ABI v2 tests: the release `--features abi-v2` dylib is loaded through
//! the real host loader and driven over a dedicated Redis (db 9, one key
//! prefix per test, only that prefix is cleaned up).
//!
//! ```text
//! TABLEX_TEST_REDIS_URI=redis://127.0.0.1:56379 \
//!   cargo test -p dbengine-redis --test live_v2 -- --ignored --nocapture --test-threads=1
//! ```

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type};
use arrow_array::{Array, RecordBatch};
use engine_driver_api::{
    Capability, DriverCapabilities, MutationRequest, MutationResult, NavigatorNode, NavigatorPage,
    OperationId, QueryPage, RowFields,
};
use engine_driver_host::{DriverLoader, DriverSession, LoaderError};
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

const DB: u32 = 9;
static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);

fn uri() -> String {
    std::env::var("TABLEX_TEST_REDIS_URI").expect(
        "set TABLEX_TEST_REDIS_URI (dedicated test container, e.g. redis://127.0.0.1:56379)",
    )
}

/// Builds the release `abi-v2` dylib once per process.
fn driver_path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let target_dir = std::env::temp_dir().join("tablex-redis-v2-live-driver");
        let output = Command::new(env!("CARGO"))
            .arg("build")
            .arg("--manifest-path")
            .arg(&manifest)
            // The default release `strip` step leaves this dylib with a
            // mis-aligned LINKEDIT string pool that macOS dyld refuses to load
            // (toolchain ld-27037); keep the symbols for the test build.
            .env("CARGO_PROFILE_RELEASE_STRIP", "none")
            .args(["--features", "abi-v2", "--release", "--target-dir"])
            .arg(&target_dir)
            .output()
            .expect("run cargo build");
        assert!(
            output.status.success(),
            "Redis v2 driver build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        target_dir.join("release").join("libdbengine_redis.dylib")
    })
}

fn operation() -> OperationId {
    OperationId(NEXT_OPERATION.fetch_add(1, Ordering::Relaxed))
}

fn code(error: LoaderError) -> String {
    match error {
        LoaderError::DriverRejected { code, .. } => code,
        other => panic!("expected a driver rejection, got {other}"),
    }
}

struct Fixture {
    session: DriverSession,
    redis: redis::Connection,
    prefix: String,
}

impl Fixture {
    fn new(test: &str) -> Self {
        let handle = DriverLoader
            .load(driver_path())
            .expect("load the v2 driver");
        let session = handle
            .connect(&serde_json::json!({ "uri": uri() }).to_string())
            .expect("connect through the driver");
        let client =
            redis::Client::open(format!("{}/{DB}", uri().trim_end_matches('/'))).expect("client");
        let redis = client.get_connection().expect("direct connection");
        let mut fixture = Self {
            session,
            redis,
            prefix: format!("tablexp4:{test}:{}:", std::process::id()),
        };
        fixture.cleanup();
        fixture
    }

    fn cleanup(&mut self) {
        let pattern = format!("{}*", self.prefix);
        let mut cursor = 0_u64;
        loop {
            let (next, keys): (u64, Vec<Vec<u8>>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(1_000)
                .query(&mut self.redis)
                .expect("scan for cleanup");
            if !keys.is_empty() {
                let _: u64 = redis::cmd("DEL")
                    .arg(&keys)
                    .query(&mut self.redis)
                    .expect("cleanup DEL");
            }
            if next == 0 {
                break;
            }
            cursor = next;
        }
    }

    fn key(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.prefix)
    }

    fn cmd<T: redis::FromRedisValue>(&mut self, args: &[&[u8]]) -> T {
        let mut command = redis::cmd(std::str::from_utf8(args[0]).expect("verb"));
        for arg in &args[1..] {
            command.arg(*arg);
        }
        command.query(&mut self.redis).expect("direct command")
    }

    fn query(
        &mut self,
        kind: &str,
        object: Option<&str>,
        payload: serde_json::Value,
        offset: u64,
        limit: u64,
    ) -> Result<QueryPage, LoaderError> {
        let request = serde_json::json!({
            "kind": kind,
            "operation_id": operation().0,
            "database": DB.to_string(),
            "object": object,
            "offset": offset,
            "limit": limit,
            "payload": payload,
        });
        self.session.open_query(&request.to_string())
    }

    fn key_page(&mut self, key: &str, offset: u64, limit: u64) -> QueryPage {
        self.query(
            "redis.key",
            Some(key),
            serde_json::Value::Null,
            offset,
            limit,
        )
        .expect("open redis.key page")
    }

    fn command(
        &mut self,
        command: &str,
        confirmed: bool,
        offset: u64,
        limit: u64,
    ) -> Result<QueryPage, LoaderError> {
        self.query(
            "redis.command",
            None,
            serde_json::json!({"command": command, "confirmed": confirmed}),
            offset,
            limit,
        )
    }

    fn batch(&mut self, page: &QueryPage) -> RecordBatch {
        let lease = self
            .session
            .acquire_arrow_page(page.page_id, 0, page.row_count)
            .expect("acquire Arrow page");
        unsafe { lease.into_record_batch() }
            .expect("import Arrow page")
            .batch
    }

    fn mutate(
        &mut self,
        kind: &str,
        identity: serde_json::Value,
        payload: serde_json::Value,
    ) -> Result<MutationResult, LoaderError> {
        self.session.mutate(&MutationRequest {
            operation_id: operation(),
            kind: kind.into(),
            database: None,
            object: None,
            identity,
            payload,
        })
    }

    fn nav(&mut self, request: serde_json::Value) -> Result<NavigatorPage, LoaderError> {
        self.session.list_nav_nodes(&request.to_string())
    }

    fn id(&self, key: &str) -> serde_json::Value {
        serde_json::json!({"db": DB, "key": self.key(key)})
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn bytes(batch: &RecordBatch, column: usize, row: usize) -> Vec<u8> {
    batch.column(column).as_binary::<i32>().value(row).to_vec()
}

fn texts(batch: &RecordBatch, column: usize) -> Vec<String> {
    (0..batch.num_rows())
        .map(|row| String::from_utf8(bytes(batch, column, row)).expect("utf8 test data"))
        .collect()
}

fn column_names(page: &QueryPage) -> Vec<&str> {
    page.columns
        .iter()
        .map(|column| column.name.as_str())
        .collect()
}

fn root_nav() -> serde_json::Value {
    serde_json::json!({"parent": null, "database": null, "payload": null, "cursor": null, "limit": 0})
}

fn keys_nav(payload: serde_json::Value, cursor: Option<&str>, limit: u64) -> serde_json::Value {
    let mut payload = payload;
    payload["database"] = DB.to_string().into();
    serde_json::json!({
        "parent": format!("database:{DB}"),
        "database": DB.to_string(),
        "payload": payload,
        "cursor": cursor,
        "limit": limit,
    })
}

fn all_keys(
    fixture: &mut Fixture,
    payload: serde_json::Value,
    limit: u64,
) -> (Vec<NavigatorNode>, usize) {
    let mut nodes = Vec::new();
    let mut pages = 0;
    let mut cursor: Option<String> = None;
    loop {
        let page = fixture
            .nav(keys_nav(payload.clone(), cursor.as_deref(), limit))
            .expect("list keys");
        pages += 1;
        nodes.extend(page.nodes);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return (nodes, pages),
        }
        assert!(pages < 1_000, "SCAN did not terminate");
    }
}

fn info_field(redis: &mut redis::Connection, section: &str, field: &str) -> u64 {
    let info: String = redis::cmd("INFO").arg(section).query(redis).expect("INFO");
    info.lines()
        .find_map(|line| line.strip_prefix(&format!("{field}:")))
        .and_then(|value| {
            value
                .split(',')
                .next()?
                .trim()
                .strip_prefix("calls=")
                .unwrap_or(value.trim())
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

fn net_out(redis: &mut redis::Connection) -> u64 {
    info_field(redis, "stats", "total_net_output_bytes")
}

fn calls(redis: &mut redis::Connection, verb: &str) -> u64 {
    info_field(redis, "commandstats", &format!("cmdstat_{verb}"))
}

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn capabilities_connect_and_disconnect() {
    let mut fixture = Fixture::new("caps");
    let capabilities: DriverCapabilities = fixture.session.capabilities().clone();
    println!(
        "capabilities: {}",
        serde_json::to_string(&capabilities).unwrap()
    );
    assert_eq!(capabilities.abi_version, 2);
    assert_eq!(capabilities.driver, "redis");
    for capability in [
        Capability::Navigator,
        Capability::QueryArrowCData,
        Capability::QueryRedisKey,
        Capability::QueryRedisCommand,
    ] {
        assert!(capabilities.has(capability), "{capability}");
    }
    assert!(!capabilities.has(Capability::OperationCancel));
    assert!(!capabilities.has(Capability::QuerySql));
    assert!(capabilities.sql_dialect.is_none());
    assert_eq!(capabilities.editors, ["redis_command"]);
    assert_eq!(capabilities.navigator_kinds, ["database", "key"]);
    for kind in [
        "set_string",
        "set_ttl",
        "delete_key",
        "delete_keys",
        "hset",
        "hdel",
        "lpush",
        "rpush",
        "lset",
        "sadd",
        "srem",
        "zadd",
        "zrem",
        "xadd",
        "xdel",
    ] {
        assert!(capabilities.supports_mutation_kind(kind), "{kind}");
        assert!(Capability::for_mutation_kind(kind).is_some(), "{kind}");
    }

    // A bad scheme is a typed rejection; an unreachable server fails cleanly.
    let handle = DriverLoader.load(driver_path()).unwrap();
    let error = handle
        .connect(r#"{"uri":"http://127.0.0.1:1"}"#)
        .err()
        .expect("bad scheme");
    assert_eq!(code(error), "connect.bad_request");
    assert!(handle.connect(r#"{"uri":"redis://127.0.0.1:1"}"#).is_err());

    assert!(fixture.nav(root_nav()).is_ok());
    fixture.session.disconnect().expect("disconnect");
    assert!(
        fixture.nav(root_nav()).is_err(),
        "disconnected session must reject work"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn navigator_databases_and_key_scan_paging() {
    let mut fixture = Fixture::new("nav");
    for i in 0..250 {
        let key = fixture.key(&format!("s:{i:03}"));
        let _: () = redis::cmd("SET")
            .arg(key)
            .arg(i)
            .query(&mut fixture.redis)
            .unwrap();
    }
    for i in 0..3 {
        let key = fixture.key(&format!("h:{i}"));
        let _: u64 = redis::cmd("HSET")
            .arg(key)
            .arg("f")
            .arg("v")
            .query(&mut fixture.redis)
            .unwrap();
    }
    let list = fixture.key("l:0");
    let _: u64 = redis::cmd("RPUSH")
        .arg(list)
        .arg("x")
        .query(&mut fixture.redis)
        .unwrap();
    let binary_key = [fixture.prefix.as_bytes(), b"bin:\xac\xed\x00\x05"].concat();
    let _: () = redis::cmd("SET")
        .arg(&binary_key[..])
        .arg("v")
        .query(&mut fixture.redis)
        .unwrap();
    let expected = 250 + 3 + 1 + 1;

    // Databases.
    let root = fixture.nav(root_nav()).expect("root");
    assert!(root.next_cursor.is_none());
    assert!(root.nodes.len() >= 16);
    let db9 = root
        .nodes
        .iter()
        .find(|node| node.id == "database:9")
        .expect("db9 node");
    assert_eq!(db9.kind, "database");
    assert!(db9.label.starts_with("db9 ("), "{}", db9.label);
    assert_eq!(db9.payload["db"], 9);
    assert_eq!(db9.payload["database"], "9");
    assert!(db9.payload["keys"].as_u64().unwrap() >= expected);
    println!("database node: {}", serde_json::to_string(db9).unwrap());

    // SCAN paging with a pattern: pages follow the cursor and cover every key once.
    let pattern = serde_json::json!({"pattern": format!("{}*", fixture.prefix)});
    let first = fixture
        .nav(keys_nav(pattern.clone(), None, 100))
        .expect("first page");
    assert!(
        first.nodes.len() >= 100,
        "limit is a lower bound, got {}",
        first.nodes.len()
    );
    assert!(
        first.next_cursor.is_some(),
        "255 keys do not fit in one 100-key page"
    );
    let (nodes, pages) = all_keys(&mut fixture, pattern.clone(), 100);
    let ids: HashSet<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
    assert_eq!(nodes.len() as u64, expected);
    assert_eq!(
        ids.len() as u64,
        expected,
        "no duplicates on a quiet keyspace"
    );
    assert!(pages >= 2);
    assert!(nodes
        .iter()
        .all(|node| node.kind == "key" && !node.has_children));
    println!(
        "{pages} pages of >=100 keys; sample key node: {}",
        serde_json::to_string(&nodes[0]).unwrap()
    );

    // Small pages still terminate and give the same set.
    let (small, small_pages) = all_keys(&mut fixture, pattern.clone(), 10);
    assert_eq!(small.len() as u64, expected);
    assert!(small_pages > pages);

    // Type filter.
    let prefix = fixture.prefix.clone();
    let (hashes, _) = all_keys(
        &mut fixture,
        serde_json::json!({"pattern": format!("{prefix}*"), "type": "hash"}),
        50,
    );
    assert_eq!(hashes.len(), 3);
    assert!(
        hashes
            .iter()
            .all(|node| node.subtitle.as_deref() == Some("hash")
                && node.payload["key_type"] == "hash")
    );
    let (lists, _) = all_keys(
        &mut fixture,
        serde_json::json!({"pattern": format!("{prefix}l:*"), "type": "list"}),
        50,
    );
    assert_eq!(lists.len(), 1);
    let (none, _) = all_keys(
        &mut fixture,
        serde_json::json!({"pattern": format!("{prefix}*"), "type": "zset"}),
        50,
    );
    assert!(none.is_empty());

    // Binary key: lossless raw key and escaped label.
    let node = nodes
        .iter()
        .find(|node| node.label.contains("bin:"))
        .expect("binary key node");
    assert!(
        node.label.ends_with("bin:\\xac\\xed\\x00\\x05"),
        "{}",
        node.label
    );
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(node.payload["key_raw"].as_str().unwrap())
        .unwrap();
    assert_eq!(raw, binary_key);
    assert_eq!(node.payload["ttl"], serde_json::Value::Null);

    // Bad cursor / parent are typed errors.
    let mut bad = keys_nav(serde_json::Value::Null, Some("abc"), 10);
    bad["payload"] = serde_json::json!({"database": "9"});
    assert_eq!(
        code(fixture.nav(bad).err().unwrap()),
        "navigator.bad_request"
    );
    let unknown_parent = serde_json::json!({"parent": "key:1:x", "database": null, "payload": null, "cursor": null, "limit": 0});
    assert_eq!(
        code(fixture.nav(unknown_parent).err().unwrap()),
        "navigator.bad_request"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn key_pages_for_all_six_types() {
    let mut fixture = Fixture::new("types");

    // string (+ TTL), byte cap and offset past the single row.
    let s = fixture.key("str");
    let _: () = redis::cmd("SET")
        .arg(&s)
        .arg("0123456789")
        .arg("EX")
        .arg(500)
        .query(&mut fixture.redis)
        .unwrap();
    let page = fixture.key_page(&s, 0, 0);
    assert_eq!(column_names(&page), ["value"]);
    assert_eq!(page.columns[0].data_type, "bytes");
    assert_eq!(
        (page.row_count, page.total_count, page.total_is_exact),
        (1, Some(1), true)
    );
    assert!(!page.flags.truncated);
    assert_eq!(texts(&fixture.batch(&page), 0), ["0123456789"]);
    let capped = fixture
        .query(
            "redis.key",
            Some(&s),
            serde_json::json!({"max_bytes": 4}),
            0,
            0,
        )
        .unwrap();
    assert!(capped.flags.truncated);
    assert_eq!(texts(&fixture.batch(&capped), 0), ["0123"]);
    let past = fixture.key_page(&s, 1, 10);
    assert_eq!((past.row_count, past.total_count), (0, Some(1)));
    let ttl = fixture
        .session
        .inspect_row(page.page_id, 0)
        .unwrap()
        .mutation_identity["ttl"]
        .as_i64()
        .unwrap();
    assert!((1..=500).contains(&ttl), "ttl {ttl}");

    // hash: 5 fields paged by 2.
    let h = fixture.key("hash");
    for i in 0..5 {
        let _: u64 = redis::cmd("HSET")
            .arg(&h)
            .arg(format!("f{i}"))
            .arg(format!("v{i}"))
            .query(&mut fixture.redis)
            .unwrap();
    }
    let mut seen = BTreeSet::new();
    for offset in [0, 2, 4] {
        let page = fixture.key_page(&h, offset, 2);
        assert_eq!(column_names(&page), ["field", "value"]);
        assert_eq!(page.total_count, Some(5));
        assert_eq!(page.flags.truncated, offset < 4);
        let batch = fixture.batch(&page);
        assert_eq!(batch.num_rows(), if offset == 4 { 1 } else { 2 });
        for (field, value) in texts(&batch, 0).into_iter().zip(texts(&batch, 1)) {
            assert_eq!(value, field.replace('f', "v"));
            assert!(seen.insert(field));
        }
    }
    assert_eq!(seen.len(), 5);

    // list: ranged by index.
    let l = fixture.key("list");
    for i in 0..10 {
        let _: u64 = redis::cmd("RPUSH")
            .arg(&l)
            .arg(format!("i{i}"))
            .query(&mut fixture.redis)
            .unwrap();
    }
    let page = fixture.key_page(&l, 3, 4);
    assert_eq!(column_names(&page), ["index", "value"]);
    assert_eq!(
        (page.row_count, page.total_count, page.flags.truncated),
        (4, Some(10), true)
    );
    let batch = fixture.batch(&page);
    assert_eq!(
        batch
            .column(0)
            .as_primitive::<Int64Type>()
            .values()
            .to_vec(),
        [3, 4, 5, 6]
    );
    assert_eq!(texts(&batch, 1), ["i3", "i4", "i5", "i6"]);
    assert_eq!(page.columns[0].data_type, "integer");

    // set: paging covers the members exactly once.
    let set = fixture.key("set");
    for i in 0..7 {
        let _: u64 = redis::cmd("SADD")
            .arg(&set)
            .arg(format!("m{i}"))
            .query(&mut fixture.redis)
            .unwrap();
    }
    let mut members = BTreeSet::new();
    for offset in [0, 3, 6] {
        let page = fixture.key_page(&set, offset, 3);
        assert_eq!(column_names(&page), ["member"]);
        assert_eq!(page.total_count, Some(7));
        for member in texts(&fixture.batch(&page), 0) {
            assert!(members.insert(member));
        }
    }
    assert_eq!(members.len(), 7);

    // zset: by rank with Float64 scores.
    let z = fixture.key("zset");
    for i in 0..6 {
        let _: u64 = redis::cmd("ZADD")
            .arg(&z)
            .arg(i as f64 + 0.5)
            .arg(format!("z{i}"))
            .query(&mut fixture.redis)
            .unwrap();
    }
    let page = fixture.key_page(&z, 2, 3);
    assert_eq!(column_names(&page), ["member", "score"]);
    assert_eq!(page.columns[1].data_type, "double");
    assert_eq!((page.row_count, page.total_count), (3, Some(6)));
    let batch = fixture.batch(&page);
    assert_eq!(texts(&batch, 0), ["z2", "z3", "z4"]);
    assert_eq!(
        batch
            .column(1)
            .as_primitive::<Float64Type>()
            .values()
            .to_vec(),
        [2.5, 3.5, 4.5]
    );

    // stream: ids and field text, paged by skipping `offset` entries.
    let x = fixture.key("stream");
    for i in 1..=5 {
        let _: String = redis::cmd("XADD")
            .arg(&x)
            .arg(format!("{i}-1"))
            .arg("a")
            .arg(format!("A{i}"))
            .arg("b")
            .arg(format!("B{i}"))
            .query(&mut fixture.redis)
            .unwrap();
    }
    let page = fixture.key_page(&x, 1, 2);
    assert_eq!(column_names(&page), ["id", "fields"]);
    assert_eq!(
        (page.row_count, page.total_count, page.flags.truncated),
        (2, Some(5), true)
    );
    let batch = fixture.batch(&page);
    assert_eq!(batch.column(0).as_string::<i32>().value(0), "2-1");
    assert_eq!(batch.column(0).as_string::<i32>().value(1), "3-1");
    assert_eq!(
        batch.column(1).as_string::<i32>().value(0),
        r#"{"a":"A2","b":"B2"}"#
    );
    let tail = fixture.key_page(&x, 4, 10);
    assert_eq!((tail.row_count, tail.flags.truncated), (1, false));
    let beyond = fixture.key_page(&x, 50, 10);
    assert_eq!((beyond.row_count, beyond.total_count), (0, Some(5)));

    // Missing key and unknown kind are typed errors.
    let missing = fixture.key("missing");
    let error = fixture
        .query("redis.key", Some(&missing), serde_json::Value::Null, 0, 0)
        .err()
        .unwrap();
    assert_eq!(code(error), "query.key_not_found");
    let error = fixture
        .query("sql", None, serde_json::json!({"sql": "select 1"}), 0, 0)
        .err()
        .unwrap();
    assert_eq!(code(error), "query.unsupported");
}

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn large_values_fetch_only_the_requested_page() {
    let mut fixture = Fixture::new("big");
    let list = fixture.key("list100k");
    let hash = fixture.key("hash100k");
    for chunk in 0..10 {
        let mut list_pipe = redis::pipe();
        let mut hash_pipe = redis::pipe();
        for i in chunk * 10_000..(chunk + 1) * 10_000 {
            list_pipe
                .cmd("RPUSH")
                .arg(&list)
                .arg(format!("element-{i:06}"))
                .ignore();
            hash_pipe
                .cmd("HSET")
                .arg(&hash)
                .arg(format!("field-{i:06}"))
                .arg(format!("value-{i:06}"))
                .ignore();
        }
        let _: () = list_pipe.query(&mut fixture.redis).unwrap();
        let _: () = hash_pipe.query(&mut fixture.redis).unwrap();
    }
    let whole_list_bytes = 100_000 * "element-000000".len() as u64;

    // List: one LRANGE of 50 elements out of 100 000.
    let (out_before, lrange_before) = (
        net_out(&mut fixture.redis),
        calls(&mut fixture.redis, "lrange"),
    );
    let started = Instant::now();
    let page = fixture.key_page(&list, 99_900, 50);
    let elapsed = started.elapsed();
    let (out_mid, lrange_after) = (
        net_out(&mut fixture.redis),
        calls(&mut fixture.redis, "lrange"),
    );
    let batch = fixture.batch(&page);
    assert_eq!(
        (page.row_count, page.total_count, page.flags.truncated),
        (50, Some(100_000), true)
    );
    assert_eq!(batch.column(0).as_primitive::<Int64Type>().value(0), 99_900);
    assert_eq!(texts(&batch, 1)[0], "element-099900");
    let list_bytes = out_mid - out_before;
    println!("list page: {elapsed:?}, server sent {list_bytes} B (whole list ~{whole_list_bytes} B), LRANGE calls {}", lrange_after - lrange_before);
    assert_eq!(lrange_after - lrange_before, 1);
    assert!(
        list_bytes < 32 * 1024,
        "server sent {list_bytes} bytes for a 50-row page"
    );
    assert!(elapsed.as_millis() < 2_000);

    // Hash: first page is one HSCAN of ~50 entries.
    let (out_before, hscan_before) = (
        net_out(&mut fixture.redis),
        calls(&mut fixture.redis, "hscan"),
    );
    let started = Instant::now();
    let first = fixture.key_page(&hash, 0, 50);
    let elapsed = started.elapsed();
    let (out_after, hscan_after) = (
        net_out(&mut fixture.redis),
        calls(&mut fixture.redis, "hscan"),
    );
    assert_eq!(
        (first.row_count, first.total_count, first.flags.truncated),
        (50, Some(100_000), true)
    );
    let hash_bytes = out_after - out_before;
    println!(
        "hash page 0: {elapsed:?}, server sent {hash_bytes} B, HSCAN calls {}",
        hscan_after - hscan_before
    );
    assert_eq!(hscan_after - hscan_before, 1);
    assert!(
        hash_bytes < 32 * 1024,
        "server sent {hash_bytes} bytes for a 50-row page"
    );

    // Deeper page skips entries chunk by chunk, never the whole hash.
    let (out_before, hscan_before) = (
        net_out(&mut fixture.redis),
        calls(&mut fixture.redis, "hscan"),
    );
    let started = Instant::now();
    let deep = fixture.key_page(&hash, 20_000, 50);
    let elapsed = started.elapsed();
    let (out_after, hscan_after) = (
        net_out(&mut fixture.redis),
        calls(&mut fixture.redis, "hscan"),
    );
    assert_eq!(deep.row_count, 50);
    let deep_bytes = out_after - out_before;
    println!(
        "hash page 20000: {elapsed:?}, server sent {deep_bytes} B, HSCAN calls {}",
        hscan_after - hscan_before
    );
    assert!(hscan_after - hscan_before <= 6);
    assert!(deep_bytes < 1024 * 1024, "server sent {deep_bytes} bytes");
    let first_fields: BTreeSet<_> = texts(&fixture.batch(&first), 0).into_iter().collect();
    let deep_fields: BTreeSet<_> = texts(&fixture.batch(&deep), 0).into_iter().collect();
    assert!(first_fields.is_disjoint(&deep_fields));
}

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn binary_keys_and_values_round_trip() {
    let mut fixture = Fixture::new("binary");
    let key = [fixture.prefix.as_bytes(), b"k\xac\xed\x00\x05"].concat();
    let value: &[u8] = b"\xff\xfe\x00z";
    let _: () = redis::cmd("SET")
        .arg(&key[..])
        .arg(value)
        .query(&mut fixture.redis)
        .unwrap();
    use base64::Engine as _;
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);

    // Read: exact bytes in the Arrow page, escaped text in the rendered cell.
    let page = fixture
        .query(
            "redis.key",
            None,
            serde_json::json!({"key_raw": b64(&key)}),
            0,
            0,
        )
        .unwrap();
    let batch = fixture.batch(&page);
    assert_eq!(bytes(&batch, 0, 0), value);
    let cells = fixture
        .session
        .render_cell_batch(page.page_id, 0, 1, 0, 1)
        .unwrap();
    assert_eq!(cells.cell(0).unwrap().text, "\\xff\\xfe\\x00z");
    let row: RowFields = fixture.session.inspect_row(page.page_id, 0).unwrap();
    assert_eq!(
        row.fields[0].value,
        serde_json::json!({"base64": b64(value)})
    );
    assert_eq!(row.mutation_identity["key_raw"], b64(&key));
    assert!(row.mutation_identity.get("key").is_none());

    // Write: binary key and values through the raw arguments.
    let new_value: &[u8] = b"\x00\x01\xfe\xff";
    let identity = serde_json::json!({"db": DB, "key_raw": b64(&key)});
    fixture
        .mutate(
            "set_string",
            identity.clone(),
            serde_json::json!({"value_raw": b64(new_value)}),
        )
        .unwrap();
    assert_eq!(fixture.cmd::<Vec<u8>>(&[b"GET", &key]), new_value);

    let hkey = [fixture.prefix.as_bytes(), b"h\xff"].concat();
    let hid = serde_json::json!({"db": DB, "key_raw": b64(&hkey)});
    fixture
        .mutate(
            "hset",
            hid.clone(),
            serde_json::json!({"field_raw": b64(b"f\xff"), "value_raw": b64(b"v\x00")}),
        )
        .unwrap();
    assert_eq!(
        fixture.cmd::<Vec<u8>>(&[b"HGET", &hkey, b"f\xff"]),
        b"v\x00"
    );
    let hpage = fixture
        .query(
            "redis.key",
            None,
            serde_json::json!({"key_raw": b64(&hkey)}),
            0,
            0,
        )
        .unwrap();
    let hbatch = fixture.batch(&hpage);
    assert_eq!(
        (bytes(&hbatch, 0, 0), bytes(&hbatch, 1, 0)),
        (b"f\xff".to_vec(), b"v\x00".to_vec())
    );

    fixture
        .mutate(
            "lpush",
            hid.clone(),
            serde_json::json!({"value_raw": b64(b"x")}),
        )
        .expect_err("WRONGTYPE");
    let lkey = [fixture.prefix.as_bytes(), b"l\xfe"].concat();
    let lid = serde_json::json!({"db": DB, "key_raw": b64(&lkey)});
    fixture
        .mutate(
            "rpush",
            lid.clone(),
            serde_json::json!({"value_raw": b64(b"\xc0\xc1")}),
        )
        .unwrap();
    assert_eq!(
        fixture.cmd::<Vec<Vec<u8>>>(&[b"LRANGE", &lkey, b"0", b"-1"]),
        vec![b"\xc0\xc1".to_vec()]
    );
    fixture.mutate("zadd", serde_json::json!({"db": DB, "key_raw": b64(&[fixture.prefix.as_bytes(), b"z"].concat())}), serde_json::json!({"score": 2, "member_raw": b64(b"m\xff")})).unwrap();

    // delete_keys with a binary key.
    let removed = fixture
        .mutate(
            "delete_keys",
            serde_json::json!({"db": DB}),
            serde_json::json!({"keys_raw": [b64(&key), b64(&hkey)]}),
        )
        .unwrap();
    assert_eq!(removed.affected_rows, 2);
    assert_eq!(fixture.cmd::<u64>(&[b"EXISTS", &key]), 0);
}

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn redis_command_queries() {
    let mut fixture = Fixture::new("command");
    let k = fixture.key("k");
    let h = fixture.key("h");
    let l = fixture.key("l");
    let _: () = redis::cmd("SET")
        .arg(&k)
        .arg("hello")
        .query(&mut fixture.redis)
        .unwrap();
    for i in 0..5 {
        let _: u64 = redis::cmd("HSET")
            .arg(&h)
            .arg(format!("f{i}"))
            .arg(format!("v{i}"))
            .query(&mut fixture.redis)
            .unwrap();
        let _: u64 = redis::cmd("RPUSH")
            .arg(&l)
            .arg(format!("i{i}"))
            .query(&mut fixture.redis)
            .unwrap();
    }

    // Read commands: scalar, hash pairs, ranged list, status.
    let page = fixture.command(&format!("GET {k}"), false, 0, 0).unwrap();
    assert_eq!(column_names(&page), ["value"]);
    assert_eq!(texts(&fixture.batch(&page), 0), ["hello"]);
    assert!(page.flags.read_only);
    let page = fixture
        .command(&format!("HGETALL {h}"), false, 0, 0)
        .unwrap();
    assert_eq!(
        (column_names(&page), page.total_count),
        (vec!["field", "value"], Some(5))
    );
    let page = fixture
        .command(&format!("LRANGE {l} 0 -1"), false, 1, 2)
        .unwrap();
    assert_eq!(
        (page.row_count, page.total_count, page.flags.truncated),
        (2, Some(5), true)
    );
    let batch = fixture.batch(&page);
    assert_eq!(
        batch
            .column(0)
            .as_primitive::<Int64Type>()
            .values()
            .to_vec(),
        [1, 2]
    );
    assert_eq!(texts(&batch, 1), ["i1", "i2"]);
    let page = fixture.command("INFO server", false, 0, 0).unwrap();
    assert!(texts(&fixture.batch(&page), 0)[0].contains("redis_version"));
    let missing = fixture.key("nope");
    let page = fixture
        .command(&format!("GET {missing}"), false, 0, 0)
        .unwrap();
    assert!(fixture.batch(&page).column(0).is_null(0));
    let page = fixture.command(&format!("HLEN {h}"), false, 0, 0).unwrap();
    assert_eq!(
        fixture
            .batch(&page)
            .column(0)
            .as_primitive::<Int64Type>()
            .value(0),
        5
    );

    // (The keys above live in db 9 only: the commands ran in the request's database.)

    // Safety classification.
    assert_eq!(
        code(fixture.command("KEYS *", true, 0, 0).err().unwrap()),
        "query.blocked"
    );
    assert_eq!(
        code(fixture.command("FLUSHALL", true, 0, 0).err().unwrap()),
        "query.blocked"
    );
    let error = fixture
        .command(&format!("SET {k} changed"), false, 0, 0)
        .err()
        .unwrap();
    assert_eq!(code(error), "query.confirmation_required");
    assert_eq!(
        fixture.cmd::<Vec<u8>>(&[b"GET", k.as_bytes()]),
        b"hello",
        "unconfirmed command must not run"
    );
    let done = fixture
        .command(&format!("SET {k} changed"), true, 0, 0)
        .unwrap();
    assert!(done.columns.is_empty());
    assert_eq!((done.row_count, done.total_count), (0, Some(0)));
    assert_eq!(fixture.cmd::<Vec<u8>>(&[b"GET", k.as_bytes()]), b"changed");
    let counter = fixture.key("counter");
    let page = fixture
        .command(&format!("INCR {counter}"), true, 0, 0)
        .unwrap();
    assert_eq!(
        fixture
            .batch(&page)
            .column(0)
            .as_primitive::<Int64Type>()
            .value(0),
        1
    );
    assert_eq!(
        code(
            fixture
                .command(&format!("INCR {counter}"), true, 3, 0)
                .err()
                .unwrap()
        ),
        "query.unsupported"
    );
    assert_eq!(
        code(
            fixture
                .command("BLPOP nothing 0", true, 0, 0)
                .err()
                .unwrap()
        ),
        "query.unsupported"
    );
    assert_eq!(
        code(fixture.command("MULTI", true, 0, 0).err().unwrap()),
        "query.unsupported"
    );
    assert_eq!(
        code(fixture.command("", false, 0, 0).err().unwrap()),
        "query.bad_request"
    );
    // A server error is a plain failure.
    let error = fixture
        .command(&format!("HGET {k} f"), false, 0, 0)
        .err()
        .unwrap();
    assert_eq!(code(error), "query.failed");
}

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn every_mutation_kind() {
    let mut fixture = Fixture::new("mutate");
    let id = |fixture: &Fixture, key: &str| fixture.id(key);

    let s = fixture.key("s");
    let r = fixture
        .mutate(
            "set_string",
            id(&fixture, "s"),
            serde_json::json!({"value": "v1"}),
        )
        .unwrap();
    assert_eq!(r.affected_rows, 1);
    assert_eq!(fixture.cmd::<Vec<u8>>(&[b"GET", s.as_bytes()]), b"v1");
    fixture
        .mutate(
            "set_string",
            id(&fixture, "s"),
            serde_json::json!({"value": ""}),
        )
        .unwrap();
    assert_eq!(fixture.cmd::<Vec<u8>>(&[b"GET", s.as_bytes()]), b"");

    assert_eq!(
        fixture
            .mutate(
                "set_ttl",
                id(&fixture, "s"),
                serde_json::json!({"ttl_seconds": 300})
            )
            .unwrap()
            .affected_rows,
        1
    );
    assert!((1..=300).contains(&fixture.cmd::<i64>(&[b"TTL", s.as_bytes()])));
    assert_eq!(
        fixture
            .mutate(
                "set_ttl",
                id(&fixture, "s"),
                serde_json::json!({"ttl_seconds": -1})
            )
            .unwrap()
            .affected_rows,
        1
    );
    assert_eq!(fixture.cmd::<i64>(&[b"TTL", s.as_bytes()]), -1);
    assert_eq!(
        fixture
            .mutate(
                "set_ttl",
                id(&fixture, "nokey"),
                serde_json::json!({"ttl_seconds": 5})
            )
            .unwrap()
            .affected_rows,
        0
    );

    // set_string keeps the TTL unless asked; set_ttl 0 is refused; null persists.
    fixture
        .mutate(
            "set_ttl",
            id(&fixture, "s"),
            serde_json::json!({"ttl_seconds": 300}),
        )
        .unwrap();
    fixture
        .mutate(
            "set_string",
            id(&fixture, "s"),
            serde_json::json!({"value": "kept"}),
        )
        .unwrap();
    let kept = fixture.cmd::<i64>(&[b"TTL", s.as_bytes()]);
    assert!((1..=300).contains(&kept), "KEEPTTL lost the TTL: {kept}");
    assert_eq!(fixture.cmd::<Vec<u8>>(&[b"GET", s.as_bytes()]), b"kept");
    fixture
        .mutate(
            "set_string",
            id(&fixture, "s"),
            serde_json::json!({"value": "new ttl", "ttl_seconds": 100}),
        )
        .unwrap();
    assert!((1..=100).contains(&fixture.cmd::<i64>(&[b"TTL", s.as_bytes()])));
    fixture
        .mutate(
            "set_string",
            id(&fixture, "s"),
            serde_json::json!({"value": "persistent", "ttl_seconds": -1}),
        )
        .unwrap();
    assert_eq!(fixture.cmd::<i64>(&[b"TTL", s.as_bytes()]), -1);
    fixture
        .mutate(
            "set_ttl",
            id(&fixture, "s"),
            serde_json::json!({"ttl_seconds": 300}),
        )
        .unwrap();
    let error = fixture
        .mutate(
            "set_ttl",
            id(&fixture, "s"),
            serde_json::json!({"ttl_seconds": 0}),
        )
        .unwrap_err();
    assert_eq!(code(error), "mutation.bad_request");
    assert_eq!(
        fixture.cmd::<Vec<u8>>(&[b"GET", s.as_bytes()]),
        b"persistent",
        "a refused TTL 0 must not delete the key"
    );
    assert!((1..=300).contains(&fixture.cmd::<i64>(&[b"TTL", s.as_bytes()])));
    let error = fixture
        .mutate(
            "set_string",
            id(&fixture, "s"),
            serde_json::json!({"value": "x", "ttl_seconds": 0}),
        )
        .unwrap_err();
    assert_eq!(code(error), "mutation.bad_request");
    assert_eq!(
        fixture
            .mutate(
                "set_ttl",
                id(&fixture, "s"),
                serde_json::json!({"ttl_seconds": null})
            )
            .unwrap()
            .affected_rows,
        1
    );
    assert_eq!(fixture.cmd::<i64>(&[b"TTL", s.as_bytes()]), -1);

    let h = fixture.key("h");
    fixture
        .mutate(
            "hset",
            id(&fixture, "h"),
            serde_json::json!({"field": "a", "value": "1"}),
        )
        .unwrap();
    fixture
        .mutate(
            "hset",
            id(&fixture, "h"),
            serde_json::json!({"field": "b", "value": "2"}),
        )
        .unwrap();
    assert_eq!(fixture.cmd::<Vec<u8>>(&[b"HGET", h.as_bytes(), b"a"]), b"1");
    assert_eq!(
        fixture
            .mutate("hdel", id(&fixture, "h"), serde_json::json!({"field": "a"}))
            .unwrap()
            .affected_rows,
        1
    );
    assert_eq!(
        fixture
            .mutate("hdel", id(&fixture, "h"), serde_json::json!({"field": "a"}))
            .unwrap()
            .affected_rows,
        0
    );
    assert_eq!(fixture.cmd::<u64>(&[b"HLEN", h.as_bytes()]), 1);

    let l = fixture.key("l");
    fixture
        .mutate(
            "rpush",
            id(&fixture, "l"),
            serde_json::json!({"value": "mid"}),
        )
        .unwrap();
    fixture
        .mutate(
            "lpush",
            id(&fixture, "l"),
            serde_json::json!({"value": "head"}),
        )
        .unwrap();
    fixture
        .mutate(
            "rpush",
            id(&fixture, "l"),
            serde_json::json!({"value": "tail"}),
        )
        .unwrap();
    fixture
        .mutate(
            "lset",
            id(&fixture, "l"),
            serde_json::json!({"index": 1, "value": "MID"}),
        )
        .unwrap();
    assert_eq!(
        fixture.cmd::<Vec<String>>(&[b"LRANGE", l.as_bytes(), b"0", b"-1"]),
        ["head", "MID", "tail"]
    );
    let error = fixture
        .mutate(
            "lset",
            id(&fixture, "l"),
            serde_json::json!({"index": 99, "value": "x"}),
        )
        .err()
        .unwrap();
    assert_eq!(code(error), "mutation.failed");

    let set = fixture.key("set");
    assert_eq!(
        fixture
            .mutate(
                "sadd",
                id(&fixture, "set"),
                serde_json::json!({"member": "m"})
            )
            .unwrap()
            .affected_rows,
        1
    );
    assert_eq!(
        fixture
            .mutate(
                "sadd",
                id(&fixture, "set"),
                serde_json::json!({"member": "m"})
            )
            .unwrap()
            .affected_rows,
        0
    );
    assert_eq!(
        fixture
            .mutate(
                "srem",
                id(&fixture, "set"),
                serde_json::json!({"member": "m"})
            )
            .unwrap()
            .affected_rows,
        1
    );
    assert_eq!(fixture.cmd::<u64>(&[b"EXISTS", set.as_bytes()]), 0);

    let z = fixture.key("z");
    fixture
        .mutate(
            "zadd",
            id(&fixture, "z"),
            serde_json::json!({"score": 1.5, "member": "m"}),
        )
        .unwrap();
    fixture
        .mutate(
            "zadd",
            id(&fixture, "z"),
            serde_json::json!({"score": "2.5", "member": "m"}),
        )
        .unwrap();
    assert_eq!(fixture.cmd::<f64>(&[b"ZSCORE", z.as_bytes(), b"m"]), 2.5);
    assert_eq!(
        fixture
            .mutate(
                "zrem",
                id(&fixture, "z"),
                serde_json::json!({"member": "m"})
            )
            .unwrap()
            .affected_rows,
        1
    );

    let x = fixture.key("x");
    let added = fixture
        .mutate("xadd", id(&fixture, "x"), serde_json::json!({"entry_id": "7-1", "fields": [{"field": "a", "value": "1"}, {"field": "b", "value": "2"}]}))
        .unwrap();
    assert_eq!(added.affected_rows, 1);
    fixture
        .mutate(
            "xadd",
            id(&fixture, "x"),
            serde_json::json!({"fields": [{"field": "c", "value": "3"}]}),
        )
        .unwrap();
    assert_eq!(fixture.cmd::<u64>(&[b"XLEN", x.as_bytes()]), 2);
    assert_eq!(
        fixture
            .mutate(
                "xdel",
                id(&fixture, "x"),
                serde_json::json!({"entry_id": "7-1"})
            )
            .unwrap()
            .affected_rows,
        1
    );
    assert_eq!(fixture.cmd::<u64>(&[b"XLEN", x.as_bytes()]), 1);

    let (a, b, c) = (fixture.key("da"), fixture.key("db"), fixture.key("dc"));
    for key in [&a, &b, &c] {
        let _: () = redis::cmd("SET")
            .arg(key)
            .arg("v")
            .query(&mut fixture.redis)
            .unwrap();
    }
    assert_eq!(
        fixture
            .mutate("delete_key", id(&fixture, "da"), serde_json::Value::Null)
            .unwrap()
            .affected_rows,
        1
    );
    assert_eq!(fixture.cmd::<u64>(&[b"EXISTS", a.as_bytes()]), 0);
    let gone = fixture
        .mutate(
            "delete_keys",
            serde_json::json!({"db": DB}),
            serde_json::json!({"keys": [b, c, fixture.key("never")]}),
        )
        .unwrap();
    assert_eq!(gone.affected_rows, 2);
    assert_eq!(
        fixture
            .mutate(
                "delete_keys",
                serde_json::json!({"db": DB}),
                serde_json::json!({"keys": []})
            )
            .unwrap()
            .affected_rows,
        0
    );

    // Typed failures.
    let error = fixture
        .mutate(
            "lpush",
            id(&fixture, "s"),
            serde_json::json!({"value": "x"}),
        )
        .err()
        .unwrap();
    assert_eq!(code(error), "mutation.failed", "WRONGTYPE from the server");
    let error = fixture
        .mutate(
            "hset",
            serde_json::json!({"key": fixture.key("h")}),
            serde_json::json!({"field": "a", "value": "1"}),
        )
        .err()
        .unwrap();
    assert_eq!(code(error), "mutation.bad_request", "the db is mandatory");
    let error = fixture
        .mutate("insert", id(&fixture, "s"), serde_json::Value::Null)
        .err()
        .unwrap();
    assert!(
        matches!(error, LoaderError::UnsupportedFeature { .. }),
        "host gates unknown kinds: {error}"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn inspect_row_identity_drives_mutations_and_pages_are_invalidated() {
    let mut fixture = Fixture::new("inspect");
    let h = fixture.key("h");
    for i in 0..3 {
        let _: u64 = redis::cmd("HSET")
            .arg(&h)
            .arg(format!("f{i}"))
            .arg(format!("v{i}"))
            .query(&mut fixture.redis)
            .unwrap();
    }
    let page = fixture.key_page(&h, 0, 0);
    let batch = fixture.batch(&page);
    let row = (0..batch.num_rows())
        .find(|row| texts(&batch, 0)[*row] == "f1")
        .expect("f1 row");
    let fields: RowFields = fixture
        .session
        .inspect_row(page.page_id, row as u64)
        .unwrap();
    println!(
        "inspect_row(hash): {}",
        serde_json::to_string(&fields).unwrap()
    );
    assert_eq!(fields.page_id, page.page_id);
    assert_eq!(fields.row_index, row as u64);
    assert_eq!(
        (
            fields.fields[0].name.as_str(),
            fields.fields[0].display_text.as_str()
        ),
        ("field", "f1")
    );
    assert_eq!(
        (
            fields.fields[1].name.as_str(),
            fields.fields[1].display_text.as_str()
        ),
        ("value", "v1")
    );
    assert!(!fields.fields[0].editable && fields.fields[1].editable);
    assert_eq!(fields.fields[1].value, serde_json::json!("v1"));
    assert_eq!(fields.mutation_identity["db"], 9);
    assert_eq!(fields.mutation_identity["key"], h);
    assert_eq!(fields.mutation_identity["type"], "hash");

    // The identity plus the new value is a complete hset request; the write
    // invalidates the open page.
    let result = fixture
        .mutate(
            "hset",
            fields.mutation_identity.clone(),
            serde_json::json!({"value": "edited"}),
        )
        .unwrap();
    assert_eq!(result.affected_rows, 1);
    assert_eq!(result.invalidated_page_ids, [page.page_id]);
    assert_eq!(
        fixture.cmd::<Vec<u8>>(&[b"HGET", h.as_bytes(), b"f1"]),
        b"edited"
    );
    assert!(
        fixture.session.inspect_row(page.page_id, 0).is_err(),
        "invalidated page is gone"
    );

    // List row -> lset by index.
    let l = fixture.key("l");
    for value in ["a", "b", "c"] {
        let _: u64 = redis::cmd("RPUSH")
            .arg(&l)
            .arg(value)
            .query(&mut fixture.redis)
            .unwrap();
    }
    let page = fixture.key_page(&l, 0, 0);
    let row = fixture.session.inspect_row(page.page_id, 2).unwrap();
    assert_eq!(row.mutation_identity["index"], 2);
    fixture
        .mutate(
            "lset",
            row.mutation_identity,
            serde_json::json!({"value": "C"}),
        )
        .unwrap();
    assert_eq!(fixture.cmd::<String>(&[b"LINDEX", l.as_bytes(), b"2"]), "C");

    // Zset row -> zadd of the member's new score.
    let z = fixture.key("z");
    let _: u64 = redis::cmd("ZADD")
        .arg(&z)
        .arg(1)
        .arg("m")
        .query(&mut fixture.redis)
        .unwrap();
    let page = fixture.key_page(&z, 0, 0);
    let row = fixture.session.inspect_row(page.page_id, 0).unwrap();
    assert!(row.fields[1].editable && row.fields[1].value == serde_json::json!(1.0));
    fixture
        .mutate(
            "zadd",
            row.mutation_identity,
            serde_json::json!({"score": 9}),
        )
        .unwrap();
    assert_eq!(fixture.cmd::<f64>(&[b"ZSCORE", z.as_bytes(), b"m"]), 9.0);

    // Stream row -> xdel; string row -> set_string.
    let x = fixture.key("x");
    let _: String = redis::cmd("XADD")
        .arg(&x)
        .arg("5-1")
        .arg("a")
        .arg("1")
        .query(&mut fixture.redis)
        .unwrap();
    let page = fixture.key_page(&x, 0, 0);
    let row = fixture.session.inspect_row(page.page_id, 0).unwrap();
    assert_eq!(row.mutation_identity["entry_id"], "5-1");
    fixture
        .mutate("xdel", row.mutation_identity, serde_json::Value::Null)
        .unwrap();
    assert_eq!(fixture.cmd::<u64>(&[b"XLEN", x.as_bytes()]), 0);
    let s = fixture.key("s");
    let _: () = redis::cmd("SET")
        .arg(&s)
        .arg("old")
        .query(&mut fixture.redis)
        .unwrap();
    let page = fixture.key_page(&s, 0, 0);
    let row = fixture.session.inspect_row(page.page_id, 0).unwrap();
    assert!(row.fields[0].editable);
    fixture
        .mutate(
            "set_string",
            row.mutation_identity,
            serde_json::json!({"value": "new"}),
        )
        .unwrap();
    assert_eq!(fixture.cmd::<Vec<u8>>(&[b"GET", s.as_bytes()]), b"new");

    // Command pages: read-only rows without a mutation identity.
    let page = fixture.command(&format!("GET {s}"), false, 0, 0).unwrap();
    let row = fixture.session.inspect_row(page.page_id, 0).unwrap();
    assert!(row.fields.iter().all(|field| !field.editable));
    assert_eq!(row.mutation_identity, serde_json::json!({}));
}

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn command_replies_are_capped_without_the_server_sending_the_value() {
    let mut fixture = Fixture::new("cap");
    let list = fixture.key("big-list");
    let huge = fixture.key("big-string");
    let small = fixture.key("small-list");
    for chunk in 0..13 {
        let mut pipe = redis::pipe();
        for i in 0..10_000 {
            pipe.cmd("RPUSH")
                .arg(&list)
                .arg(format!("e{}", chunk * 10_000 + i))
                .ignore();
        }
        let _: () = pipe.query(&mut fixture.redis).unwrap();
    }
    let _: u64 = redis::cmd("RPUSH")
        .arg(&small)
        .arg("a")
        .arg("b")
        .query(&mut fixture.redis)
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg(&huge)
        .arg(vec![b'x'; (16 << 20) + 1])
        .query(&mut fixture.redis)
        .unwrap();

    // 130 000 elements: refused from LLEN alone, before LRANGE ever runs.
    let (out_before, lrange_before) = (
        net_out(&mut fixture.redis),
        calls(&mut fixture.redis, "lrange"),
    );
    let error = fixture
        .command(&format!("LRANGE {list} 0 -1"), false, 0, 0)
        .unwrap_err();
    assert_eq!(code(error), "query.reply_too_large");
    let error = fixture
        .command(&format!("GET {huge}"), false, 0, 0)
        .unwrap_err();
    assert_eq!(code(error), "query.reply_too_large");
    let sent = net_out(&mut fixture.redis) - out_before;
    println!("refused two oversized replies, server sent {sent} B");
    assert_eq!(calls(&mut fixture.redis, "lrange"), lrange_before);
    assert!(
        sent < 16 * 1024,
        "server sent {sent} bytes for refused commands"
    );

    // A bounded range of the same list, and small values, still work.
    let page = fixture
        .command(&format!("LRANGE {list} 120000 120009"), false, 0, 0)
        .unwrap();
    assert_eq!((page.row_count, page.total_count), (10, Some(10)));
    assert_eq!(texts(&fixture.batch(&page), 1)[0], "e120000");
    let page = fixture
        .command(&format!("LRANGE {small} 0 -1"), false, 0, 0)
        .unwrap();
    assert_eq!(page.row_count, 2);
    let page = fixture
        .command(&format!("HLEN {small}x"), false, 0, 0)
        .unwrap();
    assert_eq!(page.row_count, 1);
    // The same value is still reachable through redis.key paging.
    let page = fixture.key_page(&list, 129_990, 10);
    assert_eq!((page.row_count, page.total_count), (10, Some(130_000)));
}

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn byte_budget_covers_collections_mget_and_nested_replies() {
    let mut fixture = Fixture::new("bytecap");
    let (hash, stream, small) = (
        fixture.key("hash"),
        fixture.key("stream"),
        fixture.key("small"),
    );
    let big = vec![b'v'; 6 << 20];
    let mut mget_keys = Vec::new();
    for i in 0..3 {
        let key = fixture.key(&format!("big{i}"));
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg(&big)
            .query(&mut fixture.redis)
            .unwrap();
        let _: u64 = redis::cmd("HSET")
            .arg(&hash)
            .arg(format!("f{i}"))
            .arg(&big)
            .query(&mut fixture.redis)
            .unwrap();
        let _: String = redis::cmd("XADD")
            .arg(&stream)
            .arg(format!("{}-1", i + 1))
            .arg("f")
            .arg(&big)
            .query(&mut fixture.redis)
            .unwrap();
        mget_keys.push(key);
    }
    let _: () = redis::cmd("SET")
        .arg(&small)
        .arg("tiny")
        .query(&mut fixture.redis)
        .unwrap();

    let out_before = net_out(&mut fixture.redis);
    // 3 x 6 MiB: few elements, over the byte budget.
    for command in [
        format!("HGETALL {hash}"),
        format!("HVALS {hash}"),
        format!("MGET {} {} {}", mget_keys[0], mget_keys[1], mget_keys[2]),
        format!("XRANGE {stream} - +"),
        format!("XREVRANGE {stream} + -"),
    ] {
        let error = fixture.command(&command, false, 0, 0).unwrap_err();
        assert_eq!(code(error), "query.reply_too_large", "{command}");
    }
    let sent = net_out(&mut fixture.redis) - out_before;
    println!("five oversized collection replies refused, server sent {sent} B");
    assert!(
        sent < 64 * 1024,
        "the oversized replies were sent anyway: {sent} B"
    );

    // Within budget: one field, one entry, small keys.
    let page = fixture
        .command(&format!("XRANGE {stream} - + COUNT 1"), false, 0, 0)
        .unwrap();
    assert_eq!(page.row_count, 1);
    let page = fixture
        .command(&format!("HGET {hash} f0"), false, 0, 0)
        .unwrap();
    assert_eq!(page.row_count, 1);
    let page = fixture
        .command(&format!("MGET {small} {small}"), false, 0, 0)
        .unwrap();
    assert_eq!(page.row_count, 2);
    let page = fixture
        .command(&format!("GETRANGE {} 0 9", mget_keys[0]), false, 0, 0)
        .unwrap();
    assert_eq!(texts(&fixture.batch(&page), 0), ["vvvvvvvvvv"]);
}

#[test]
#[ignore = "needs TABLEX_TEST_REDIS_URI"]
fn key_info_rename_key_and_lrem() {
    let mut fixture = Fixture::new("keyinfo");
    let caps: DriverCapabilities = {
        let handle = DriverLoader.load(driver_path()).expect("load");
        handle.capabilities().clone()
    };
    assert!(caps.has(Capability::MutationRenameKey));
    assert!(caps.has(Capability::MutationLrem));

    // key_info: one row per key, never the value.
    let hash = fixture.key("h");
    let list = fixture.key("l");
    let text = fixture.key("s");
    fixture.cmd::<u64>(&[b"HSET", hash.as_bytes(), b"a", b"1", b"b", b"2"]);
    fixture.cmd::<u64>(&[b"RPUSH", list.as_bytes(), b"x", b"y", b"x"]);
    fixture.cmd::<()>(&[b"SET", text.as_bytes(), b"hello", b"EX", b"300"]);
    let info = |fixture: &mut Fixture, key: &str| {
        let full = fixture.key(key);
        let page = fixture
            .query("redis.key_info", Some(&full), serde_json::Value::Null, 0, 0)
            .expect("key_info page");
        assert_eq!(column_names(&page), ["type", "ttl", "length", "encoding", "memory"]);
        assert_eq!(page.row_count, 1);
        let batch = fixture.batch(&page);
        let ttl = batch.column(1).as_primitive::<Int64Type>();
        let length = batch.column(2).as_primitive::<Int64Type>().value(0);
        (
            batch.column(0).as_string::<i32>().value(0).to_owned(),
            (!ttl.is_null(0)).then(|| ttl.value(0)),
            length,
            (!batch.column(3).is_null(0)).then(|| batch.column(3).as_string::<i32>().value(0).to_owned()),
        )
    };
    let (kind, ttl, length, encoding) = info(&mut fixture, "h");
    assert_eq!((kind.as_str(), ttl, length), ("hash", None, 2));
    assert!(encoding.is_some());
    assert_eq!(info(&mut fixture, "l").0, "list");
    let (kind, ttl, length, _) = info(&mut fixture, "s");
    assert_eq!((kind.as_str(), length), ("string", 5));
    assert!(ttl.is_some_and(|ttl| (1..=300).contains(&ttl)));
    let missing = fixture
        .query("redis.key_info", Some(&fixture.key("none")), serde_json::Value::Null, 0, 0)
        .map(|_| ())
        .unwrap_err();
    assert_eq!(code(missing), "query.key_not_found");

    // lrem: first match by default, all with count 0.
    let removed = fixture
        .mutate("lrem", fixture.id("l"), serde_json::json!({"value": "x"}))
        .unwrap();
    assert_eq!(removed.affected_rows, 1);
    assert_eq!(
        fixture.cmd::<Vec<Vec<u8>>>(&[b"LRANGE", list.as_bytes(), b"0", b"-1"]),
        vec![b"y".to_vec(), b"x".to_vec()]
    );
    fixture.cmd::<u64>(&[b"RPUSH", list.as_bytes(), b"x"]);
    let removed = fixture
        .mutate("lrem", fixture.id("l"), serde_json::json!({"value": "x", "count": 0}))
        .unwrap();
    assert_eq!(removed.affected_rows, 2);

    // rename_key: refuses to clobber unless asked.
    let renamed = fixture.key("h2");
    let moved = fixture
        .mutate(
            "rename_key",
            fixture.id("h"),
            serde_json::json!({"new_key": renamed}),
        )
        .unwrap();
    assert_eq!(moved.affected_rows, 1);
    assert_eq!(fixture.cmd::<i64>(&[b"EXISTS", hash.as_bytes()]), 0);
    assert_eq!(fixture.cmd::<i64>(&[b"HLEN", renamed.as_bytes()]), 2);
    let clash = fixture
        .mutate(
            "rename_key",
            fixture.id("s"),
            serde_json::json!({"new_key": renamed}),
        )
        .unwrap();
    assert_eq!(clash.affected_rows, 0, "an existing target is left alone");
    assert_eq!(fixture.cmd::<i64>(&[b"EXISTS", text.as_bytes()]), 1);
    let forced = fixture
        .mutate(
            "rename_key",
            fixture.id("s"),
            serde_json::json!({"new_key": renamed, "overwrite": true}),
        )
        .unwrap();
    assert_eq!(forced.affected_rows, 1);
    assert_eq!(fixture.cmd::<Vec<u8>>(&[b"GET", renamed.as_bytes()]), b"hello");
    let absent = fixture
        .mutate(
            "rename_key",
            fixture.id("nokey"),
            serde_json::json!({"new_key": fixture.key("zzz")}),
        )
        .unwrap_err();
    assert!(matches!(absent, LoaderError::DriverRejected { .. }));
}
