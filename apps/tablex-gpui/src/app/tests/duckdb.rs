// DuckDB on the driver route: the REAL DuckDB driver dylib (built on demand like the SQLite one) behind
// a real host session, the real sidebar, grid editing and Apply. The database is a file in the temp
// directory, seeded through the rig's own session (a second copy of the driver could not open the
// same file in this process). Nothing else is touched.

/// A path for a DuckDB file that no other run uses; its database is named after its stem.
fn duckdb_file() -> (std::path::PathBuf, String) {
    let name = format!("tablexduck{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    (std::env::temp_dir().join(format!("{name}.duckdb")), name)
}

/// Removes the file and what DuckDB writes beside it when the test ends.
struct DuckdbFileGuard(std::path::PathBuf);

impl Drop for DuckdbFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut wal = self.0.clone().into_os_string();
        wal.push(".wal");
        let _ = std::fs::remove_file(wal);
    }
}

fn duckdb_rig<'a>(cx: &'a mut TestAppContext, path: &std::path::Path) -> SqlRig<'a> {
    nav_rig(cx, ConnectionKind::Duckdb, ("dbengine-duckdb", ""), &format!("duckdb://{}", path.display()))
}

impl SqlRig<'_> {
    /// Runs the seed on the rig's session.
    fn seed(&self, sql: &str) {
        assert!(try_exec(&self.session, sql), "the seed runs: {sql}");
    }
}

const DUCKDB_SEED: &str = "CREATE SEQUENCE item_ids START 4; \
     CREATE TABLE items(id INTEGER PRIMARY KEY DEFAULT nextval('item_ids'), name VARCHAR, qty INTEGER); \
     INSERT INTO items VALUES (1,'a',1),(2,'b',2),(3,'c',3); \
     CREATE VIEW item_names AS SELECT name FROM items; \
     CREATE TABLE owners(id INTEGER PRIMARY KEY); \
     CREATE TABLE pets(id INTEGER PRIMARY KEY, name VARCHAR, owner INTEGER REFERENCES owners(id)); \
     CREATE UNIQUE INDEX pets_name ON pets(name);";

#[gpui::test]
fn the_sidebar_expands_a_duckdb_file_through_the_driver(cx: &mut TestAppContext) {
    use crate::navigator::TableFolder;
    let (path, database) = duckdb_file();
    let _guard = DuckdbFileGuard(path.clone());
    let mut rig = duckdb_rig(cx, &path);
    rig.seed(DUCKDB_SEED);
    let key = rig.key.clone();

    rig.click_node(&connection_node(&key), "sidebar-row");
    rig.wait_children(&connection_node(&key));
    assert_eq!(rig.tree_children(&connection_node(&key)), [database.as_str()]);
    rig.expand(&database_node(&key, &database));
    assert_eq!(rig.tree_children(&database_node(&key, &database)), ["main"]);
    rig.expand(&schema_node(&key, &database, "main"));
    assert_eq!(rig.tree_children(&schema_node(&key, &database, "main")), ["Tables", "Views"]);
    let tables = group_node(&key, &database, "main", ObjectGroup::Tables);
    let views = group_node(&key, &database, "main", ObjectGroup::Views);
    rig.expand(&tables);
    rig.expand(&views);
    assert_eq!(rig.tree_children(&tables), ["items", "owners", "pets"]);
    assert_eq!(rig.tree_children(&views), ["item_names"]);

    let pets = object_node(&key, &database, "main", "pets");
    rig.expand(&pets);
    // DuckDB has no triggers: the table folders are the other four.
    assert_eq!(rig.tree_children(&pets), ["Columns", "Indexes", "Constraints", "Foreign Keys"]);
    let folder = |kind| pets.table_folder(kind).expect("a table");
    rig.expand(&folder(TableFolder::Columns));
    rig.expand(&folder(TableFolder::Indexes));
    rig.expand(&folder(TableFolder::Constraints));
    rig.expand(&folder(TableFolder::ForeignKeys));
    assert_eq!(rig.tree_children(&folder(TableFolder::Columns)), ["id", "name", "owner"]);
    assert_eq!(rig.tree_children(&folder(TableFolder::Indexes)), ["pets_name"]);
    assert!(!rig.tree_children(&folder(TableFolder::Constraints)).is_empty(), "the primary key is listed");
    assert_eq!(rig.tree_children(&folder(TableFolder::ForeignKeys)).len(), 1);
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(this.navigator.object_kind(&object_node(&key, &database, "main", "item_names")), Some(KindOfObject::View));
        assert!(this.driver_runtime.sessions.contains_key(&key), "served by the driver session");
    });
}

