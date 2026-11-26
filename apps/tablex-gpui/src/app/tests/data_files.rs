// Data files opened as tables (File > Open Data File, the command palette, a drop): the REAL DuckDB
// driver, installed through the real missing-driver flow from a local manifest into a temporary store,
// then the real connect, sidebar and grid. The file dialog is the platform's test stub. Every file lives
// in a temporary directory; the Files connection's record (in the process-wide temporary store) is removed
// when a test ends.

use crate::app::data_files::FILES_ID;

/// The Files connection has one id and the temporary store is shared by the tests of the process: these
/// tests run one at a time, and the last one's record is removed when a test ends.
static DF_STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct DfCleanup(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl DfCleanup {
    fn new() -> Self {
        Self(DF_STORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
}

impl Drop for DfCleanup {
    fn drop(&mut self) {
        let _ = engine_storage::connection::delete_connection(&FILES_ID.to_string().to_ascii_uppercase());
    }
}

fn df_key() -> String {
    FILES_ID.to_string()
}

const DF_ROWS: [[&str; 3]; 3] = [["1", "ann", "10.5"], ["2", "bob", "3.25"], ["3", "cy", "7.75"]];

/// A rig for the REAL DuckDB driver behind a local manifest, an empty sidebar and the menu bar's actions.
fn df_rig(cx: &mut TestAppContext) -> DmRig<'_> {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.dylib = built_driver("dbengine-duckdb", "");
    rig.sha = driver_sha256(&rig.dylib);
    rig.size = std::fs::metadata(&rig.dylib).unwrap().len();
    let artifact = rig.root.join("duckdb-artifact.dylib");
    std::fs::copy(&rig.dylib, &artifact).unwrap();
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "generatedAt": "2026-10-08T00:00:00Z",
        "drivers": [{
            "id": "duckdb", "displayName": "DuckDB", "version": "1.0.0",
            "appAbiVersion": 2, "uriSchemes": ["duckdb"],
            "features": ["navigator", "query.sql", "query.arrow_c_data", "operation.cancel"],
            "license": "MIT", "sourceRepo": "https://example.com/tablex", "teamId": "",
            "artifacts": [{
                "target": crate::drivers::current_target().id(),
                "url": file_url(&artifact), "sha256": rig.sha, "sizeBytes": rig.size,
            }],
            "category": "sql", "description": "DuckDB test driver",
        }],
    });
    std::fs::write(rig.root.join("manifest.json"), manifest.to_string()).unwrap();
    rig.visual.update(|window, cx| {
        actions::bind_keys(cx);
        crate::app::menus::finish_launch(cx, |_| {});
        window.activate_window();
    });
    rig.update(|this, cx| {
        // No fixture connections: the sidebar holds what the test opens.
        this.saved_connections.clear();
        this.navigator.set_connections(Vec::new());
        this.sync_sidebar_connections(cx);
        this.reload_driver_catalog(cx);
        this.refresh_driver_catalog(cx);
    });
    rig.wait("the catalog", |this| this.driver_ui.catalog_task.is_none() && this.driver_ui.is_downloadable("duckdb"));
    rig
}

/// Installs the driver through the Driver Manager, so the opens that follow do not ask.
fn df_install(rig: &mut DmRig<'_>) {
    rig.open_manager();
    rig.click("driver-action-duckdb");
    rig.wait("the install", |this| this.driver_ui.queue.job("duckdb").is_none() && this.driver_ui.handles.contains_key("duckdb"));
}

fn df_dir(rig: &DmRig<'_>) -> std::path::PathBuf {
    let dir = rig.root.join("data files");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn df_write(rig: &DmRig<'_>, name: &str, text: &str) -> std::path::PathBuf {
    let path = df_dir(rig).join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// A Parquet file from `select`, written by a DuckDB of the test's own (the driver, loaded as a copy, in
/// memory, with file access) since the app links none.
fn df_parquet(rig: &DmRig<'_>, name: &str, select: &str) -> std::path::PathBuf {
    let path = df_dir(rig).join(name);
    let copy = DriverCopy::of(&rig.dylib);
    let handle = engine_driver_host::DriverLoader.load_verified(&copy.0, &driver_sha256(&copy.0)).expect("the driver loads");
    let session = handle.connect(&serde_json::json!({ "uri": "duckdb://:memory:" }).to_string()).expect("the driver connects");
    let session = std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session));
    let sql = format!("COPY ({select}) TO '{}' (FORMAT parquet)", path.display().to_string().replace('\'', "''"));
    assert!(try_exec(&session, &sql), "{sql}");
    path
}

