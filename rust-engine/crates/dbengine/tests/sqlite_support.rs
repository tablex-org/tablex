use std::ffi::{CStr, CString};
use std::path::PathBuf;

fn temp_db_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tablex-{}-{}.db", name, std::process::id()))
}

unsafe fn take_json(ptr: *mut std::ffi::c_char) -> serde_json::Value {
    assert!(!ptr.is_null());
    let raw = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    dbengine::dbengine_free_string(ptr);
    serde_json::from_str(&raw).expect("valid json")
}

#[test]
fn sqlite_connect_catalog_and_query_work() {
    let path = temp_db_path("catalog-query");
    let _ = std::fs::remove_file(&path);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO users (name) VALUES ('ada')", [])
            .unwrap();
    }

    let uri = CString::new(format!("sqlite://{}", path.display())).unwrap();
    let name = CString::new("sqlite-test").unwrap();
    let connected = unsafe { take_json(dbengine::dbengine_connect(uri.as_ptr(), name.as_ptr())) };
    let conn_id = connected["id"].as_u64().expect("connection id");

    let catalog = unsafe { take_json(dbengine::dbengine_sqlite_catalog(conn_id)) };
    let schemas = catalog["schemas"].as_array().expect("schemas array");
    assert!(schemas.iter().any(|schema| schema["name"] == "main"));
    assert!(schemas.iter().any(|schema| {
        schema["tables"].as_array().is_some_and(|tables| {
            tables
                .iter()
                .any(|table| table["name"] == "users" && table["kind"] == "table")
        })
    }));

    let sql = CString::new("SELECT id, name FROM users ORDER BY id").unwrap();
    let result = unsafe {
        take_json(dbengine::dbengine_execute_sqlite_sql_arrow(
            conn_id,
            sql.as_ptr(),
            100,
        ))
    };
    assert_eq!(result["row_count"], 1);
    assert!(result["schema_ptr"].as_u64().unwrap() > 0);
    assert!(result["array_ptr"].as_u64().unwrap() > 0);

    assert_eq!(dbengine::dbengine_disconnect(conn_id), 0);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sqlite_memory_database_persists_across_calls() {
    let uri = CString::new("sqlite://:memory:").unwrap();
    let name = CString::new("sqlite-memory-test").unwrap();
    let connected = unsafe { take_json(dbengine::dbengine_connect(uri.as_ptr(), name.as_ptr())) };
    let conn_id = connected["id"].as_u64().expect("connection id");

    let setup = CString::new("CREATE TABLE memory_probe (id INTEGER PRIMARY KEY, name TEXT NOT NULL); INSERT INTO memory_probe (name) VALUES ('ada');").unwrap();
    let setup_result = unsafe {
        take_json(dbengine::dbengine_execute_sqlite_sql_arrow(
            conn_id,
            setup.as_ptr(),
            100,
        ))
    };
    assert_eq!(setup_result["row_count"], 0, "setup result: {setup_result}");

    let sql = CString::new("SELECT name FROM memory_probe WHERE id = 1").unwrap();
    let result = unsafe {
        take_json(dbengine::dbengine_execute_sqlite_sql_arrow(
            conn_id,
            sql.as_ptr(),
            100,
        ))
    };
    assert_eq!(result["row_count"], 1, "select result: {result}");
    assert!(result["schema_ptr"].as_u64().unwrap() > 0);
    assert!(result["array_ptr"].as_u64().unwrap() > 0);

    assert_eq!(dbengine::dbengine_disconnect(conn_id), 0);
}

#[test]
fn sqlite_catalog_includes_columns_and_views() {
    let path = temp_db_path("catalog-columns");
    let _ = std::fs::remove_file(&path);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT)",
            [],
        )
        .unwrap();
        conn.execute("CREATE VIEW user_names AS SELECT name FROM users", [])
            .unwrap();
    }

    let uri = CString::new(format!("sqlite://{}", path.display())).unwrap();
    let name = CString::new("sqlite-catalog-columns").unwrap();
    let connected = unsafe { take_json(dbengine::dbengine_connect(uri.as_ptr(), name.as_ptr())) };
    let conn_id = connected["id"].as_u64().expect("connection id");

    let catalog = unsafe { take_json(dbengine::dbengine_sqlite_catalog(conn_id)) };
    let main = catalog["schemas"]
        .as_array()
        .expect("schemas")
        .iter()
        .find(|schema| schema["name"] == "main")
        .expect("main schema");
    let tables = main["tables"].as_array().expect("tables");
    let users = tables
        .iter()
        .find(|table| table["name"] == "users")
        .expect("users table");
    assert_eq!(users["kind"], "table");
    let columns = users["columns"].as_array().expect("columns");
    assert!(columns.iter().any(|column| {
        column["name"] == "id" && column["primary_key"] == true && column["data_type"] == "INTEGER"
    }));
    assert!(columns.iter().any(|column| {
        column["name"] == "name" && column["nullable"] == false && column["data_type"] == "TEXT"
    }));
    assert!(tables
        .iter()
        .any(|table| table["name"] == "user_names" && table["kind"] == "view"));

    assert_eq!(dbengine::dbengine_disconnect(conn_id), 0);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sqlite_function_aliases_work_outside_literals_and_comments() {
    let uri = CString::new("sqlite://:memory:").unwrap();
    let name = CString::new("sqlite-alias-test").unwrap();
    let connected = unsafe { take_json(dbengine::dbengine_connect(uri.as_ptr(), name.as_ptr())) };
    let conn_id = connected["id"].as_u64().expect("connection id");

    let setup =
        CString::new("CREATE TABLE t (name TEXT); INSERT INTO t VALUES ('hello');").unwrap();
    let setup_result = unsafe {
        take_json(dbengine::dbengine_execute_sqlite_sql_arrow(
            conn_id,
            setup.as_ptr(),
            100,
        ))
    };
    assert_eq!(setup_result["row_count"], 0, "setup result: {setup_result}");

    let sql = CString::new("-- if(1,2,3) comment\nSELECT substring(if(1 = 1, name, 'no'), 1, 2) AS part, 'if(0,1,2)' AS literal FROM t").unwrap();
    let result = unsafe {
        take_json(dbengine::dbengine_execute_sqlite_sql_arrow(
            conn_id,
            sql.as_ptr(),
            100,
        ))
    };
    assert_eq!(result["row_count"], 1, "alias result: {result}");
    assert!(result["schema_ptr"].as_u64().unwrap() > 0);
    assert!(result["array_ptr"].as_u64().unwrap() > 0);

    assert_eq!(dbengine::dbengine_disconnect(conn_id), 0);
}