#[gpui::test]
fn duckdb_pages_a_read_and_runs_a_script_once(cx: &mut TestAppContext) {
    let (path, _) = duckdb_file();
    let _guard = DuckdbFileGuard(path.clone());
    let mut rig = duckdb_rig(cx, &path);
    rig.set_page_size(100);
    rig.run("SELECT i::INTEGER AS n FROM range(0, 250) t(i)");
    rig.wait_not_running("the paged read");
    assert_eq!(rig.rows(), 100);
    assert_eq!(rig.paging(), (0, true), "the first page of more");

    // A script: the rows are those of its last statement, and its writes ran once.
    rig.run("CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1), (2), (3); SELECT a FROM t ORDER BY a");
    rig.wait_not_running("the script");
    assert_eq!(rig.rows(), 3);
    assert_eq!(rig.grid_rows(), [["1"], ["2"], ["3"]]);
    rig.run("SELECT count(*) AS n, [1, 2] AS l, {'k': 'v'} AS s, 12.50::DECIMAL(10,2) AS d FROM t");
    rig.wait_not_running("a read with nested values");
    assert_eq!(rig.grid_rows()[0][0], "3");
    assert_eq!(rig.grid_rows()[0][3], "12.50");
    assert!(rig.grid_rows()[0][1].contains('1') && rig.grid_rows()[0][1].contains('2'), "{:?}", rig.grid_rows());
    assert!(rig.grid_rows()[0][2].contains('k'), "{:?}", rig.grid_rows());
}

#[gpui::test]
fn duckdb_apply_sends_edit_insert_and_delete_as_one_batch_and_persists(cx: &mut TestAppContext) {
    let (path, database) = duckdb_file();
    let _guard = DuckdbFileGuard(path.clone());
    let mut rig = duckdb_rig(cx, &path);
    rig.seed(DUCKDB_SEED);
    rig.open_table_in(&database, Some("main"), "items");
    assert_eq!(rig.rows(), 3);

    rig.edit_cell(0, 1, "alpha");
    rig.edit_cell(0, 2, "10");
    rig.stage_insert(r#"{"name":"new","qty":7}"#);
    rig.delete_row(2);
    assert_eq!(rig.staged(), (2, 2));

    let request = rig.batch_request();
    assert_eq!(request.kind, "batch");
    assert_eq!(request.object.as_deref(), Some("items"));
    assert_eq!(
        request.payload,
        serde_json::json!({"changes": [
            {"op": "update", "key": {"id": 1}, "values": {"name": "alpha", "qty": 10}},
            {"op": "insert", "values": {"name": "new", "qty": 7}},
            {"op": "delete", "key": {"id": 3}},
        ]}),
        "one update per row, then the row operations in staging order"
    );

    rig.apply();
    rig.wait_apply_finished();
    assert_eq!(rig.staged(), (0, 0));
    assert_eq!(rig.detail_error(), None);
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    rig.wait_not_running("the reload after Apply");
    let mut rows = rig.grid_rows();
    rows.sort();
    assert_eq!(rows, [["1", "alpha", "10"], ["2", "b", "2"], ["4", "new", "7"]], "the new id came from the column's default");
}

#[gpui::test]
fn a_duckdb_table_without_a_key_says_so_and_a_filter_reruns_the_select(cx: &mut TestAppContext) {
    let (path, database) = duckdb_file();
    let _guard = DuckdbFileGuard(path.clone());
    let mut rig = duckdb_rig(cx, &path);
    rig.seed("CREATE TABLE journal(msg VARCHAR); INSERT INTO journal VALUES ('x'), ('y'); CREATE TABLE items(id INTEGER PRIMARY KEY, name VARCHAR); INSERT INTO items VALUES (1, 'a'), (2, 'b'), (3, 'c');");
    rig.open_table_in(&database, Some("main"), "journal");
    rig.double_click_cell(0, 0);
    assert_eq!(rig.editing(), None);
    assert_eq!(rig.detail_error().as_deref(), Some("A single primary key is required"));

    rig.open_table_in(&database, Some("main"), "items");
    rig.rerun_table("id > 1", "id DESC");
    assert_eq!(rig.grid_rows(), [["3", "c"], ["2", "b"]]);
}

#[gpui::test]
fn duckdb_reports_its_catalog_and_database_to_the_generated_sql(cx: &mut TestAppContext) {
    let (path, database) = duckdb_file();
    let _guard = DuckdbFileGuard(path.clone());
    let rig = duckdb_rig(cx, &path);
    rig.seed("CREATE TABLE t(a INTEGER)");
    rig.app.read_with(rig.visual, |this, _| {
        let capabilities = this.driver_capabilities(&rig.key, "duckdb").expect("the driver's capabilities");
        let dialect = capabilities.sql_dialect.as_ref().expect("a dialect");
        assert!(dialect.supports_schemas && dialect.double_quote_identifiers && !dialect.bracket_identifiers);
        assert!(!capabilities.has(engine_driver_api::Capability::DatabaseCreate));
        assert!(this.sql_supports_schemas(&rig.key));
        let _ = &database;
    });
}

// A manual-commit tab sends a BEGIN before every statement; DuckDB refuses a BEGIN inside a transaction
// and the refusal aborts it, so the driver does not run one while a transaction is open.

fn ran_ok(rig: &SqlRig, what: &str) {
    assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { .. }), "{what}: {:?}", rig.status());
}