const DF_PEOPLE_SELECT: &str = "SELECT i::INTEGER AS id, n AS name, p::DOUBLE AS price FROM (VALUES (1, 'ann', 10.5), (2, 'bob', 3.25), (3, 'cy', 7.75)) t(i, n, p)";

fn df_pick(rig: &mut DmRig<'_>, paths: &[std::path::PathBuf]) {
    assert!(rig.visual.did_prompt_for_paths(), "Open Data File asks the platform for files");
    let picked = paths.to_vec();
    rig.visual.simulate_path_prompt_response(move |options| {
        assert!(options.files && !options.directories && options.multiple);
        Some(picked.clone())
    });
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(rig.visual);
}

/// File > Open Data File, then the dialog answered with `paths`.
fn df_menu_open(rig: &mut DmRig<'_>, paths: &[std::path::PathBuf]) {
    rig.visual.dispatch_action(crate::app::menus::OpenDataFile);
    settle(rig.visual);
    df_pick(rig, paths);
}

/// The command palette's "Open Data File", then the dialog answered with `paths`.
fn df_palette_open(rig: &mut DmRig<'_>, paths: &[std::path::PathBuf]) {
    palette_open(rig.visual);
    palette_type(rig.visual, "open data file");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    assert!(!palette_is_open(rig.visual), "Enter runs the command and closes the palette");
    df_pick(rig, paths);
}

fn df_tab_of(this: &AppRoot, view: &str) -> Option<u64> {
    this.workspace
        .tabs()
        .iter()
        .find(|tab| tab.kind == TabKind::Query && tab.query_mode.is_table_data() && tab.connection_id == df_key() && tab.collection == view)
        .map(|tab| tab.id)
}

fn df_ready(this: &AppRoot, view: &str, rows: usize) -> bool {
    df_tab_of(this, view).and_then(|tab| this.workspace.runtime(tab)).is_some_and(|runtime| {
        matches!(runtime.query.status(), crate::query::QueryStatus::Ready { .. }) && runtime.grid.row_count() == rows
    })
}

/// Waits until the table tab of `view` shows `rows` rows.
fn df_wait(rig: &mut DmRig<'_>, view: &'static str, rows: usize) {
    rig.wait("the data file's table tab", move |this| df_ready(this, view, rows));
}

/// The cells of the table tab of `view`.
fn df_grid(rig: &mut DmRig<'_>, view: &str) -> Vec<Vec<String>> {
    let view = view.to_owned();
    rig.update(|this, cx| {
        let tab = df_tab_of(this, &view).expect("the table tab is open");
        this.activate_tab_ui(tab, cx);
    });
    settle(rig.visual);
    rig.read(|this| {
        (0..this.active_grid().row_count())
            .map(|row| (0..this.active_grid().column_count()).map(|column| this.active_grid().full_cell_text(CellPosition { row, column }).unwrap_or_default()).collect())
            .collect()
    })
}

fn df_table_tabs(rig: &DmRig<'_>) -> Vec<String> {
    rig.read(|this| {
        this.workspace
            .tabs()
            .iter()
            .filter(|tab| tab.kind == TabKind::Query && tab.query_mode.is_table_data() && tab.connection_id == df_key())
            .map(|tab| tab.collection.clone())
            .collect()
    })
}

fn df_log(rig: &DmRig<'_>) -> Vec<String> {
    rig.read(|this| (0..this.error_log.log.len()).filter_map(|index| this.error_log.log.get(index)).map(|entry| entry.message.to_string()).collect())
}