#[test]
fn sqlite_catalog_includes_index_foreign_key_and_trigger_metadata() {
    let path = temp_db_path("catalog-metadata");
    let _ = std::fs::remove_file(&path);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE teams (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, team_id INTEGER NOT NULL, email TEXT UNIQUE, FOREIGN KEY(team_id) REFERENCES teams(id))", [])
            .unwrap();
        conn.execute("CREATE INDEX idx_users_team_id ON users(team_id)", [])
            .unwrap();
        conn.execute("CREATE TRIGGER users_ai AFTER INSERT ON users BEGIN UPDATE teams SET name = name WHERE id = NEW.team_id; END", [])
            .unwrap();
    }

    let uri = CString::new(format!("sqlite://{}", path.display())).unwrap();
    let name = CString::new("sqlite-catalog-metadata").unwrap();
    let connected = unsafe { take_json(dbengine::dbengine_connect(uri.as_ptr(), name.as_ptr())) };
    let conn_id = connected["id"].as_u64().expect("connection id");

    let catalog = unsafe { take_json(dbengine::dbengine_sqlite_catalog(conn_id)) };
    let tables = catalog["schemas"][0]["tables"].as_array().expect("tables");
    let users = tables
        .iter()
        .find(|table| table["name"] == "users")
        .expect("users table");

    let indexes = users["indexes"].as_array().expect("indexes");
    assert!(indexes.iter().any(|index| {
        index["name"] == "idx_users_team_id"
            && index["columns"]
                .as_array()
                .is_some_and(|columns| columns.iter().any(|column| column == "team_id"))
    }));

    let foreign_keys = users["foreign_keys"].as_array().expect("foreign keys");
    assert!(foreign_keys.iter().any(|fk| {
        fk["column"] == "team_id" && fk["ref_table"] == "teams" && fk["ref_column"] == "id"
    }));

    let triggers = users["triggers"].as_array().expect("triggers");
    assert!(triggers.iter().any(|trigger| {
        trigger["name"] == "users_ai"
            && trigger["event"] == "INSERT"
            && trigger["timing"] == "AFTER"
    }));

    assert_eq!(dbengine::dbengine_disconnect(conn_id), 0);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sqlite_catalog_lists_attached_database_schema() {
    let main_path = temp_db_path("attached-main");
    let attached_path = temp_db_path("attached-other");
    let _ = std::fs::remove_file(&main_path);
    let _ = std::fs::remove_file(&attached_path);

    let uri = CString::new(format!("sqlite://{}", main_path.display())).unwrap();
    let name = CString::new("sqlite-attached").unwrap();
    let connected = unsafe { take_json(dbengine::dbengine_connect(uri.as_ptr(), name.as_ptr())) };
    let conn_id = connected["id"].as_u64().expect("connection id");

    let attach_sql = CString::new(format!(
        "ATTACH DATABASE '{}' AS other; CREATE TABLE other.attached_items (id INTEGER PRIMARY KEY);",
        attached_path.display().to_string().replace('\'', "''")
    ))
    .unwrap();
    let attach_result = unsafe {
        take_json(dbengine::dbengine_execute_sqlite_sql_arrow(
            conn_id,
            attach_sql.as_ptr(),
            100,
        ))
    };
    assert_eq!(
        attach_result["row_count"], 0,
        "attach result: {attach_result}"
    );

    let catalog = unsafe { take_json(dbengine::dbengine_sqlite_catalog(conn_id)) };
    let schemas = catalog["schemas"].as_array().expect("schemas");
    let other = schemas
        .iter()
        .find(|schema| schema["name"] == "other")
        .expect("other schema");
    assert!(other["tables"].as_array().is_some_and(|tables| {
        tables
            .iter()
            .any(|table| table["name"] == "attached_items" && table["kind"] == "table")
    }));

    assert_eq!(dbengine::dbengine_disconnect(conn_id), 0);
    let _ = std::fs::remove_file(&main_path);
    let _ = std::fs::remove_file(&attached_path);
}