/// Rows of `sql` as the rig's shared session (a connection of its own) sees them: committed data only.
fn committed(rig: &SqlRig, sql: &str) -> u64 {
    raw_sql(rig, sql)
}

#[gpui::test]
fn a_manual_duckdb_tab_runs_statement_after_statement_in_one_transaction_and_commits_them(cx: &mut TestAppContext) {
    let (path, _) = duckdb_file();
    let _guard = DuckdbFileGuard(path.clone());
    let mut rig = duckdb_rig(cx, &path);
    rig.seed(DUCKDB_SEED);
    let a = rig.tab;
    rig.click("tx-mode-manual");

    rig.run_on(a, "INSERT INTO items VALUES (10, 'mine', 9)");
    ran_ok(&rig, "the INSERT");
    rig.run_on(a, "SELECT name FROM items ORDER BY id");
    ran_ok(&rig, "the SELECT after the INSERT");
    assert_eq!(rig.grid_rows(), [["a"], ["b"], ["c"], ["mine"]]);
    rig.run_on(a, "UPDATE items SET qty = 99 WHERE id = 10");
    ran_ok(&rig, "the UPDATE after the SELECT");
    assert_eq!(rig.pending(a), 3);
    assert_eq!(committed(&rig, "SELECT * FROM items"), 3, "nothing is committed yet");

    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    ran_ok(&rig, "the COMMIT");
    assert_eq!(committed(&rig, "SELECT * FROM items"), 4);
    assert_eq!(committed(&rig, "SELECT * FROM items WHERE id = 10 AND qty = 99"), 1);

    // The next statement opens the next transaction.
    rig.run_on(a, "DELETE FROM items WHERE id = 10");
    ran_ok(&rig, "the DELETE after the commit");
    assert_eq!(committed(&rig, "SELECT * FROM items"), 4);
    rig.click("tx-commit");
    rig.wait_tx("the second commit", a, |tx| tx.statements == 0);
    assert_eq!(committed(&rig, "SELECT * FROM items"), 3);
}

#[gpui::test]
fn rolling_back_a_manual_duckdb_tab_applies_none_of_its_statements(cx: &mut TestAppContext) {
    let (path, _) = duckdb_file();
    let _guard = DuckdbFileGuard(path.clone());
    let mut rig = duckdb_rig(cx, &path);
    rig.seed(DUCKDB_SEED);
    let a = rig.tab;
    rig.click("tx-mode-manual");

    rig.run_on(a, "INSERT INTO items VALUES (10, 'mine', 9)");
    rig.run_on(a, "SELECT * FROM items");
    ran_ok(&rig, "the SELECT");
    rig.run_on(a, "UPDATE items SET qty = 0 WHERE qty > 0");
    ran_ok(&rig, "the UPDATE");
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(committed(&rig, "SELECT * FROM items"), 3);
    assert_eq!(committed(&rig, "SELECT * FROM items WHERE qty = 0"), 0);
    rig.run_on(a, "SELECT qty FROM items ORDER BY id");
    assert_eq!(rig.grid_rows(), [["1"], ["2"], ["3"]], "the tab sees the rolled-back data");
}

#[gpui::test]
fn a_begin_and_a_commit_typed_in_a_manual_duckdb_tab_do_not_break_it(cx: &mut TestAppContext) {
    let (path, _) = duckdb_file();
    let _guard = DuckdbFileGuard(path.clone());
    let mut rig = duckdb_rig(cx, &path);
    rig.seed(DUCKDB_SEED);
    let a = rig.tab;
    rig.click("tx-mode-manual");

    // A BEGIN of the user's, alone and in front of a statement of a script.
    rig.run_on(a, "BEGIN");
    ran_ok(&rig, "the typed BEGIN");
    rig.run_on(a, "INSERT INTO items VALUES (10, 'mine', 9)");
    ran_ok(&rig, "the INSERT after it");
    rig.run_on(a, "BEGIN TRANSACTION; INSERT INTO items VALUES (11, 'two', 9)");
    ran_ok(&rig, "the script that begins again");
    rig.run_on(a, "SELECT * FROM items");
    ran_ok(&rig, "the SELECT");
    assert_eq!(rig.rows(), 5);
    assert_eq!(committed(&rig, "SELECT * FROM items"), 3);

    // A COMMIT of the user's ends it; what follows is in a transaction of its own again.
    rig.run_on(a, "COMMIT");
    ran_ok(&rig, "the typed COMMIT");
    assert_eq!(committed(&rig, "SELECT * FROM items"), 5);
    rig.run_on(a, "INSERT INTO items VALUES (12, 'three', 9)");
    ran_ok(&rig, "the INSERT after the COMMIT");
    assert_eq!(committed(&rig, "SELECT * FROM items"), 5);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(committed(&rig, "SELECT * FROM items"), 5, "only the statement after the typed COMMIT was rolled back");
}