fn df_notes(rig: &mut DmRig<'_>) -> usize {
    rig.visual.update(|window, cx| window.notifications(cx).len())
}

fn df_rows(expected: &[[&str; 3]]) -> Vec<Vec<String>> {
    expected.iter().map(|row| row.iter().map(|cell| cell.to_string()).collect()).collect()
}

#[gpui::test]
fn a_missing_duckdb_driver_asks_to_install_and_the_file_opens_once_it_is_there(cx: &mut TestAppContext) {
    let _cleanup = DfCleanup::new();
    let mut rig = df_rig(cx);
    let csv = df_write(&rig, "orders.csv", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");
    assert!(rig.read(|this| !this.driver_ui.has_driver("duckdb")));

    df_menu_open(&mut rig, std::slice::from_ref(&csv));
    rig.wait("the missing-driver dialog", |this| this.driver_ui.prompt.is_some());
    settle(rig.visual);
    assert!(rig.rendered("driver-prompt-body"), "the missing-driver dialog is open");
    assert!(rig.rendered("driver-prompt-install") && rig.rendered("driver-prompt-open-manager"));
    assert!(df_table_tabs(&rig).is_empty(), "nothing opens before the driver is there");

    rig.click("driver-prompt-install");
    df_wait(&mut rig, "orders", 3);
    assert!(rig.read(|this| this.driver_ui.prompt.is_none()));
    assert_eq!(df_grid(&mut rig, "orders"), df_rows(&DF_ROWS));
    // The Files connection is a saved DuckDB connection of the sidebar, in memory.
    rig.read(|this| {
        let files = this.saved_connections.iter().find(|saved| saved.id == FILES_ID).expect("the Files connection is saved");
        assert_eq!((files.name.as_str(), files.uri.as_str(), files.kind), ("Files", "duckdb://:memory:", ConnectionKind::Duckdb));
        assert!(this.navigator.visible_rows().iter().any(|row| row.label == "Files"), "the sidebar shows it");
        assert!(matches!(this.sessions.status(&df_key()), ConnectionStatus::Connected));
        let tab = df_tab_of(this, "orders").unwrap();
        let tab = this.workspace.tab(tab).unwrap();
        assert_eq!((tab.database.as_str(), tab.schema.as_deref()), ("memory", Some("main")));
    });
    let record = engine_storage::connection::get_connection(&df_key().to_ascii_uppercase()).unwrap().expect("the record is stored");
    assert_eq!(record.overrides_json, r#"{"agent.access":"off"}"#, "an agent cannot use the user's files unless the user says so");
}

#[gpui::test]
fn open_driver_manager_in_the_prompt_lands_on_duckdb_and_nothing_opens(cx: &mut TestAppContext) {
    let _cleanup = DfCleanup::new();
    let mut rig = df_rig(cx);
    let csv = df_write(&rig, "orders.csv", "id\n1\n");
    df_menu_open(&mut rig, &[csv]);
    rig.wait("the missing-driver dialog", |this| this.driver_ui.prompt.is_some());
    settle(rig.visual);
    rig.click("driver-prompt-open-manager");
    rig.wait("the abandoned connect", |this| matches!(this.sessions.status(&df_key()), ConnectionStatus::Failed(_)));
    assert!(rig.read(|this| this.workspace.active_tab().is_some_and(|tab| tab.kind == TabKind::DriverManager)));
    assert_eq!(rig.read(|this| this.driver_ui.selected.clone()).as_deref(), Some("duckdb"), "the manager opens on DuckDB");
    assert!(rig.rendered("driver-manager"));
    assert!(df_table_tabs(&rig).is_empty());
    assert!(rig.read(|this| this.data_files.waiting.is_empty()), "the file is not held for a later connect");
    assert!(df_log(&rig).iter().any(|message| message.contains("not installed") || message.contains("DuckDB")), "{:?}", df_log(&rig));
}

#[gpui::test]
fn every_format_opens_as_a_table_with_its_rows_through_the_menu(cx: &mut TestAppContext) {
    let _cleanup = DfCleanup::new();
    let mut rig = df_rig(cx);
    df_install(&mut rig);
    let csv = df_write(&rig, "orders.csv", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");
    let tsv = df_write(&rig, "stock.tsv", "id\tname\tprice\n1\tann\t10.5\n2\tbob\t3.25\n3\tcy\t7.75\n");
    let json = df_write(
        &rig,
        "users.json",
        r#"[{"id":1,"name":"ann","price":10.5},{"id":2,"name":"bob","price":3.25},{"id":3,"name":"cy","price":7.75}]"#,
    );
    let ndjson = df_write(
        &rig,
        "events.ndjson",
        "{\"id\":1,\"name\":\"ann\",\"price\":10.5}\n{\"id\":2,\"name\":\"bob\",\"price\":3.25}\n{\"id\":3,\"name\":\"cy\",\"price\":7.75}\n",
    );
    let parquet = df_parquet(&rig, "items.parquet", DF_PEOPLE_SELECT);

    // One dialog, five files: a table tab each, on the one Files connection.
    df_menu_open(&mut rig, &[csv, tsv, json, ndjson, parquet]);
    for view in ["orders", "stock", "users", "events", "items"] {
        df_wait(&mut rig, view, 3);
        assert_eq!(df_grid(&mut rig, view), df_rows(&DF_ROWS), "{view}");
    }
    assert_eq!(df_table_tabs(&rig), ["orders", "stock", "users", "events", "items"]);
    // The store is the process's (other tests' connections are in it): count the Files connection alone.
    assert_eq!(rig.read(|this| this.saved_connections.iter().filter(|saved| saved.id == FILES_ID).count()), 1, "one connection holds every file");
    assert!(df_log(&rig).iter().all(|message| !message.contains("Could not open")), "{:?}", df_log(&rig));
}

#[gpui::test]
fn the_palette_opens_files_with_spaces_quotes_and_unicode_in_their_names(cx: &mut TestAppContext) {
    let _cleanup = DfCleanup::new();
    let mut rig = df_rig(cx);
    df_install(&mut rig);
    let spaced = df_write(&rig, "my orders (final).csv", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");
    let quoted = df_write(&rig, "it's \"mine\".csv", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");
    let unicode = df_write(&rig, "dữ liệu 😀.csv", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");
    let globby = df_write(&rig, "data[1]*.csv", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");
    let upper = df_write(&rig, "UPPER.CSV", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");

    df_palette_open(&mut rig, &[spaced, quoted, unicode, globby, upper]);
    for view in ["my_orders_final", "it_s_mine", "dữ_liệu", "data_1", "UPPER"] {
        df_wait(&mut rig, view, 3);
        assert_eq!(df_grid(&mut rig, view), df_rows(&DF_ROWS), "{view}");
    }
    assert!(df_log(&rig).iter().all(|message| !message.contains("Could not open")), "{:?}", df_log(&rig));
}

#[gpui::test]
fn reopening_a_file_reuses_its_view_and_tab_and_reads_what_the_file_holds_now(cx: &mut TestAppContext) {
    let _cleanup = DfCleanup::new();
    let mut rig = df_rig(cx);
    df_install(&mut rig);
    let first = df_write(&rig, "orders.csv", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");
    let other = df_dir(&rig).join("sub");
    std::fs::create_dir_all(&other).unwrap();
    let second = other.join("orders.csv");
    std::fs::write(&second, "k\n10\n20\n").unwrap();

    df_menu_open(&mut rig, std::slice::from_ref(&first));
    df_wait(&mut rig, "orders", 3);
    // The same file again, changed on disk: the same tab, read again (and a column more).
    std::fs::write(&first, "id,name,price,extra\n1,ann,10.5,x\n2,bob,3.25,y\n3,cy,7.75,z\n4,dee,1.5,w\n").unwrap();
    df_menu_open(&mut rig, std::slice::from_ref(&first));
    df_wait(&mut rig, "orders", 4);
    assert_eq!(df_table_tabs(&rig), ["orders"], "no second tab for the same file");
    assert_eq!(df_grid(&mut rig, "orders")[3], ["4", "dee", "1.5", "w"]);
    // Another file with the same name gets a view of its own, and the first one is left alone.
    df_menu_open(&mut rig, std::slice::from_ref(&second));
    df_wait(&mut rig, "orders_2", 2);
    assert_eq!(df_grid(&mut rig, "orders_2"), [["10"], ["20"]]);
    assert_eq!(df_table_tabs(&rig), ["orders", "orders_2"]);
    assert_eq!(df_grid(&mut rig, "orders").len(), 4);
}

#[gpui::test]
fn a_file_that_cannot_be_read_says_why_and_opens_nothing(cx: &mut TestAppContext) {
    let _cleanup = DfCleanup::new();
    let mut rig = df_rig(cx);
    df_install(&mut rig);
    let good = df_write(&rig, "good.csv", "a\n1\n");
    let missing = df_dir(&rig).join("missing.csv");
    let not_a_file = df_dir(&rig).join("folder.csv");
    std::fs::create_dir_all(&not_a_file).unwrap();
    let broken = df_write(&rig, "broken.parquet", "this is not parquet");
    let notes = df_write(&rig, "notes.txt", "hello");
    #[cfg(unix)]
    let locked = {
        use std::os::unix::fs::PermissionsExt;
        let path = df_write(&rig, "locked.csv", "a\n1\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        path
    };

    // The first open connects; the good file opens beside the ones that fail.
    let before = df_notes(&mut rig);
    #[cfg(unix)]
    df_menu_open(&mut rig, &[missing, not_a_file, broken, notes, locked.clone(), good]);
    #[cfg(not(unix))]
    df_menu_open(&mut rig, &[missing, not_a_file, broken, notes, good]);
    df_wait(&mut rig, "good", 1);
    let app = rig.app.clone();
    wait_until(rig.visual, "the failures to be reported", |visual| {
        app.read_with(visual, |this, _| {
            let failures = (0..this.error_log.log.len()).filter_map(|index| this.error_log.log.get(index)).filter(|entry| entry.message.starts_with("Could not open")).count();
            failures >= if cfg!(unix) { 4 } else { 3 }
        })
    });
    settle(rig.visual);
    assert_eq!(df_table_tabs(&rig), ["good"], "only the file that could be read opened");
    let log = df_log(&rig).join("\n");
    assert!(log.contains("Could not open missing.csv: the file does not exist"), "{log}");
    assert!(log.contains("Could not open folder.csv: it is not a file"), "{log}");
    assert!(log.contains("Could not open broken.parquet:"), "{log}");
    #[cfg(unix)]
    assert!(log.contains("Could not open locked.csv: permission denied"), "{log}");
    assert!(!log.contains("notes.txt"), "a file of another kind is skipped with a message, not an error: {log}");
    // One notification each, and one for the file that is not a data file.
    assert!(df_notes(&mut rig) >= before + if cfg!(unix) { 5 } else { 4 }, "the user is told");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644));
    }
    // A pick of nothing but other files says so and does not even connect.
    let other = df_write(&rig, "other.txt", "x");
    rig.update(|this, cx| this.disconnect_connection(&df_key(), cx));
    let before = df_notes(&mut rig);
    df_menu_open(&mut rig, &[other]);
    assert_eq!(df_notes(&mut rig), before + 1);
    assert!(rig.read(|this| !matches!(this.sessions.status(&df_key()), ConnectionStatus::Connected | ConnectionStatus::Connecting)));
}

#[gpui::test]
fn the_view_pages_sorts_and_filters_like_a_table_and_sql_runs_against_it(cx: &mut TestAppContext) {
    let _cleanup = DfCleanup::new();
    let mut rig = df_rig(cx);
    df_install(&mut rig);
    let mut text = String::from("id,n\n");
    for id in 1..=250 {
        text.push_str(&format!("{id},{}\n", id * 2));
    }
    let big = df_write(&rig, "big.csv", &text);
    let parquet = df_parquet(&rig, "scores.parquet", "SELECT i::INTEGER AS id, (i * 3)::INTEGER AS score FROM range(1, 6) t(i)");
    df_menu_open(&mut rig, &[big, parquet]);
    df_wait(&mut rig, "scores", 5);
    // The grid pages: a page is the page size, the file is not read into the tab.
    let (tab, key, session, handle) = rig.read(|this| {
        let tab = df_tab_of(this, "big").expect("the tab of big.csv");
        (tab, df_key(), this.driver_runtime.sessions.get(&df_key()).cloned().unwrap(), this.driver_runtime.session_handles.get(&df_key()).cloned().unwrap())
    });
    rig.update(|this, cx| this.activate_tab_ui(tab, cx));
    settle(rig.visual);
    {
        let mut table = SqlRig { app: rig.app.clone(), visual: &mut *rig.visual, tab, key, session, handle, _driver_copy: DriverCopy(std::path::PathBuf::new()) };
        table.set_page_size(100);
        table.rerun_table("", "");
        assert_eq!(table.rows(), 100, "one page of the 250 rows");
        assert_eq!(table.paging(), (0, true), "more to come");
        assert_eq!(table.grid_rows()[0], ["1", "2"]);
        // Next page: the footer button, which reads rows 100..200 from the file.
        table.click("page-next");
        table.wait_not_running("the next page");
        assert_eq!(table.paging().0, 1);
        assert_eq!(table.grid_rows()[0], ["101", "202"]);
        // A filter, then a sort from the column header's menu: both run in DuckDB over the whole file.
        table.rerun_table("id > 240", "");
        assert_eq!(table.rows(), 10);
    }
    let generation = header_generation(&rig.app, rig.visual, tab);
    open_header_sort_menu(rig.visual, 0);
    pick_header_sort_entry(rig.visual, 2);
    rig.wait("the sorted rerun", move |this| {
        this.workspace.runtime(tab).is_some_and(|runtime| {
            runtime.query.generation() > generation && matches!(runtime.query.status(), crate::query::QueryStatus::Ready { .. })
        })
    });
    assert_eq!(rig.read(|this| this.workspace.tab(tab).unwrap().applied_order_by.clone()), "\"id\" DESC");
    let sorted = df_grid(&mut rig, "big");
    assert_eq!((sorted.len(), sorted[0].clone(), sorted[9].clone()), (10, vec!["250".to_owned(), "500".to_owned()], vec!["241".to_owned(), "482".to_owned()]));

    // The editor: SQL joins the views of two files, as on any connection.
    rig.update(|this, cx| {
        let tab = this.workspace.open_query_tab_with_metadata("SQL", TabMetadata { connection_id: df_key(), ..Default::default() });
        this.workspace.tab_mut(tab).unwrap().sql = "SELECT count(*) AS n, sum(b.score) AS total FROM big a JOIN scores b ON a.id = b.id".into();
        this.reset_active_view();
        this.run_active_tab_query(cx);
        cx.notify();
    });
    rig.wait("the join", |this| {
        this.workspace.active_tab().and_then(|tab| this.workspace.runtime(tab.id)).is_some_and(|runtime| {
            matches!(runtime.query.status(), crate::query::QueryStatus::Ready { .. }) && runtime.grid.row_count() == 1
        })
    });
    assert_eq!(rig.read(|this| this.active_grid().full_cell_text(CellPosition { row: 0, column: 0 })).as_deref(), Some("5"));
    assert_eq!(rig.read(|this| this.active_grid().full_cell_text(CellPosition { row: 0, column: 1 })).as_deref(), Some("45"));
}

#[gpui::test]
fn data_files_dropped_on_the_navigator_or_an_editor_open_as_tables_and_other_files_are_named(cx: &mut TestAppContext) {
    let _cleanup = DfCleanup::new();
    let mut rig = df_rig(cx);
    df_install(&mut rig);
    let csv = df_write(&rig, "dropped.csv", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");
    let ndjson = df_write(&rig, "second.jsonl", "{\"a\":1}\n{\"a\":2}\n");
    let notes = df_write(&rig, "notes.txt", "hello");
    let external = |paths: &[&std::path::Path]| gpui::ExternalPaths(paths.iter().map(|path| path.to_path_buf()).collect());
    let drop_at = |rig: &mut DmRig<'_>, paths: &[&std::path::Path], at: gpui::Point<gpui::Pixels>| {
        rig.visual.simulate_event(gpui::FileDropEvent::Entered { position: at, paths: external(paths) });
        rig.visual.simulate_event(gpui::FileDropEvent::Pending { position: at });
        rig.visual.simulate_event(gpui::FileDropEvent::Submit { position: at });
        rig.visual.simulate_event(gpui::FileDropEvent::Ended);
        settle(rig.visual);
    };

    // On the navigator (nothing under the pointer: a data file needs no row), with a file that is neither.
    let before = df_notes(&mut rig);
    drop_at(&mut rig, &[&csv, &notes], gpui::point(gpui::px(20.0), gpui::px(300.0)));
    df_wait(&mut rig, "dropped", 3);
    assert_eq!(df_grid(&mut rig, "dropped"), df_rows(&DF_ROWS));
    assert_eq!(df_notes(&mut rig), before + 1, "the file that is no data file gets a message");

    // On an editor tab: no connection of its own is needed either.
    rig.update(|this, cx| {
        this.workspace.open_query_tab("Scratch");
        this.reset_active_view();
        cx.notify();
    });
    settle(rig.visual);
    let editor = rig.visual.debug_bounds("query-editor-area").expect("the editor area").center();
    drop_at(&mut rig, &[&ndjson], editor);
    df_wait(&mut rig, "second", 2);
    assert_eq!(df_grid(&mut rig, "second"), [["1"], ["2"]]);
    assert_eq!(df_table_tabs(&rig), ["dropped", "second"]);
}

#[gpui::test]
fn a_reconnect_of_the_files_connection_creates_every_view_again_before_the_tabs_run(cx: &mut TestAppContext) {
    let _cleanup = DfCleanup::new();
    let mut rig = df_rig(cx);
    df_install(&mut rig);
    let orders = df_write(&rig, "orders.csv", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");
    let stock = df_write(&rig, "stock.csv", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");
    df_menu_open(&mut rig, &[orders.clone(), stock.clone()]);
    df_wait(&mut rig, "orders", 3);
    df_wait(&mut rig, "stock", 3);

    // The in-memory instance dies with the session: a row more in each file shows that the tabs ran
    // again on the new one, against views that were created again.
    std::fs::write(&orders, "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n4,dee,1.5\n").unwrap();
    std::fs::write(&stock, "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n4,dee,1.5\n").unwrap();
    rig.update(|this, cx| this.reconnect_connection(&df_key(), cx));
    rig.wait("the new session", |this| matches!(this.sessions.status(&df_key()), ConnectionStatus::Connected) && this.driver_runtime.sessions.contains_key(&df_key()));
    // The tab on screen runs at once, the other when it is shown.
    df_wait(&mut rig, "stock", 4);
    let tab = rig.read(|this| df_tab_of(this, "orders")).unwrap();
    rig.update(|this, cx| this.activate_tab_ui(tab, cx));
    df_wait(&mut rig, "orders", 4);
    assert_eq!(df_grid(&mut rig, "orders")[3], ["4", "dee", "1.5"]);
    assert_eq!(df_table_tabs(&rig), ["orders", "stock"]);
    assert!(df_log(&rig).iter().all(|message| !message.contains("does not exist") && !message.contains("Catalog Error")), "{:?}", df_log(&rig));

    // The instance holds both views (the sidebar lists what it holds).
    rig.update(|this, cx| {
        let tab = this.workspace.open_query_tab_with_metadata("SQL", TabMetadata { connection_id: df_key(), ..Default::default() });
        this.workspace.tab_mut(tab).unwrap().sql = "SELECT view_name FROM duckdb_views() WHERE NOT internal ORDER BY 1".into();
        this.reset_active_view();
        this.run_active_tab_query(cx);
        cx.notify();
    });
    rig.wait("the list of views", |this| {
        this.workspace.active_tab().and_then(|tab| this.workspace.runtime(tab.id)).is_some_and(|runtime| {
            matches!(runtime.query.status(), crate::query::QueryStatus::Ready { .. }) && runtime.grid.row_count() == 2
        })
    });
    let views: Vec<_> = (0..2).map(|row| rig.read(|this| this.active_grid().full_cell_text(CellPosition { row, column: 0 }))).collect();
    assert_eq!(views, [Some("orders".to_owned()), Some("stock".to_owned())]);
}

#[cfg(unix)]
#[gpui::test]
fn a_file_reached_through_a_symlink_reuses_the_view_and_tab_of_the_file(cx: &mut TestAppContext) {
    let _cleanup = DfCleanup::new();
    let mut rig = df_rig(cx);
    df_install(&mut rig);
    let orders = df_write(&rig, "orders.csv", "id\n1\n2\n");
    let link = df_dir(&rig).join("link.csv");
    std::os::unix::fs::symlink(&orders, &link).unwrap();
    df_menu_open(&mut rig, std::slice::from_ref(&orders));
    df_wait(&mut rig, "orders", 2);
    df_menu_open(&mut rig, std::slice::from_ref(&link));
    // The path is resolved on a background thread, then the view is created and the tab opened: give it time.
    for _ in 0..40 {
        settle(rig.visual);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(df_table_tabs(&rig), ["orders"], "the symlink is the same file: no second view or tab");
    assert!(df_log(&rig).iter().all(|message| !message.contains("Could not open")), "{:?}", df_log(&rig));
}

#[gpui::test]
fn the_files_connection_does_not_outlive_the_run_and_its_tabs_are_not_restored(cx: &mut TestAppContext) {
    let _cleanup = DfCleanup::new();
    let mut rig = df_rig(cx);
    df_install(&mut rig);
    let csv = df_write(&rig, "orders.csv", "id,name,price\n1,ann,10.5\n2,bob,3.25\n3,cy,7.75\n");
    df_menu_open(&mut rig, &[csv]);
    df_wait(&mut rig, "orders", 3);
    let id = df_key().to_ascii_uppercase();
    assert!(engine_storage::connection::get_connection(&id).is_ok_and(|record| record.is_some()), "while the app runs it is a connection like any other");

    // What the app writes when it quits.
    let session_path = rig.root.join("session.json");
    rig.update(|this, _| {
        let bytes = crate::gpui_session::encode(&this.workspace.snapshot()).unwrap();
        crate::gpui_session::save(&session_path, &bytes).unwrap();
    });

    // The next start: the stale record goes (from the list and the store), then the session is restored.
    let mut records = engine_storage::connection::list_connections().unwrap();
    assert!(records.iter().any(|record| record.id.eq_ignore_ascii_case(&id)));
    crate::app::data_files::forget_stale_record(&mut records);
    assert!(!records.iter().any(|record| record.id.eq_ignore_ascii_case(&id)));
    assert!(engine_storage::connection::list_connections().unwrap().iter().all(|record| !record.id.eq_ignore_ascii_case(&id)), "gone from the store");
    rig.update(|this, cx| {
        for tab in this.workspace.tabs().iter().map(|tab| tab.id).collect::<Vec<_>>() {
            this.close_tab(tab, cx);
        }
        this.workspace.open_welcome_tab();
        this.apply_saved_connections(records.iter().map(SavedConnection::from_record).collect(), cx);
        this.shell.session.path = Some(session_path.clone());
        this.restore_workspace_session(false, |_, _| {}, cx);
    });
    assert!(rig.read(|this| this.saved_connections.iter().all(|saved| saved.id != FILES_ID)), "no empty Files connection after a restart");
    assert!(df_table_tabs(&rig).is_empty(), "no tab on a file whose view is gone");
    assert!(rig.read(|this| this.workspace.tabs().iter().all(|tab| tab.connection_id != df_key())));
    assert!(df_log(&rig).iter().all(|message| !message.contains("orders")), "no error for a tab that was never opened: {:?}", df_log(&rig));
}