#[gpui::test]
fn a_failed_statement_in_a_manual_duckdb_tab_aborts_the_transaction_until_it_is_ended(cx: &mut TestAppContext) {
    let (path, _) = duckdb_file();
    let _guard = DuckdbFileGuard(path.clone());
    let mut rig = duckdb_rig(cx, &path);
    rig.seed(DUCKDB_SEED);
    let a = rig.tab;
    rig.click("tx-mode-manual");

    rig.run_on(a, "INSERT INTO items VALUES (10, 'mine', 9)");
    ran_ok(&rig, "the INSERT");
    // A duplicate key: the statement fails, and like PostgreSQL the transaction is aborted.
    rig.run_on(a, "INSERT INTO items VALUES (1, 'dup', 1)");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
    rig.run_on(a, "SELECT * FROM items");
    let crate::query::QueryStatus::Failed(message) = rig.status() else { panic!("{:?}", rig.status()) };
    assert!(message.contains("transaction is aborted"), "the statement says why, not a nested BEGIN: {message}");

    // Roll Back ends it and drops the first INSERT too.
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(committed(&rig, "SELECT * FROM items"), 3);
    // And the next statement runs in a fresh transaction, which commits.
    rig.run_on(a, "INSERT INTO items VALUES (11, 'fresh', 9)");
    ran_ok(&rig, "the INSERT after the rollback");
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(committed(&rig, "SELECT * FROM items"), 4);

    // Commit of an aborted transaction applies nothing (DuckDB rolls it back and reports a success): the tab
    // marks the transaction as aborted, and Commit says it was rolled back, in a notification and the log.
    rig.run_on(a, "INSERT INTO items VALUES (12, 'lost', 9)");
    rig.run_on(a, "INSERT INTO items VALUES (1, 'dup', 1)");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
    assert!(rig.tx(a).unwrap().aborted_hint);
    assert!(rig.rendered_now("tx-hint"), "the header says the transaction is aborted");
    let notes = rig.visual.update(|window, cx| window.notifications(cx).len());
    rig.click("tx-commit");
    rig.wait_tx("the commit of the aborted transaction", a, |tx| tx.statements == 0);
    assert_eq!(committed(&rig, "SELECT * FROM items"), 4);
    assert_eq!(rig.logged_times("nothing was committed"), 1, "{:?}", rig.log_messages());
    assert_eq!(rig.visual.update(|window, cx| window.notifications(cx).len()), notes + 1, "the user is told, not left to think it was saved");
    assert!(!rig.tx(a).unwrap().aborted_hint);
    assert!(!rig.rendered_now("tx-hint"));
    rig.run_on(a, "INSERT INTO items VALUES (13, 'next', 9)");
    ran_ok(&rig, "the INSERT after it");
}

/// A file run reads a DuckDB script with DuckDB's own tokenizer: a `'` inside a backtick name is no
/// string, so this is two statements, each counted as it runs (the PostgreSQL lexer found one).
#[gpui::test]
fn a_duckdb_file_run_splits_the_script_where_duckdb_does_and_counts_each_statement(cx: &mut TestAppContext) {
    let (path, database) = duckdb_file();
    let _guard = DuckdbFileGuard(path.clone());
    let mut rig = duckdb_rig(cx, &path);
    rig.seed(DUCKDB_SEED);
    let dir = tempfile::tempdir().unwrap();
    let script = fr_write(&dir, "two.sql", "SELECT 1 AS `it's`;\nSELECT 2;\n");
    fr_open_path(&mut rig, &database, "main", &script);
    assert_eq!(fr_snapshot(&rig.app, rig.visual).first, ["SELECT 1 AS `it's`", "SELECT 2"], "the sample of the file");
    fr_to_run(&mut rig);
    let report = fr_run(&mut rig);
    assert_eq!(report.outcome, FrOutcome::Completed);
    assert_eq!((report.ok, report.failed, report.skipped, report.result_sets, report.last), (2, 0, 0, 2, 2), "{report:?}");
}
