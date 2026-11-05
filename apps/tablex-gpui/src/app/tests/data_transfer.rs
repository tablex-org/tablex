// R4b: the data transfer wizard, through the real navigator menus, the wizard tab, the shared review dialog (tables
// to create) and the REAL SQLite driver on sessions of its own (one reads, one writes). The database is read back
// with the `sqlite3` CLI; files are read back from disk.
//
// * The live tests at the end are `#[ignore]`d (dedicated `tablex-test-*` containers only).

use crate::app::data_import::run::Outcome as DtOutcome;
use crate::app::data_transfer::job::{TransferOutcome as DtTransfer, TransferReport as DtReport};
use crate::app::data_transfer::order::RowOrder;
use crate::app::data_transfer::reader::SnapshotState;
use crate::app::data_transfer::view::Snapshot as DtSnapshot;

const DT_SEED: &str = "CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, born DATE, active BOOLEAN, photo BLOB, doc TEXT, price NUMERIC); \
     INSERT INTO people VALUES (1,'Ann',1.5,'1990-05-17',1,x'00ff10','{\"a\": [1, 2]}',12.50), \
        (2,'Zoë 😀 \"q\" ''s',NULL,NULL,0,NULL,NULL,NULL), (3,'',3.25,'2001-02-03',NULL,x'',NULL,0.05); \
     CREATE TABLE orders(id INTEGER PRIMARY KEY, person_id INTEGER, total REAL); \
     INSERT INTO orders VALUES (1,1,10.0),(2,1,20.5),(3,3,7.0); \
     CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes VALUES (1,'one'),(2,'two');";

fn dt_snapshot(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> DtSnapshot {
    app.read_with(visual, |this, cx| this.transfers.values().next().expect("a transfer tab is open").read(cx).snapshot())
}

fn dt_wait(rig: &mut SqlRig<'_>, label: &'static str, ready: impl Fn(&DtSnapshot) -> bool) {
    let app = rig.app.clone();
    wait_until(rig.visual, label, |visual| ready(&dt_snapshot(&app, visual)));
    settle(rig.visual);
}

/// A menu entry of the navigator row `id`, then the list of tables.
fn dt_open(rig: &mut SqlRig<'_>, id: &NavigatorNodeId, selector: &'static str) {
    oa_menu(rig, id, selector);
    dt_wait(rig, "the list of tables", |s| s.listing == "ready");
}

/// Next on the current step; on Mapping, once every table is read.
fn dt_next(rig: &mut SqlRig<'_>) {
    rig.wait_rendered("transfer-next");
    rig.click("transfer-next");
}

/// Source -> Target -> Mapping, waiting for the columns of every table.
fn dt_to_mapping(rig: &mut SqlRig<'_>) {
    dt_next(rig);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "target");
    dt_next(rig);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "mapping");
    dt_wait(rig, "the columns of every table", |s| s.jobs.iter().all(|job| job.columns.is_some() || job.columns_error.is_some()) && s.jobs.iter().all(|job| job.action == "leave" || job.target_ready || job.action == "create" || job.target_error.is_some()));
}

/// Start (creating the tables first when there are some, through their review), then waits for the report.
fn dt_run(rig: &mut SqlRig<'_>) -> DtReport {
    rig.wait_rendered("transfer-start");
    if dt_snapshot(&rig.app, rig.visual).to_create > 0 {
        rig.click("transfer-start");
        assert!(rig.visual.debug_bounds("changes-subject").is_some(), "the review of the tables is open");
        rig.click("changes-apply");
        oa_wait_ok(rig);
        dt_wait(rig, "the tables to exist", |s| s.to_create == 0 && s.jobs.iter().all(|job| job.action == "leave" || job.target_ready));
    }
    rig.click("transfer-start");
    dt_wait(rig, "the transfer to end", |s| s.report.is_some());
    dt_snapshot(&rig.app, rig.visual).report.unwrap()
}

/// Types `text` into the input at `selector`, replacing what it holds ("" empties it).
fn dt_set(rig: &mut SqlRig<'_>, selector: &'static str, text: &str) {
    if text.is_empty() {
        let bounds = rig.visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"));
        rig.visual.simulate_click(bounds.center(), Default::default());
        rig.visual.simulate_keystrokes("secondary-a backspace");
        settle(rig.visual);
    } else {
        oa_type(rig.visual, selector, text);
    }
}

/// Selects job `index` and gives its table the name `name` (looked for when Enter is pressed).
fn dt_rename(rig: &mut SqlRig<'_>, index: usize, name: &str) {
    if dt_snapshot(&rig.app, rig.visual).selected != index {
        rig.click(match index {
            0 => "transfer-job-0",
            1 => "transfer-job-1",
            2 => "transfer-job-2",
            _ => panic!("no selector for job {index}"),
        });
    }
    oa_type(rig.visual, "transfer-job-name", name);
    rig.visual.simulate_keystrokes("enter");
    let wanted = name.to_owned();
    dt_wait(rig, "the table to be looked for", move |s| s.jobs[index].target_name == wanted && (s.jobs[index].action == "create" || s.jobs[index].target_ready));
}

fn dt_close(rig: &mut SqlRig<'_>) {
    rig.click("transfer-close");
    assert!(rig.app.read_with(rig.visual, |this, _| this.transfers.is_empty()), "the wizard is gone with its tab");
}

/// How each table that ran was read: one snapshot or not.
fn dt_snapshots(report: &DtReport) -> Vec<SnapshotState> {
    report.jobs.iter().flatten().map(|job| job.snapshot.clone()).collect()
}

fn dt_rows(report: &DtReport) -> Vec<(u64, u64)> {
    report.jobs.iter().map(|job| job.as_ref().map_or((0, 0), |job| (job.report.rows_read, job.report.inserted))).collect()
}

#[gpui::test]
fn a_table_is_copied_into_a_new_table_of_the_same_connection_through_the_wizard(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let people = oa_table(&rig, "people");
    dt_open(&mut rig, &people, "object-menu-transfer");

    // The tab, the tables of the schema (the clicked one is ticked), the first step.
    let (kind, title) = rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        (tab.kind, tab.title.clone())
    });
    assert_eq!((kind, title.as_str()), (TabKind::Transfer, "Transfer people"));
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.step, snapshot.mode, snapshot.target_kind), ("source", "tables", "table"));
    assert_eq!(snapshot.tables, [("notes".to_owned(), false), ("orders".to_owned(), false), ("people".to_owned(), true)]);
    for selector in ["transfer-source-panel", "transfer-mode", "transfer-filter", "transfer-table-0", "transfer-select-all", "transfer-next", "transfer-step-source"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }

    // Target: the connections that are open and the namespaces the navigator has loaded.
    dt_next(&mut rig);
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.step, snapshot.connections.clone(), snapshot.namespaces.clone()), ("target", vec!["driver-test".to_owned()], vec!["main".to_owned()]));
    for selector in ["transfer-target-panel", "transfer-target-kind", "transfer-target-connection", "transfer-target-namespace", "transfer-back"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }

    // Mapping: `people` exists in `main`, so it would be appended to itself: refused, and the target is renamed.
    dt_next(&mut rig);
    dt_wait(&mut rig, "the columns", |s| s.jobs.len() == 1 && s.jobs[0].columns.is_some() && s.jobs[0].target_ready);
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!((job.label.as_str(), job.action, job.exists, job.assignments.clone()), ("people", "append", Some(true), (0..8).map(Some).collect::<Vec<_>>()));
    assert!(job.sql.starts_with("SELECT * FROM \"main\".\"people\" ORDER BY \"id\""), "paged in the order of the primary key: {}", job.sql);
    for selector in ["transfer-mapping-panel", "transfer-job-0", "transfer-job-status-0", "transfer-job-name", "transfer-job-action", "transfer-map-7", "transfer-map-type-1"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // Next is off, and the note says why.
    rig.click("transfer-next");
    assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "mapping");
    assert!(rig.visual.debug_bounds("transfer-note-general-0").is_some(), "the note says the table would be copied onto itself");

    oa_type(rig.visual, "transfer-job-name", "people_copy");
    rig.visual.simulate_keystrokes("enter");
    dt_wait(&mut rig, "the table to be looked for", |s| s.jobs[0].action == "create" && s.jobs[0].exists == Some(false));
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!(job.primary_key, ["id"]);
    assert_eq!(
        job.new_columns.iter().map(|(_, name, data_type, nullable)| (name.as_str(), data_type.as_str(), *nullable)).collect::<Vec<_>>(),
        [("id", "INTEGER", false), ("name", "TEXT", false), ("score", "REAL", true), ("born", "DATE", true), ("active", "BOOLEAN", true), ("photo", "BLOB", true), ("doc", "TEXT", true), ("price", "NUMERIC", true)],
        "the types and the NOT NULLs of the source"
    );
    dt_next(&mut rig);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "run");
    assert!(rig.visual.debug_bounds("transfer-batch").is_some() && rig.visual.debug_bounds("transfer-transaction").is_some() && rig.visual.debug_bounds("transfer-on-error").is_some());

    // The table does not exist: its CREATE TABLE is reviewed, nothing exists before it is applied.
    rig.click("transfer-start");
    assert!(rig.visual.debug_bounds("changes-subject").is_some());
    let script = oa_script(rig.visual);
    assert!(script.starts_with("CREATE TABLE \"main\".\"people_copy\" ("), "{script}");
    assert!(script.contains("\"id\" INTEGER NOT NULL") && script.contains("PRIMARY KEY (\"id\")") && script.contains("\"photo\" BLOB"), "{script}");
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'people_copy'"), "0");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    dt_wait(&mut rig, "the transfer to learn the table exists", |s| s.to_create == 0 && s.jobs[0].action == "append" && s.jobs[0].target_ready);
    assert_eq!(db.sql("SELECT count(*) FROM people_copy"), "0", "nothing copied yet");

    let report = {
        rig.click("transfer-start");
        dt_wait(&mut rig, "the transfer to end", |s| s.report.is_some());
        dt_snapshot(&rig.app, rig.visual).report.unwrap()
    };
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert_eq!(dt_rows(&report), [(3, 3)]);
    for selector in ["transfer-summary", "transfer-headline", "transfer-stats", "transfer-result-0", "transfer-again", "transfer-close"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // Every value arrived: text with quotes and emoji, REAL, dates, booleans, NULLs, a BLOB (also an empty one), JSON text, NUMERIC.
    assert_eq!(db.sql("SELECT count(*) FROM (SELECT * FROM people EXCEPT SELECT * FROM people_copy)"), "0");
    assert_eq!(db.sql("SELECT count(*) FROM (SELECT * FROM people_copy EXCEPT SELECT * FROM people)"), "0");
    assert_eq!(db.sql("SELECT hex(photo), typeof(photo) FROM people_copy WHERE id = 1"), "00FF10|blob");
    assert_eq!(db.sql("SELECT typeof(photo), length(photo) FROM people_copy WHERE id = 3"), "blob|0", "an empty BLOB is not NULL");
    assert_eq!(db.sql("SELECT name FROM people_copy WHERE id = 2"), "Zoë 😀 \"q\" 's");
    assert_eq!(db.sql("SELECT typeof(active) || typeof(score) || typeof(born) FROM people_copy WHERE id = 2"), "integernullnull");
    // The navigator lists the new table.
    let (app, group, key) = (rig.app.clone(), group_node(&rig.key, "main", "main", ObjectGroup::Tables), rig.key.clone());
    let wanted = object_node(&key, "main", "main", "people_copy");
    wait_until(rig.visual, "the Tables group to list the new table", |visual| app.read_with(visual, |this, _| this.navigator.loaded_children(&group).is_some_and(|children| children.contains(&wanted))));
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
    dt_close(&mut rig);
}

/// The wizard on a query, opened the way the result's Export menu opens it.
fn dt_open_query(rig: &mut SqlRig<'_>, sql: &str) {
    rig.run(sql);
    rig.wait_not_running("the query to run");
    let (tab, app) = (rig.tab, rig.app.clone());
    assert!(app.read_with(rig.visual, |this, _| this.transfer_available(tab)), "the result of a SQL tab can be transferred");
    rig.visual.update(|window, cx| app.update(cx, |this, cx| this.open_transfer_from_tab(tab, window, cx)));
    settle(rig.visual);
}

#[gpui::test]
fn a_query_is_copied_into_a_new_table_with_its_own_columns(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    dt_open_query(&mut rig, "SELECT id, name, score * 2 AS double_score, upper(name) AS shout FROM people WHERE id > 1 ORDER BY id;");
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.step, snapshot.mode, snapshot.target_kind), ("source", "query", "file"));
    assert!(rig.visual.debug_bounds("transfer-query").is_some(), "the statement is in a box of its own");
    // The statement is the one that ran, without its semicolon.
    let tab = rig.app.read_with(rig.visual, |this, _| this.workspace.active_tab().unwrap().clone());
    assert_eq!(tab.kind, TabKind::Transfer);

    // To a table of the same connection.
    dt_next(&mut rig);
    rig.visual.update(|window, cx| {
        let view = rig.app.read(cx).transfers.values().next().cloned().unwrap();
        view.update(cx, |view, cx| view.choose_target_kind_for_test(false, window, cx));
    });
    settle(rig.visual);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).target_kind, "table");
    dt_next(&mut rig);
    dt_wait(&mut rig, "the columns of the query", |s| s.jobs.len() == 1 && s.jobs[0].columns.is_some());
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!((job.label.as_str(), job.target_name.as_str(), job.action, job.exists), ("Query", "query_result", "create", Some(false)));
    assert_eq!(job.columns.as_ref().unwrap().iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["id", "name", "double_score", "shout"]);
    assert!(job.sql.contains("WHERE id > 1 ORDER BY id\n) AS transfer_page ORDER BY 1, ") && !job.sql.ends_with(';'), "the user's order leads and the columns break its ties: {}", job.sql);
    // Name it, leave a column out, retype another.
    oa_type(rig.visual, "transfer-job-name", "scores");
    rig.click("transfer-include-3");
    oa_type(rig.visual, "transfer-new-type-2", "NUMERIC");
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert_eq!(dt_rows(&report), [(2, 2)]);
    assert_eq!(db.sql("SELECT id || '|' || name || '|' || ifnull(double_score, '~') FROM scores ORDER BY id"), "2|Zoë 😀 \"q\" 's|~\n3||6.5");
    assert_eq!(db.sql("SELECT count(*) FROM pragma_table_info('scores')"), "3", "the column that was left out is not in the table");
    assert_eq!(db.sql("SELECT type FROM pragma_table_info('scores') WHERE name = 'double_score'"), "NUMERIC");
}

#[gpui::test]
fn a_schema_is_written_to_a_folder_with_a_file_per_table(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let folder = tempfile::tempdir().unwrap();
    let out = folder.path().join("export");
    dt_open(&mut rig, &schema_node(&key, "main", "main"), "object-menu-export");
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.tables.iter().filter(|(_, selected)| *selected).count(), snapshot.target_kind), (3, "file"), "every table of the schema is ticked, and the target is a file");
    dt_next(&mut rig);
    for selector in ["transfer-format", "transfer-folder", "transfer-browse", "transfer-overwrite"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(rig.visual.debug_bounds("transfer-file-name").is_none(), "several tables: the files are named after them");
    // The folder does not exist yet: it is made. Enter in the field goes on.
    oa_type(rig.visual, "transfer-folder", &out.display().to_string());
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    dt_wait(&mut rig, "the files to be listed", |s| s.step == "mapping" && s.jobs.len() == 3 && s.jobs.iter().all(|job| job.columns.is_some()));
    for selector in ["transfer-job-0", "transfer-job-1", "transfer-job-2"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(rig.visual.debug_bounds("transfer-job-name").is_none(), "files have no target table to map");
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert_eq!(dt_rows(&report), [(2, 2), (3, 3), (3, 3)]);
    let mut names: Vec<String> = std::fs::read_dir(&out).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["notes.csv", "orders.csv", "people.csv"], "no partial file is left");
    assert_eq!(std::fs::read_to_string(out.join("notes.csv")).unwrap(), "id,body\n1,one\n2,two\n");
    assert_eq!(std::fs::read_to_string(out.join("orders.csv")).unwrap(), "id,person_id,total\n1,1,10\n2,1,20.5\n3,3,7\n");
    let people = std::fs::read_to_string(out.join("people.csv")).unwrap();
    assert!(people.starts_with("id,name,score,born,active,photo,doc,price\n1,Ann,1.5,1990-05-17,1,0x00ff10,\"{\"\"a\"\": [1, 2]}\",12.5\n"), "{people}");
    assert!(people.contains("2,\"Zoë 😀 \"\"q\"\" 's\",,,0,,,\n") && people.contains("3,\"\",3.25,2001-02-03,,0x,,0.05\n"), "{people}");
    for selector in ["transfer-file", "transfer-result-2"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
}

#[gpui::test]
fn several_tables_are_picked_filtered_and_transferred_together_into_old_and_new_tables(cx: &mut TestAppContext) {
    let db = SqliteDb::new(&format!("{DT_SEED} CREATE TABLE archive_notes(id INTEGER PRIMARY KEY, body TEXT, extra TEXT DEFAULT 'kept');"));
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    // The toolbar's Data Transfer works from the row selected in the navigator: a schema offers all its tables.
    click_node_part(&rig.app, rig.visual, &schema_node(&key, "main", "main"), "sidebar-row");
    rig.click("global-transfer");
    dt_wait(&mut rig, "the list of tables", |s| s.listing == "ready");
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.tables.iter().map(|(name, selected)| (name.as_str(), *selected)).collect::<Vec<_>>(), [("archive_notes", true), ("notes", true), ("orders", true), ("people", true)], "from a schema every table is ticked");

    // Next waits for a table; Clear unticks them all, Select all ticks what the filter shows.
    rig.click("transfer-select-none");
    assert_eq!(dt_snapshot(&rig.app, rig.visual).tables.iter().filter(|(_, selected)| *selected).count(), 0);
    dt_next(&mut rig);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "source", "nothing is ticked: nowhere to go");
    oa_type(rig.visual, "transfer-filter", "rs");
    assert_eq!(dt_snapshot(&rig.app, rig.visual).shown, 1, "the filter shows the tables whose name has it");
    rig.click("transfer-select-all");
    dt_set(&mut rig, "transfer-filter", "");
    assert_eq!(dt_snapshot(&rig.app, rig.visual).shown, 4);
    // archive_notes is row 0, notes row 1: tick notes with its box.
    rig.click("transfer-table-1");
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.tables.iter().filter(|(_, selected)| *selected).map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["notes", "orders"], "only what the filter showed was ticked");
    assert!(rig.visual.debug_bounds("transfer-selected-count").is_some());

    // Two tables, one list; `notes` goes to the table that exists (extra column kept), `orders` to a new one.
    dt_to_mapping(&mut rig);
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.jobs.iter().map(|job| (job.label.as_str(), job.action)).collect::<Vec<_>>(), [("notes", "append"), ("orders", "append")]);
    oa_type(rig.visual, "transfer-job-name", "archive_notes");
    rig.visual.simulate_keystrokes("enter");
    dt_wait(&mut rig, "archive_notes to be read", |s| s.jobs[0].target_ready && s.jobs[0].assignments.len() == 2);
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!((job.target_name.as_str(), job.assignments.clone()), ("archive_notes", vec![Some(0), Some(1)]), "id and body found their namesakes, extra is not fed");
    assert!(job.notes.is_empty(), "extra has a default: {:?}", job.notes);
    // The other table: select its row, rename the target.
    rig.click("transfer-job-1");
    assert_eq!(dt_snapshot(&rig.app, rig.visual).selected, 1);
    oa_type(rig.visual, "transfer-job-name", "orders_copy");
    rig.visual.simulate_keystrokes("enter");
    dt_wait(&mut rig, "orders_copy to be looked for", |s| s.jobs[1].action == "create");
    dt_next(&mut rig);

    // One review for the one table to create; then both are transferred in one run.
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert_eq!(dt_rows(&report), [(2, 2), (3, 3)]);
    assert_eq!(db.sql("SELECT id || ':' || body || ':' || extra FROM archive_notes ORDER BY id"), "1:one:kept\n2:two:kept");
    assert_eq!(db.sql("SELECT count(*) FROM orders_copy"), "3");
    assert_eq!(db.sql("SELECT sum(total) FROM orders_copy"), "37.5");
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.diagram.applies.len()), 0, "a transfer holds no structure-apply slot");
}

#[gpui::test]
fn the_columns_of_a_table_are_mapped_by_hand_and_a_table_can_be_left_out(cx: &mut TestAppContext) {
    let db = SqliteDb::new(&format!("{DT_SEED} CREATE TABLE slim(ident INTEGER PRIMARY KEY, label TEXT, spare TEXT NOT NULL);"));
    let mut rig = oa_rig(cx, &db);
    let people = oa_table(&rig, "people");
    dt_open(&mut rig, &people, "object-menu-transfer");
    // Tick orders as well: the jobs are orders, then people. People is left out again on the Mapping step.
    rig.click("transfer-table-1");
    dt_to_mapping(&mut rig);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).jobs.iter().map(|job| job.label.as_str()).collect::<Vec<_>>(), ["orders", "people"]);
    dt_rename(&mut rig, 0, "slim");
    dt_wait(&mut rig, "slim to be read", |s| s.jobs[0].target_ready && s.jobs[0].assignments.iter().all(Option::is_none));
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert!(job.notes.iter().any(|note| note.contains("\"spare\" does not accept NULL")), "no column of orders is called like a column of slim: {:?}", job.notes);
    // `person_id` goes to `label` (the select lists (skip), ident, label, spare); `id` to `ident`. A target takes one source.
    di_choose(&mut rig, "transfer-map-0", 1);
    di_choose(&mut rig, "transfer-map-1", 2);
    di_choose(&mut rig, "transfer-map-2", 2);
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!(job.assignments[..3], [Some(0), None, Some(1)], "total took label from person_id");
    di_choose(&mut rig, "transfer-map-1", 2);
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!(job.assignments[..3], [Some(0), Some(1), None], "person_id took it back: a target takes one source at a time");
    di_choose(&mut rig, "transfer-map-1", 1);
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!(job.assignments[..3], [Some(0), Some(2), None]);
    assert!(job.notes.iter().any(|note| note.contains("\"person_id\" can hold NULL but \"spare\" does not accept it")) && !job.notes.iter().any(|note| note.contains("no column goes to it")), "{:?}", job.notes);
    // The second table: leave it out with its box.
    rig.click("transfer-job-include-1");
    assert_eq!(dt_snapshot(&rig.app, rig.visual).jobs[1].action, "leave");
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!(dt_rows(&report), [(3, 3)], "only the first table ran");
    assert_eq!(report.jobs.len(), 1, "the table that was left out is not in the plan");
    assert_eq!(db.sql("SELECT ident || ':' || spare FROM slim ORDER BY ident"), "1:1\n2:1\n3:3");
    assert_eq!(db.sql("SELECT count(*) FROM slim WHERE label IS NOT NULL"), "0");
}

const STRICT_SEED: &str = "CREATE TABLE strict(id INTEGER PRIMARY KEY, name TEXT NOT NULL CHECK(length(name) > 0));";

/// `people` onto `strict` (its CHECK refuses the empty name of row 3), to the Run step.
fn dt_to_strict(rig: &mut SqlRig<'_>) {
    let people = oa_table(rig, "people");
    dt_open(rig, &people, "object-menu-transfer");
    dt_to_mapping(rig);
    oa_type(rig.visual, "transfer-job-name", "strict");
    rig.visual.simulate_keystrokes("enter");
    dt_wait(rig, "strict to be read", |s| s.jobs[0].target_ready && s.jobs[0].assignments.iter().flatten().count() == 2);
    dt_next(rig);
}

#[gpui::test]
fn a_row_the_target_refuses_stops_the_transfer_or_is_skipped_and_listed(cx: &mut TestAppContext) {
    let db = SqliteDb::new(&format!("{DT_SEED} {STRICT_SEED}"));
    let mut rig = oa_rig(cx, &db);
    dt_to_strict(&mut rig);
    // Stop (the default): the batch with the refused row is not kept.
    let report = dt_run(&mut rig);
    let DtTransfer::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
    assert!(reason.contains("people") && reason.contains("rows 1\u{2013}3") && reason.contains("CHECK constraint failed"), "{reason}");
    assert_eq!(db.sql("SELECT count(*) FROM strict"), "0");
    assert!(rig.visual.debug_bounds("transfer-headline").is_some() && rig.visual.debug_bounds("transfer-failure").is_some());
    rig.click("transfer-again");
    assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "source");
    dt_to_run_again(&mut rig);

    // Skip: the good rows go in, the refused one is listed with its number.
    di_choose(&mut rig, "transfer-on-error", 1);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    let result = report.jobs[0].as_ref().unwrap();
    assert_eq!((result.report.inserted, result.report.skipped, result.report.rows_read), (2, 1, 3));
    assert_eq!(result.report.errors[0].row, 3);
    assert!(result.report.errors[0].message.contains("CHECK constraint failed"), "{:?}", result.report.errors);
    assert_eq!(db.sql("SELECT group_concat(id) FROM strict"), "1,2");
    for selector in ["transfer-errors", "transfer-error-0"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    let warnings = rig.app.read_with(rig.visual, |this, _| (0..this.error_log.log.len()).filter_map(|index| this.error_log.log.get(index)).filter(|entry| entry.severity == crate::error_log::Severity::Warning).count());
    assert_eq!(warnings, 2, "the stop and the skipped rows are said once each in the Error Log");
}

/// Back to the Run step on the same tables after "New transfer" (the mapping is read again).
fn dt_to_run_again(rig: &mut SqlRig<'_>) {
    dt_to_mapping(rig);
    dt_wait(rig, "the target to be read", |s| s.jobs.iter().all(|job| job.target_ready || job.action == "create"));
    let name = dt_snapshot(&rig.app, rig.visual).jobs[0].target_name.clone();
    if name != "strict" {
        oa_type(rig.visual, "transfer-job-name", "strict");
        rig.visual.simulate_keystrokes("enter");
        dt_wait(rig, "strict to be read", |s| s.jobs[0].target_ready && s.jobs[0].assignments.iter().flatten().count() == 2);
    }
    dt_next(rig);
}

#[gpui::test]
fn a_table_that_cannot_be_transferred_stops_the_rest_or_is_listed_and_the_others_go_on(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let notes = oa_table(&rig, "notes");
    dt_open(&mut rig, &notes, "object-menu-transfer");
    rig.click("transfer-table-1"); // orders
    dt_to_mapping(&mut rig);
    dt_rename(&mut rig, 0, "notes_copy");
    dt_rename(&mut rig, 1, "orders_copy");
    dt_next(&mut rig);
    // Create the two tables (one review), then the source of the first is dropped under the wizard.
    rig.click("transfer-start");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    dt_wait(&mut rig, "the tables to exist", |s| s.to_create == 0 && s.jobs.iter().all(|job| job.target_ready));
    db.sql("DROP TABLE notes");

    // Stop: the first table fails, the second does not run.
    rig.click("transfer-start");
    dt_wait(&mut rig, "the transfer to end", |s| s.report.is_some());
    let report = dt_snapshot(&rig.app, rig.visual).report.unwrap();
    let DtTransfer::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
    assert!(reason.starts_with("notes: ") && reason.contains("no such table"), "{reason}");
    assert!(report.jobs[0].is_some() && report.jobs[1].is_none());
    assert_eq!(db.sql("SELECT count(*) FROM orders_copy"), "0");

    // Skip: the first is listed as failed, the second is transferred. The source is back for the wizard to read, and gone again for the run.
    db.sql("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT)");
    rig.click("transfer-again");
    dt_to_mapping(&mut rig);
    dt_rename(&mut rig, 0, "notes_copy");
    dt_rename(&mut rig, 1, "orders_copy");
    dt_next(&mut rig);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "run");
    db.sql("DROP TABLE notes");
    di_choose(&mut rig, "transfer-on-error", 1);
    rig.click("transfer-start");
    dt_wait(&mut rig, "the transfer to end", |s| s.report.is_some());
    let report = dt_snapshot(&rig.app, rig.visual).report.unwrap();
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert!(matches!(&report.jobs[0].as_ref().unwrap().report.outcome, DtOutcome::Failed(reason) if reason.contains("no such table")));
    assert_eq!(report.jobs[1].as_ref().unwrap().report.inserted, 3);
    assert_eq!(db.sql("SELECT count(*) FROM orders_copy"), "3");
    for selector in ["transfer-result-0", "transfer-result-1", "transfer-failure"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
}

const DT_HEAVY: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 3000000) SELECT x, x * 2 AS y FROM c";

/// A query of three million rows to the Run step: to files in `folder` (`to_table`: into a new table `big`).
fn dt_heavy_to_run(rig: &mut SqlRig<'_>, folder: Option<&std::path::Path>) {
    dt_open_query(rig, DT_HEAVY);
    dt_next(rig);
    match folder {
        Some(folder) => {
            oa_type(rig.visual, "transfer-folder", &folder.display().to_string());
            oa_type(rig.visual, "transfer-file-name", "heavy.csv");
        }
        None => {
            rig.app.update(rig.visual, |_, _| ());
            rig.visual.update(|window, cx| {
                let view = rig.app.read(cx).transfers.values().next().cloned().unwrap();
                view.update(cx, |view, cx| view.choose_target_kind_for_test(false, window, cx));
            });
            settle(rig.visual);
        }
    }
    dt_next(rig);
    dt_wait(rig, "the columns", |s| s.jobs.len() == 1 && s.jobs[0].columns.is_some());
    if folder.is_none() {
        dt_rename(rig, 0, "big");
    }
    dt_next(rig);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "run");
}

#[gpui::test]
fn cancel_stops_a_running_export_and_leaves_no_file_and_escape_does_not_cancel(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let folder = tempfile::tempdir().unwrap();
    dt_heavy_to_run(&mut rig, Some(folder.path()));
    rig.click("transfer-start");
    dt_wait(&mut rig, "some rows to be written", |s| s.progress_rows.is_some_and(|rows| rows > 2_000));
    for selector in ["transfer-progress", "transfer-progress-bar", "transfer-cancel", "transfer-running-0"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // Escape is for dialogs and menus: it does not cancel a transfer.
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert!(dt_snapshot(&rig.app, rig.visual).running, "Escape did not cancel the transfer");
    // Back and Next do nothing while it runs.
    assert!(rig.visual.debug_bounds("transfer-back").is_none() && rig.visual.debug_bounds("transfer-next").is_none());
    rig.click("transfer-cancel");
    dt_wait(&mut rig, "the cancelled transfer to end", |s| s.report.is_some());
    let report = dt_snapshot(&rig.app, rig.visual).report.unwrap();
    assert_eq!(report.outcome, DtTransfer::Cancelled);
    assert_eq!(report.jobs[0].as_ref().unwrap().report.outcome, DtOutcome::Cancelled);
    assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 0, "a cancelled export leaves neither the file nor its .part");
    assert!(rig.visual.debug_bounds("transfer-headline").is_some());
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.diagram.applies.len()), 0);
}

fn dt_cancel_into_table(cx: &mut TestAppContext, transaction_steps: i32) -> (SqliteDb, DtReport) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    dt_heavy_to_run(&mut rig, None);
    // Make the table (its review), then one row per batch: slow enough to cancel in the middle.
    rig.click("transfer-start");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    dt_wait(&mut rig, "the table to exist", |s| s.to_create == 0 && s.jobs[0].target_ready);
    dt_set(&mut rig, "transfer-batch", "1");
    if transaction_steps > 0 {
        di_choose(&mut rig, "transfer-transaction", transaction_steps);
    }
    rig.click("transfer-start");
    dt_wait(&mut rig, "some rows to go in", |s| s.progress_rows.is_some_and(|rows| rows > 300));
    rig.click("transfer-cancel");
    dt_wait(&mut rig, "the cancelled transfer to end", |s| s.report.is_some());
    let report = dt_snapshot(&rig.app, rig.visual).report.unwrap();
    assert_eq!(report.outcome, DtTransfer::Cancelled);
    drop(rig);
    (db, report)
}

#[gpui::test]
fn cancel_keeps_what_was_committed_per_batch(cx: &mut TestAppContext) {
    let (db, report) = dt_cancel_into_table(cx, 0);
    let stored: u64 = db.sql("SELECT count(*) FROM big").parse().unwrap();
    let job = &report.jobs[0].as_ref().unwrap().report;
    assert!(stored > 100 && stored < 3_000_000, "{stored}");
    assert_eq!((job.inserted, job.rolled_back), (stored, 0));
}

#[gpui::test]
fn cancel_in_one_transaction_per_table_rolls_that_table_back(cx: &mut TestAppContext) {
    let (db, report) = dt_cancel_into_table(cx, 1);
    assert_eq!(db.sql("SELECT count(*) FROM big"), "0");
    let job = &report.jobs[0].as_ref().unwrap().report;
    assert_eq!(job.inserted, 0);
    assert!(job.rolled_back > 100, "{}", job.rolled_back);
}

#[gpui::test]
fn a_read_only_connection_exports_but_takes_no_tables_and_production_asks_first(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let db = SqliteDb::new(&format!("{DT_SEED} {STRICT_SEED}"));
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let folder = tempfile::tempdir().unwrap();
    // Read-only: reading is fine, so the wizard opens and files can be written; tables are refused when the transfer starts.
    publish_types(&rig.app, rig.visual, true);
    give_profile(&rig.app, rig.visual, &key, typed(PRODUCTION));
    dt_to_strict(&mut rig);
    rig.click("transfer-start");
    let message = dt_snapshot(&rig.app, rig.visual).message.expect("the refusal is shown in the wizard");
    assert!(message.contains("Transferring data is") && message.contains("read-only"), "{message}");
    assert_eq!(db.sql("SELECT count(*) FROM strict"), "0");
    assert!(!dt_snapshot(&rig.app, rig.visual).running);
    rig.click("transfer-back");
    rig.click("transfer-back");
    rig.click("transfer-back");
    let _ = &folder;

    // Production, writable: the Transfer button asks first; Escape declines, Enter confirms.
    publish_types(&rig.app, rig.visual, false);
    dt_next(&mut rig);
    dt_next(&mut rig);
    dt_wait(&mut rig, "the table to be read", |s| s.jobs[0].target_ready);
    dt_next(&mut rig);
    di_choose(&mut rig, "transfer-on-error", 1);
    rig.click("transfer-start");
    assert!(rig.visual.debug_bounds("transfer-confirm").is_some(), "Production asks for its confirmation");
    assert_eq!(db.sql("SELECT count(*) FROM strict"), "0", "nothing before the answer");
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert!(dt_snapshot(&rig.app, rig.visual).report.is_none() && !dt_snapshot(&rig.app, rig.visual).running, "declined: nothing ran");
    rig.click("transfer-start");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    dt_wait(&mut rig, "the confirmed transfer to end", |s| s.report.is_some());
    assert_eq!(db.sql("SELECT group_concat(id) FROM strict"), "1,2");

    // The connection turns read-only while the wizard is open: the next start is refused.
    rig.click("transfer-again");
    dt_to_run_again(&mut rig);
    publish_types(&rig.app, rig.visual, true);
    rig.click("transfer-start");
    let message = dt_snapshot(&rig.app, rig.visual).message.expect("the refusal is shown in the wizard");
    assert!(message.contains("read-only"), "{message}");
    assert_eq!(db.sql("SELECT count(*) FROM strict"), "2");
}

const HOSTILE_SEED: &str = "CREATE TABLE decoy(id INTEGER); INSERT INTO decoy VALUES (1); \
     CREATE TABLE \"t\"\"x; DROP TABLE decoy; --\"(id INTEGER PRIMARY KEY, \"a\"\"b\" TEXT, \"c d; --\" TEXT); \
     INSERT INTO \"t\"\"x; DROP TABLE decoy; --\" VALUES (1, 'it''s', 'x\"y'), (2, '''; DELETE FROM decoy; --', 'z'); \
     CREATE TABLE \"../../escape\"(v TEXT); INSERT INTO \"../../escape\" VALUES ('e');";

#[gpui::test]
fn hostile_names_are_quoted_in_files_and_statements_and_never_leave_the_folder(cx: &mut TestAppContext) {
    let table = "t\"x; DROP TABLE decoy; --";
    let db = SqliteDb::new(HOSTILE_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let parent = tempfile::tempdir().unwrap();
    let out = parent.path().join("out");
    // Every table of the schema to SQL INSERT files.
    dt_open(&mut rig, &schema_node(&key, "main", "main"), "object-menu-export");
    dt_next(&mut rig);
    di_choose(&mut rig, "transfer-format", 2);
    oa_type(rig.visual, "transfer-folder", &out.display().to_string());
    dt_next(&mut rig);
    dt_wait(&mut rig, "the tables", |s| s.jobs.len() == 3 && s.jobs.iter().all(|job| job.columns.is_some()));
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    let mut names: Vec<String> = std::fs::read_dir(&out).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["_.._escape.sql", "decoy.sql", "t_x; DROP TABLE decoy; --.sql"], "an object name never makes a path");
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1, "nothing outside the folder");
    let sql = std::fs::read_to_string(out.join("t_x; DROP TABLE decoy; --.sql")).unwrap();
    assert!(sql.starts_with("INSERT INTO \"main\".\"t\"\"x; DROP TABLE decoy; --\" (\"id\", \"a\"\"b\", \"c d; --\") VALUES\n"), "{sql}");
    assert!(sql.contains("(1, 'it''s', 'x\"y'),\n(2, '''; DELETE FROM decoy; --', 'z');"), "{sql}");
    // The file is replayed into a copy of the table: the statements do what they say and nothing else.
    let copy = SqliteDb::new("CREATE TABLE decoy(id INTEGER); INSERT INTO decoy VALUES (1); CREATE TABLE \"t\"\"x; DROP TABLE decoy; --\"(id INTEGER PRIMARY KEY, \"a\"\"b\" TEXT, \"c d; --\" TEXT);");
    let status = std::process::Command::new("sqlite3").arg(&copy.0).stdin(std::fs::File::open(out.join("t_x; DROP TABLE decoy; --.sql")).unwrap()).status().unwrap();
    assert!(status.success());
    assert_eq!(copy.sql("SELECT count(*) FROM decoy"), "1");
    assert_eq!(copy.sql(&format!("SELECT \"a\"\"b\" || '|' || \"c d; --\" FROM \"{}\" ORDER BY id", table.replace('"', "\"\""))), "it's|x\"y\n'; DELETE FROM decoy; --|z");
    rig.click("transfer-close");

    // Into a new table of a hostile name, with hostile column names.
    let hostile = oa_table(&rig, table);
    dt_open(&mut rig, &hostile, "object-menu-transfer");
    dt_to_mapping(&mut rig);
    dt_rename(&mut rig, 0, "n\"w; DROP TABLE decoy; --");
    dt_next(&mut rig);
    rig.click("transfer-start");
    let script = oa_script(rig.visual);
    assert!(script.starts_with("CREATE TABLE \"main\".\"n\"\"w; DROP TABLE decoy; --\" ("), "{script}");
    assert!(script.contains("\"a\"\"b\" TEXT") && script.contains("\"c d; --\" TEXT"), "{script}");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    dt_wait(&mut rig, "the table to exist", |s| s.to_create == 0 && s.jobs[0].target_ready);
    rig.click("transfer-start");
    dt_wait(&mut rig, "the transfer to end", |s| s.report.is_some());
    let report = dt_snapshot(&rig.app, rig.visual).report.unwrap();
    assert_eq!(dt_rows(&report), [(2, 2)]);
    assert_eq!(db.sql("SELECT count(*) FROM decoy"), "1", "no statement was smuggled in");
    assert_eq!(db.sql("SELECT \"a\"\"b\" || '|' || \"c d; --\" FROM \"n\"\"w; DROP TABLE decoy; --\" ORDER BY id"), "it's|x\"y\n'; DELETE FROM decoy; --|z");
}

#[gpui::test]
fn files_are_written_in_every_format_and_the_sql_dialect_is_the_one_asked_for(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let folder = tempfile::tempdir().unwrap();
    let notes = oa_table(&rig, "notes");
    dt_open(&mut rig, &notes, "object-menu-export");
    // CSV, JSON, SQL INSERT (the select lists CSV, JSON, SQL INSERT, XLSX), then XLSX.
    for (steps, name) in [(0, "notes.csv"), (1, "notes.json"), (1, "notes.sql"), (1, "notes.xlsx")] {
        dt_next(&mut rig);
        assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "target");
        oa_type(rig.visual, "transfer-folder", &folder.path().display().to_string());
        if steps > 0 {
            di_choose(&mut rig, "transfer-format", steps);
        }
        if name.ends_with(".sql") {
            // The dialect of the file: MySQL quotes with backticks (the select lists This connection's, PostgreSQL, MySQL / MariaDB, SQLite, ClickHouse).
            di_choose(&mut rig, "transfer-sql-dialect", 2);
        }
        let shown = rig.app.read_with(rig.visual, |this, cx| this.transfers.values().next().unwrap().read(cx).file_name_for_test(cx));
        assert_eq!(shown, name, "the name follows the format until one is typed");
        dt_next(&mut rig);
        dt_next(&mut rig);
        let report = dt_run(&mut rig);
        assert_eq!(report.outcome, DtTransfer::Completed, "{name}");
        assert_eq!(dt_rows(&report), [(2, 2)]);
        assert!(folder.path().join(name).exists(), "{name}");
        rig.click("transfer-again");
    }
    assert_eq!(std::fs::read_to_string(folder.path().join("notes.csv")).unwrap(), "id,body\n1,one\n2,two\n");
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(folder.path().join("notes.json")).unwrap()).unwrap();
    assert_eq!(json, serde_json::json!([{"id": 1, "body": "one"}, {"id": 2, "body": "two"}]));
    assert_eq!(std::fs::read_to_string(folder.path().join("notes.sql")).unwrap(), "INSERT INTO `main`.`notes` (`id`, `body`) VALUES\n(1, 'one'),\n(2, 'two');\n");
    let workbook = std::fs::read(folder.path().join("notes.xlsx")).unwrap();
    assert!(workbook.starts_with(b"PK") && workbook.len() > 500, "a zip archive of a workbook");
    // A second export of the same table to the same name is refused unless replacing is asked for.
    dt_next(&mut rig);
    oa_type(rig.visual, "transfer-folder", &folder.path().display().to_string());
    dt_next(&mut rig);
    dt_next(&mut rig);
    rig.click("transfer-start");
    dt_wait(&mut rig, "the transfer to end", |s| s.report.is_some());
    let report = dt_snapshot(&rig.app, rig.visual).report.unwrap();
    let DtTransfer::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
    assert!(reason.contains("exists already"), "{reason}");
    assert_eq!(std::fs::read_to_string(folder.path().join("notes.csv")).unwrap(), "id,body\n1,one\n2,two\n", "the old file is untouched");
}

#[gpui::test]
fn a_schema_of_a_hundred_and_fifty_tables_keeps_the_lists_virtualized(cx: &mut TestAppContext) {
    let script: String = (0..150).map(|index| format!("CREATE TABLE t{index:03}(id INTEGER PRIMARY KEY, v TEXT);")).collect();
    let db = SqliteDb::new(&script);
    let mut rig = nav_rig_sqlite(cx, &db);
    rig.expand_to_tables("main", "main");
    let key = rig.key.clone();
    dt_open(&mut rig, &schema_node(&key, "main", "main"), "object-menu-export");
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.tables.len(), snapshot.shown), (150, 150));
    let drawn = (0..150).filter(|position| rig.visual.debug_bounds(Box::leak(format!("transfer-table-{position}").into_boxed_str())).is_some()).count();
    assert!(drawn > 5 && drawn < 40, "{drawn} rows of 150 are drawn");
    dt_next(&mut rig);
    dt_next(&mut rig);
    // The tables are read one after the other; the first 20 cover every row on screen and the selected one. Waiting for all 150
    // would time each read through this rig's polling, which a loaded machine stretches past the wait.
    dt_wait(&mut rig, "the tables on screen to be read", |s| s.jobs.len() == 150 && s.jobs.iter().take(20).all(|job| job.columns.is_some()));
    let drawn = (0..150).filter(|index| rig.visual.debug_bounds(Box::leak(format!("transfer-job-{index}").into_boxed_str())).is_some()).count();
    assert!(drawn > 3 && drawn < 20, "{drawn} rows of 150 are drawn");
    assert_eq!(dt_snapshot(&rig.app, rig.visual).widgets, 0, "files have no mapping widgets");
}

#[gpui::test]
fn a_read_only_connection_can_still_be_exported(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let folder = tempfile::tempdir().unwrap();
    publish_types(&rig.app, rig.visual, true);
    give_profile(&rig.app, rig.visual, &key, typed(PRODUCTION));
    let notes = oa_table(&rig, "notes");
    dt_open(&mut rig, &notes, "object-menu-export");
    dt_next(&mut rig);
    oa_type(rig.visual, "transfer-folder", &folder.path().display().to_string());
    // Enter in the name of the file goes on to the mapping.
    oa_type(rig.visual, "transfer-file-name", "read_only.csv");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "mapping");
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert_eq!(std::fs::read_to_string(folder.path().join("read_only.csv")).unwrap(), "id,body\n1,one\n2,two\n");
}

// ---- live servers (ignored: they need the dedicated tablex-test-* containers) --------------------

/// The text of the rows of `sql` as the grid shows them: columns joined by `|`, rows by a newline, NULL as `~`.
fn dt_select(session: &std::sync::Arc<engine_driver_host::SharedDriverSession>, sql: &str) -> String {
    let runner = session.query_runner().expect("a query runner");
    let request = serde_json::to_string(&engine_driver_api::QueryRequest {
        kind: "sql".into(),
        operation_id: runner.operation_id(),
        database: None,
        object: None,
        offset: 0,
        limit: 1000,
        payload: serde_json::json!({ "sql": sql, "confirmed": true }),
    })
    .unwrap();
    let page = runner.open_query(&request).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let text = {
        let mut guard = session.lock().expect("the session");
        guard.adopt_page(&runner, page.page_id).expect("the page");
        let lease = guard.acquire_arrow_page(page.page_id, 0, page.row_count).expect("the Arrow page");
        // SAFETY: the lease holds the Arrow C Data pair the driver exported for this page; import consumes it once.
        let imported = unsafe { lease.into_record_batch() }.expect("the Arrow batch");
        let readers: Vec<crate::grid_arrow::ArrowCellReader<'_>> = imported.batch.columns().iter().map(|array| crate::grid_arrow::ArrowCellReader::new(array.as_ref(), crate::grid_arrow::BinaryDisplay::Hex)).collect();
        let mut buffer = String::new();
        let text = (0..imported.batch.num_rows())
            .map(|row| readers.iter().map(|reader| if reader.is_null(row) { "~".to_owned() } else { reader.text(row, &mut buffer).to_owned() }).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = guard.release_page(page.page_id);
        text
    };
    text
}

/// A second connection beside the rig's: the target of a transfer between servers.
fn dt_add_connection(rig: &mut SqlRig<'_>, kind: ConnectionKind, (package, features): (&'static str, &'static str), uri: &str, name: &str) -> (String, std::sync::Arc<engine_driver_host::SharedDriverSession>) {
    let dylib = built_driver(package, features);
    let uri = uri.to_owned();
    let name = name.to_owned();
    let (key, session) = rig.app.update(rig.visual, |this, cx| {
        let id = uuid::Uuid::new_v4();
        this.saved_connections.push(SavedConnection { id, name: name.clone(), uri: uri.clone(), kind, save_password: false, is_pinned: false, disabled: false, ssh_tunnel: false });
        let key = id.to_string();
        let handle = engine_driver_host::DriverLoader.load_verified(&dylib, &driver_sha256(&dylib)).expect("the driver loads");
        let session = handle.connect(&serde_json::json!({ "uri": uri }).to_string()).expect("the driver connects");
        let engine_id = session.connection_id();
        if let Some(version) = session.server_version() {
            this.driver_runtime.server_versions.insert(key.clone(), version);
        }
        let session = std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session));
        this.driver_runtime.sessions.insert(key.clone(), session.clone());
        this.driver_runtime.connection_ids.insert(key.clone(), engine_id);
        this.driver_runtime.capabilities.insert(key.clone(), std::sync::Arc::new(handle.capabilities().clone()));
        this.driver_ui.handles.insert(kind.driver_id().into(), handle.clone());
        this.sync_navigator_kinds(&key);
        this.driver_runtime.session_handles.insert(key.clone(), handle.clone());
        let epoch = this.sessions.begin_connect(&key).unwrap();
        this.sessions.finish_connect(&key, epoch, Ok(()));
        this.navigator.set_connections(this.saved_connections.clone());
        this.sync_sidebar_connections(cx);
        cx.notify();
        (key, session)
    });
    settle(rig.visual);
    (key, session)
}

/// Both servers' tables are dropped when the test ends, pass or fail.
struct DtCleanup(Vec<(std::sync::Arc<engine_driver_host::SharedDriverSession>, String)>);

impl Drop for DtCleanup {
    fn drop(&mut self) {
        for (session, sql) in &self.0 {
            exec_confirmed(session, sql);
        }
    }
}

fn dt_suffix() -> String {
    uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned()
}

/// The place a connection's tables live in: PostgreSQL `public`, the others their database.
fn dt_namespace(kind: ConnectionKind, uri: &str) -> (String, String) {
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let schema = if kind == ConnectionKind::Postgres { "public".to_owned() } else { database.clone() };
    (database, schema)
}

/// Expands the navigator down to the Tables of the connection `key` (so the wizard can list its namespaces and tables).
fn dt_expand(rig: &mut SqlRig<'_>, key: &str, database: &str, schema: &str, postgres: bool) {
    rig.expand(&connection_node(key));
    rig.expand(&database_node(key, database));
    if postgres {
        rig.expand(&schema_node(key, database, schema));
    }
}

/// The wizard on the table `source` of the rig's connection, to `target` of the second connection.
fn dt_live_open(rig: &mut SqlRig<'_>, source: crate::app::structure_edit::apply::TableOrigin, to_file: bool) {
    let init = crate::app::data_transfer::TransferInit {
        source: crate::app::data_transfer::Namespace { connection: source.connection.clone(), database: source.database.clone(), schema: source.schema.clone() },
        tables: vec![source.object.clone()],
        all: false,
        to_file,
        query: None,
    };
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.open_data_transfer(init, window, cx)));
    settle(rig.visual);
    dt_wait(rig, "the list of tables", |s| s.listing == "ready" || s.listing == "failed");
}

/// The wizard on the query `sql`, of the namespace of `source`.
fn dt_live_open_query(rig: &mut SqlRig<'_>, source: &crate::app::structure_edit::apply::TableOrigin, sql: &str) {
    let init = crate::app::data_transfer::TransferInit {
        source: crate::app::data_transfer::Namespace { connection: source.connection.clone(), database: source.database.clone(), schema: source.schema.clone() },
        tables: Vec::new(),
        all: false,
        to_file: false,
        query: Some(sql.to_owned()),
    };
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.open_data_transfer(init, window, cx)));
    settle(rig.visual);
}

/// Runs a statement on `session`; whether the server took it.
fn dt_try_exec(session: &std::sync::Arc<engine_driver_host::SharedDriverSession>, sql: &str) -> bool {
    let Ok(runner) = session.query_runner() else { return false };
    let Ok(request) = serde_json::to_string(&engine_driver_api::QueryRequest {
        kind: "sql".into(),
        operation_id: runner.operation_id(),
        database: None,
        object: None,
        offset: 0,
        limit: 0,
        payload: serde_json::json!({ "sql": sql, "confirmed": true }),
    }) else {
        return false;
    };
    match runner.open_query(&request) {
        Ok(page) => {
            runner.release_page(page.page_id);
            true
        }
        Err(_) => false,
    }
}

/// Brings the transfer tab back (the checks ran SQL in the rig's own tab).
fn dt_front(rig: &mut SqlRig<'_>) {
    let tab = rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().iter().find(|tab| tab.kind == TabKind::Transfer).expect("a transfer tab").id);
    rig.app.update(rig.visual, |this, cx| this.activate_tab_ui(tab, cx));
    settle(rig.visual);
}

/// Sets the target connection select to `row` and the namespace to `namespace`.
fn dt_choose_target(rig: &mut SqlRig<'_>, connection_row: i32, namespace: &str) {
    dt_next(rig);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "target");
    di_choose(rig, "transfer-target-connection", connection_row);
    let wanted = namespace.to_owned();
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    let position = snapshot.namespaces.iter().position(|label| *label == wanted).unwrap_or_else(|| panic!("{wanted} is one of the namespaces {:?}", snapshot.namespaces));
    let current = snapshot.namespaces.iter().position(|label| *label == snapshot.target.label_for_test()).unwrap_or(0);
    if position != current {
        di_choose(rig, "transfer-target-namespace", position as i32 - current as i32);
    }
    assert_eq!(dt_snapshot(&rig.app, rig.visual).target.label_for_test(), wanted);
}

const DT_PG: (ConnectionKind, (&str, &str), &str) = (ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
const DT_MYSQL: (ConnectionKind, (&str, &str), &str) = (ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
const DT_CLICKHOUSE: (ConnectionKind, (&str, &str), &str) = (ConnectionKind::Clickhouse, ("dbengine-clickhouse", ""), "TABLEX_TEST_CLICKHOUSE_URI");
const DT_MARIADB: (ConnectionKind, (&str, &str), &str) = (ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");

type DtEngine = (ConnectionKind, (&'static str, &'static str), &'static str);

/// The typed table every live transfer moves: numbers of each width, a decimal, floats, a flag, a date, timestamps, text with
/// quotes / unicode / a line break / the empty string, JSON, a UUID, bytes, and a row of NULLs.
fn dt_seed_sql(kind: ConnectionKind, table: &str) -> Vec<String> {
    match kind {
        ConnectionKind::Postgres => vec![
            format!("CREATE TABLE {table} (id bigint PRIMARY KEY, i2 smallint, i4 integer, i8 bigint, amt numeric(12,2), flt double precision, rl real, flag boolean, d date, ts timestamp, tstz timestamptz, txt text, vc varchar(20), js jsonb, uid uuid, bin bytea)"),
            format!(
                "INSERT INTO {table} VALUES \
                 (1, 1, 2, 3, 1234.50, 1.5, 2.25, true, '2024-02-29', '2024-02-29 13:14:15.123456', '2024-02-29 13:14:15+00', 'Zoë 😀 \"q\" ''s' || chr(10) || 'line2', 'abc', '{{\"a\": [1, 2], \"b\": null}}', '123e4567-e89b-12d3-a456-426614174000', '\\x00ff10'), \
                 (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
                 (3, -32768, -2147483648, -9223372036854775808, -0.05, -1e10, 0, false, '1999-12-31', '1999-12-31 23:59:59', '1999-12-31 23:59:59+00', '', '', '[]', '00000000-0000-0000-0000-000000000000', '\\x')"
            ),
        ],
        _ => vec![
            format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY, i2 SMALLINT, i4 INT, i8 BIGINT, amt DECIMAL(12,2), flt DOUBLE, rl FLOAT, flag TINYINT(1), d DATE, ts DATETIME(6), tstz TIMESTAMP NULL, txt TEXT, vc VARCHAR(20), js JSON, uid CHAR(36), bin BLOB) ENGINE=InnoDB"),
            format!(
                "INSERT INTO {table} VALUES \
                 (1, 1, 2, 3, 1234.50, 1.5, 2.25, 1, '2024-02-29', '2024-02-29 13:14:15.123456', '2024-02-29 13:14:15', CONCAT('Zoë 😀 \"q\" ''s', CHAR(10), 'line2'), 'abc', '{{\"a\": [1, 2], \"b\": null}}', '123e4567-e89b-12d3-a456-426614174000', X'00ff10'), \
                 (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
                 (3, -32768, -2147483648, -9223372036854775808, -0.05, -1e10, 0, 0, '1999-12-31', '1999-12-31 23:59:59', '1999-12-31 23:59:59', '', '', '[]', '00000000-0000-0000-0000-000000000000', X'')"
            ),
        ],
    }
}

/// What a live transfer is made of: how the source is seeded, what the target already holds (nothing: the table is created
/// through the review), and the statements run on the source's server before (and undone after) the transfer.
struct DtLivePlan {
    seed: fn(ConnectionKind, &str) -> Vec<String>,
    /// The target table's statements when it exists before the transfer (the transfer appends to it).
    existing_target: Option<fn(ConnectionKind, &str) -> Vec<String>>,
    /// Statements run first, on the source's session (or the target's: `true`), each with the one that undoes it when the test ends.
    around: Vec<(bool, String, String)>,
    /// The source is this query (it reads the seeded table) instead of the table.
    query: Option<fn(ConnectionKind, &str) -> String>,
    /// A MySQL or MariaDB target gets a database of its own for the test (dropped after it), so two tests never meet in one
    /// namespace; `tablex` itself when the server refuses to create it.
    own_database: bool,
}

impl DtLivePlan {
    fn typed() -> Self {
        Self { seed: dt_seed_sql, existing_target: None, around: Vec::new(), query: None, own_database: false }
    }
}

/// What a live transfer did, for the checks that run while its tables still exist.
struct DtLiveRun {
    report: DtReport,
    source: std::sync::Arc<engine_driver_host::SharedDriverSession>,
    target: std::sync::Arc<engine_driver_host::SharedDriverSession>,
    source_table: String,
    target_table: String,
    /// The mapping step's view of the (first) table: how it was read.
    job: crate::app::data_transfer::view::JobSnapshot,
}

/// `source` -> `target` through the wizard: the table is created on the target through the review (or filled when it exists),
/// then `inspect` looks at both servers. `None` when the servers are not given.
fn dt_live_run<T>(cx: &mut TestAppContext, source: DtEngine, target: DtEngine, plan: &DtLivePlan, inspect: impl FnOnce(&DtLiveRun) -> T) -> Option<T> {
    let (Ok(source_uri), Ok(target_uri)) = (std::env::var(source.2), std::env::var(target.2)) else {
        eprintln!("skipped: set {} and {} to the dedicated tablex-test-* containers", source.2, target.2);
        return None;
    };
    let suffix = dt_suffix();
    let (from_name, to_name) = (format!("cf_r4b_src_{suffix}"), format!("cf_r4b_dst_{suffix}"));
    let (source_database, source_schema) = dt_namespace(source.0, &source_uri);
    let (mut target_database, mut target_schema) = dt_namespace(target.0, &target_uri);
    let qualified = |schema: &str, name: &str| format!("{schema}.{name}");
    let mut rig = nav_rig(cx, source.0, source.1, &source_uri);
    let key = rig.key.clone();
    let (target_key, target_session) = dt_add_connection(&mut rig, target.0, target.1, &target_uri, "target");
    let mut drop_database = None;
    if plan.own_database && matches!(target.0, ConnectionKind::Mysql | ConnectionKind::Mariadb) {
        let own = format!("cf_r4b_db_{suffix}");
        if dt_try_exec(&target_session, &format!("CREATE DATABASE {own}")) {
            drop_database = Some((target_session.clone(), format!("DROP DATABASE IF EXISTS {own}")));
            (target_database, target_schema) = (own.clone(), own);
        }
    }
    let source_table = qualified(&source_schema, &from_name);
    let target_table = qualified(&target_schema, &to_name);
    let mut cleanup = vec![(rig.session.clone(), format!("DROP TABLE IF EXISTS {source_table}")), (target_session.clone(), format!("DROP TABLE IF EXISTS {target_table}"))];
    // Undone last of all, when the tables are gone.
    cleanup.extend(plan.around.iter().rev().map(|(on_target, _, undo)| (if *on_target { target_session.clone() } else { rig.session.clone() }, undo.clone())));
    // The database of the target goes after its table.
    cleanup.splice(2..2, drop_database);
    let _cleanup = DtCleanup(cleanup);
    for (on_target, apply, _) in &plan.around {
        exec_confirmed(if *on_target { &target_session } else { &rig.session }, apply);
    }
    for sql in (plan.seed)(source.0, &source_table) {
        exec_confirmed(&rig.session, &sql);
    }
    if let Some(existing) = plan.existing_target {
        for sql in existing(target.0, &target_table) {
            exec_confirmed(&target_session, &sql);
        }
    }
    dt_expand(&mut rig, &target_key, &target_database, &target_schema, target.0 == ConnectionKind::Postgres);
    let origin = crate::app::structure_edit::apply::TableOrigin { connection: key.clone(), database: source_database.clone(), schema: source_schema.clone(), object: from_name.clone() };
    match plan.query {
        None => dt_live_open(&mut rig, origin, false),
        Some(query) => {
            dt_live_open_query(&mut rig, &origin, &query(source.0, &source_table));
            // A source whose session is set to UTC says so where the query is typed.
            let pinned = crate::app::data_transfer::reader::pins_session_zone(source.0);
            assert_eq!(rig.visual.debug_bounds("transfer-query-utc").is_some(), pinned, "the note about the zone of a query, on the Source step");
        }
    }
    // The second connection is the second row of the select.
    let namespace = if target.0 == ConnectionKind::Postgres && target_database != target_schema { format!("{target_database}.{target_schema}") } else { target_database.clone() };
    dt_choose_target(&mut rig, 1, &namespace);
    dt_next(&mut rig);
    dt_wait(&mut rig, "the columns", |s| s.jobs.len() == 1 && s.jobs[0].columns.is_some());
    let wanted_action = if plan.existing_target.is_some() { "append" } else { "create" };
    dt_rename(&mut rig, 0, &to_name);
    dt_wait(&mut rig, "the table to be looked for", |s| s.jobs[0].exists.is_some() && s.jobs[0].target_name == to_name);
    let job = dt_snapshot(&rig.app, rig.visual).jobs.remove(0);
    assert_eq!((job.action, job.exists), (wanted_action, Some(plan.existing_target.is_some())), "{}", job.label);
    dt_next(&mut rig);
    if plan.query.is_some() {
        assert_eq!(rig.visual.debug_bounds("transfer-run-query-utc").is_some(), crate::app::data_transfer::reader::pins_session_zone(source.0), "the note about the zone of a query, on the Run step");
    }
    let report = dt_run(&mut rig);
    dt_front(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed, "{:?}", report.jobs);
    let run = DtLiveRun { report, source: rig.session.clone(), target: target_session.clone(), source_table, target_table, job };
    Some(inspect(&run))
}

/// The typed table: what the target holds, as `dt_select` shows it, and what the source holds.
fn dt_live_transfer(cx: &mut TestAppContext, source: DtEngine, target: DtEngine) -> Option<(String, String)> {
    dt_live_run(cx, source, target, &DtLivePlan::typed(), |run| {
        assert_eq!(dt_rows(&run.report), [(3, 3)]);
        let columns = "id, i2, i4, i8, amt, flt, rl, flag, d, ts, tstz, txt, vc, js, uid, bin";
        (dt_select(&run.target, &format!("SELECT {columns} FROM {} ORDER BY id", run.target_table)), dt_select(&run.source, &format!("SELECT {columns} FROM {} ORDER BY id", run.source_table)))
    })
}

/// What PostgreSQL holds of the typed table (the source seed, and what a transfer from MySQL or MariaDB makes of it).
const DT_PG_SOURCE: &str = "1|1|2|3|1234.50|1.5|2.25|true|2024-02-29|2024-02-29 13:14:15.123456|2024-02-29 13:14:15+00|Zoë 😀 \"q\" 's\nline2|abc|{\"a\": [1, 2], \"b\": null}|123e4567-e89b-12d3-a456-426614174000|\\x00ff10\n\
     2|~|~|~|~|~|~|~|~|~|~|~|~|~|~|~\n\
     3|-32768|-2147483648|-9223372036854775808|-0.05|-10000000000|0|false|1999-12-31|1999-12-31 23:59:59|1999-12-31 23:59:59+00|||[]|00000000-0000-0000-0000-000000000000|\\x";

/// The same rows in a MySQL / MariaDB table made by a transfer from PostgreSQL: a flag is a tinyint(1), a datetime(6) shows its
/// fraction, a timestamp with a zone arrives as the UTC wall clock, bytes show as `0x`.
const DT_MYSQL_TARGET: &str = "1|1|2|3|1234.50|1.5|2.25|1|2024-02-29|2024-02-29 13:14:15.123456|2024-02-29 13:14:15.000000|Zoë 😀 \"q\" 's\nline2|abc|{\"a\": [1, 2], \"b\": null}|123e4567-e89b-12d3-a456-426614174000|0x00ff10\n\
     2|~|~|~|~|~|~|~|~|~|~|~|~|~|~|~\n\
     3|-32768|-2147483648|-9223372036854775808|-0.05|-10000000000|0|0|1999-12-31|1999-12-31 23:59:59.000000|1999-12-31 23:59:59.000000|||[]|00000000-0000-0000-0000-000000000000|0x";

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI and TABLEX_TEST_MYSQL_URI (containers tablex-test-pg, tablex-test-mysql)"]
fn live_postgresql_to_mysql_keeps_every_typed_value(cx: &mut TestAppContext) {
    let Some((target, source)) = dt_live_transfer(cx, DT_PG, DT_MYSQL) else { return };
    assert_eq!(source, DT_PG_SOURCE);
    assert_eq!(target, DT_MYSQL_TARGET);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI and TABLEX_TEST_MARIADB_URI (containers tablex-test-pg, tablex-test-mariadb)"]
fn live_postgresql_to_mariadb_keeps_every_typed_value(cx: &mut TestAppContext) {
    let Some((target, source)) = dt_live_transfer(cx, DT_PG, DT_MARIADB) else { return };
    assert_eq!(source, DT_PG_SOURCE);
    assert_eq!(target, DT_MYSQL_TARGET);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI and TABLEX_TEST_PG_URI (containers tablex-test-mysql, tablex-test-pg)"]
fn live_mysql_to_postgresql_keeps_every_typed_value(cx: &mut TestAppContext) {
    let Some((target, source)) = dt_live_transfer(cx, DT_MYSQL, DT_PG) else { return };
    assert_eq!(
        source,
        "1|1|2|3|1234.50|1.5|2.25|1|2024-02-29|2024-02-29 13:14:15.123456|2024-02-29 13:14:15|Zoë 😀 \"q\" 's\nline2|abc|{\"a\": [1, 2], \"b\": null}|123e4567-e89b-12d3-a456-426614174000|0x00ff10\n\
         2|~|~|~|~|~|~|~|~|~|~|~|~|~|~|~\n\
         3|-32768|-2147483648|-9223372036854775808|-0.05|-10000000000|0|0|1999-12-31|1999-12-31 23:59:59.000000|1999-12-31 23:59:59|||[]|00000000-0000-0000-0000-000000000000|0x"
    );
    // The flag is a boolean, the fraction is kept, the bytes are a bytea, the JSON is a jsonb, a MySQL `timestamp` (an instant) is a timestamptz.
    assert_eq!(
        target,
        "1|1|2|3|1234.50|1.5|2.25|true|2024-02-29|2024-02-29 13:14:15.123456|2024-02-29 13:14:15+00|Zoë 😀 \"q\" 's\nline2|abc|{\"a\": [1, 2], \"b\": null}|123e4567-e89b-12d3-a456-426614174000|\\x00ff10\n\
         2|~|~|~|~|~|~|~|~|~|~|~|~|~|~|~\n\
         3|-32768|-2147483648|-9223372036854775808|-0.05|-10000000000|0|false|1999-12-31|1999-12-31 23:59:59|1999-12-31 23:59:59+00|||[]|00000000-0000-0000-0000-000000000000|\\x"
    );
}

// ---- live: the pages of a read follow each other on every engine --------------------------------------------

/// A row count and a checksum of the rows' columns (`cols` joined by `|`), the same way on both sides of a transfer.
fn dt_checksum(kind: ConnectionKind, table: &str, cols: &str) -> String {
    match kind {
        ConnectionKind::Postgres => format!("SELECT count(*), sum(hashtext(concat_ws('|', {cols})))::text FROM {table}"),
        _ => format!("SELECT count(*), CAST(sum(crc32(concat_ws('|', {cols}))) AS CHAR) FROM {table}"),
    }
}

/// The two tables of a live transfer hold the same rows (count and checksum, read on their own servers).
fn dt_same_rows(run: &DtLiveRun, source: ConnectionKind, target: ConnectionKind, cols: &str, rows: usize) {
    let on_source = dt_select(&run.source, &dt_checksum(source, &run.source_table, cols));
    let on_target = dt_select(&run.target, &dt_checksum(target, &run.target_table, cols));
    assert!(on_source.starts_with(&format!("{rows}|")), "{on_source}");
    assert_eq!(on_target, on_source);
}

/// 1..=6000 as `n` on MySQL and MariaDB (a recursive CTE stops at 1,000 rows there).
const DT_MYSQL_NUMBERS: &str = "(SELECT a0.d + 10 * a1.d + 100 * a2.d + 1000 * a3.d + 1 AS n FROM \
     (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) a0, \
     (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) a1, \
     (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) a2, \
     (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) a3) numbers";

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_a_postgresql_table_without_a_key_is_read_in_a_total_order_and_arrives_once(cx: &mut TestAppContext) {
    // 6,000 rows (three pages), 60 distinct: the ties a bare OFFSET would read twice or miss.
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(a int, b text, c numeric)"), format!("INSERT INTO {table} SELECT i % 10, 't' || (i % 4), i % 3 FROM generate_series(1, 6000) i")]
    }
    let plan = DtLivePlan { seed, existing_target: None, around: Vec::new(), query: None, own_database: false };
    let Some(()) = dt_live_run(cx, DT_PG, DT_PG, &plan, |run| {
        assert_eq!((run.job.order.clone(), run.job.keyset.clone()), (RowOrder::Total, None), "{}", run.job.sql);
        assert_eq!(dt_snapshots(&run.report), [SnapshotState::Held], "PostgreSQL reads every table in one repeatable-read transaction");
        assert_eq!(dt_rows(&run.report), [(6_000, 6_000)]);
        let groups = |table: &str, session| dt_select(session, &format!("SELECT a, b, c, count(*) FROM {table} GROUP BY 1, 2, 3 ORDER BY 1, 2, 3"));
        assert_eq!(groups(&run.target_table, &run.target), groups(&run.source_table, &run.source));
    }) else {
        return;
    };
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_a_postgresql_distinct_on_query_is_wrapped_and_arrives_once_page_after_page(cx: &mut TestAppContext) {
    // 6,000 rows in 3,000 groups of two: DISTINCT ON keeps the first of each by `v`; three pages of the wrapped query.
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table} AS SELECT i % 3000 AS g, md5(i::text) AS v FROM generate_series(1, 6000) i")]
    }
    fn query(_: ConnectionKind, table: &str) -> String {
        format!("SELECT DISTINCT ON (g) g, v FROM {table} ORDER BY g, v")
    }
    let plan = DtLivePlan { seed, existing_target: None, around: Vec::new(), query: Some(query), own_database: false };
    let Some(()) = dt_live_run(cx, DT_PG, DT_PG, &plan, |run| {
        assert_eq!(run.job.order, RowOrder::OwnLimited, "{}", run.job.sql);
        assert!(run.job.sql.starts_with("SELECT * FROM (\nSELECT DISTINCT ON (g)") && run.job.sql.contains("AS transfer_page ORDER BY \"g\", \"v\""), "{}", run.job.sql);
        assert_eq!(dt_rows(&run.report), [(3_000, 3_000)]);
        let kept = |table: &str| format!("SELECT count(*), count(DISTINCT g), sum(hashtext(g::text || v)) FROM {table}");
        let on_source = dt_select(&run.source, &format!("SELECT count(*), count(DISTINCT g), sum(hashtext(g::text || v)) FROM (SELECT DISTINCT ON (g) g, v FROM {} ORDER BY g, v) d", run.source_table));
        assert!(on_source.starts_with("3000|3000|"), "{on_source}");
        assert_eq!(dt_select(&run.target, &kept(&run.target_table)), on_source, "each group arrived once, with the row DISTINCT ON keeps");
    }) else {
        return;
    };
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg; a role that may CREATE EXTENSION citext and CREATE COLLATION)"]
fn live_a_postgresql_keyless_table_with_citext_and_a_non_deterministic_collation_arrives_once_page_after_page(cx: &mut TestAppContext) {
    // `A` and `a` are equal to citext, and to an ICU collation that is not deterministic: ordered by them, 6,000 rows tie in 4
    // groups of 1,500 and the pages (three) would repeat and miss rows. Ordered by `(col)::text COLLATE "C"` they do not.
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![
            format!("CREATE TABLE {table}(c citext, n text COLLATE cf_r4b_nondeterministic)"),
            format!("INSERT INTO {table} SELECT CASE WHEN i % 4 < 2 THEN 'A' ELSE 'a' END, CASE WHEN i % 2 = 0 THEN 'B' ELSE 'b' END FROM generate_series(1, 6000) i"),
        ]
    }
    let around = vec![
        (false, "CREATE EXTENSION IF NOT EXISTS citext".to_owned(), "SELECT 1".to_owned()),
        (false, "CREATE COLLATION IF NOT EXISTS cf_r4b_nondeterministic (provider = icu, locale = 'und-u-ks-level2', deterministic = false)".to_owned(), "DROP COLLATION IF EXISTS cf_r4b_nondeterministic".to_owned()),
    ];
    let plan = DtLivePlan { seed, existing_target: None, around, query: None, own_database: false };
    let Some(()) = dt_live_run(cx, DT_PG, DT_PG, &plan, |run| {
        assert_eq!((run.job.order.clone(), run.job.keyset.clone()), (RowOrder::Total, None), "{}", run.job.sql);
        assert!(run.job.sql.ends_with("ORDER BY (\"c\")::text COLLATE \"C\", (\"n\")::text COLLATE \"C\""), "{}", run.job.sql);
        assert_eq!(dt_rows(&run.report), [(6_000, 6_000)]);
        let groups = |table: &str, session| dt_select(session, &format!("SELECT (c)::text COLLATE \"C\", (n)::text COLLATE \"C\", count(*) FROM {table} GROUP BY 1, 2 ORDER BY 1, 2"));
        let on_source = groups(&run.source_table, &run.source);
        assert_eq!(on_source, "A|B|1500\nA|b|1500\na|B|1500\na|b|1500");
        assert_eq!(groups(&run.target_table, &run.target), on_source, "each of the four arrived 1,500 times");
    }) else {
        return;
    };
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_a_postgresql_composite_key_declared_b_then_a_and_a_timestamptz_key_are_paged_by_key(cx: &mut TestAppContext) {
    fn composite(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![
            format!("CREATE TABLE {table}(a text, b int, v text, PRIMARY KEY (b, a))"),
            format!("INSERT INTO {table} SELECT 'k' || (i % 7), i / 7, 'v' || i FROM generate_series(1, 6000) i"),
        ]
    }
    let plan = DtLivePlan { seed: composite, existing_target: None, around: Vec::new(), query: None, own_database: true };
    dt_live_run(cx, DT_PG, DT_PG, &plan, |run| {
        assert_eq!(run.job.keyset, Some(vec!["b".to_owned(), "a".to_owned()]), "the key is read in the order it is declared");
        assert_eq!(run.job.order, RowOrder::Key);
        dt_same_rows(run, ConnectionKind::Postgres, ConnectionKind::Postgres, "a, b, v", 6_000);
    });
    // A timestamptz key whose server runs at +07: the literal of each page's first key is an instant, not a wall clock.
    fn instants(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![
            format!("CREATE TABLE {table}(ts timestamptz PRIMARY KEY, v int)"),
            format!("INSERT INTO {table} SELECT timestamptz '2024-01-01 00:00:00+00' + i * interval '1 second' + i * interval '1 microsecond', i FROM generate_series(1, 5000) i"),
        ]
    }
    let plan = DtLivePlan {
        seed: instants,
        existing_target: None,
        around: vec![(false, "ALTER ROLE tablex SET timezone = 'Asia/Ho_Chi_Minh'".into(), "ALTER ROLE tablex RESET timezone".into()), (true, "SET TIME ZONE 'Asia/Ho_Chi_Minh'".into(), "RESET TIME ZONE".into())],
        query: None,
        own_database: false,
    };
    dt_live_run(cx, DT_PG, DT_PG, &plan, |run| {
        assert_eq!((run.job.keyset.clone(), run.job.order.clone()), (Some(vec!["ts".to_owned()]), RowOrder::Key));
        let sum = |table: &str, session| dt_select(session, &format!("SELECT count(*), sum(extract(epoch FROM ts)), max(v) FROM {table}"));
        assert_eq!(sum(&run.target_table, &run.target), sum(&run.source_table, &run.source));
        assert!(sum(&run.source_table, &run.source).starts_with("5000|"));
    });
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_composite_key_declared_b_then_a_is_paged_by_key(cx: &mut TestAppContext) {
    // Declared (b, a): the order of the key, not of the table's columns; the range is expanded for MySQL.
    fn composite(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![
            format!("CREATE TABLE {table}(a VARCHAR(20), b INT, v VARCHAR(20), PRIMARY KEY (b, a)) ENGINE=InnoDB"),
            format!("INSERT INTO {table} SELECT CONCAT('k', n % 7), n DIV 7, CONCAT('v', n) FROM {DT_MYSQL_NUMBERS} WHERE n <= 6000"),
        ]
    }
    let plan = DtLivePlan { seed: composite, existing_target: None, around: Vec::new(), query: None, own_database: true };
    dt_live_run(cx, DT_MYSQL, DT_MYSQL, &plan, |run| {
        assert_eq!((run.job.keyset.clone(), run.job.order.clone()), (Some(vec!["b".to_owned(), "a".to_owned()]), RowOrder::Key));
        assert!(run.job.sql.ends_with("ORDER BY `b`, `a`"), "{}", run.job.sql);
        dt_same_rows(run, ConnectionKind::Mysql, ConnectionKind::Mysql, "a, b, v", 6_000);
    });
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_enum_key_is_read_by_offset_in_key_order_and_arrives_once(cx: &mut TestAppContext) {
    // An enum orders by its position, a literal compares as text: not a key to page by, so the pages go by offset in key order.
    fn with_enum(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![
            format!("CREATE TABLE {table}(e ENUM('z','y','x') NOT NULL, n INT NOT NULL, PRIMARY KEY (e, n)) ENGINE=InnoDB"),
            format!("INSERT INTO {table} SELECT ELT(1 + n % 3, 'z', 'y', 'x'), n FROM {DT_MYSQL_NUMBERS} WHERE n <= 6000"),
        ]
    }
    let plan = DtLivePlan { seed: with_enum, existing_target: None, around: Vec::new(), query: None, own_database: true };
    dt_live_run(cx, DT_MYSQL, DT_MYSQL, &plan, |run| {
        assert_eq!((run.job.keyset.clone(), run.job.order.clone()), (None, RowOrder::UniqueKey), "{}", run.job.sql);
        dt_same_rows(run, ConnectionKind::Mysql, ConnectionKind::Mysql, "e, n", 6_000);
    });
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_text_key_under_a_case_insensitive_collation_arrives_once(cx: &mut TestAppContext) {
    // 'A9' sorts among the 'a's, and the literal compares the same way.
    fn ci(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![
            format!("CREATE TABLE {table}(k VARCHAR(30) PRIMARY KEY, v INT) ENGINE=InnoDB"),
            format!("INSERT INTO {table} SELECT CONCAT(IF(n % 2 = 0, 'A', 'b'), n), n FROM {DT_MYSQL_NUMBERS} WHERE n <= 6000"),
        ]
    }
    let plan = DtLivePlan { seed: ci, existing_target: None, around: Vec::new(), query: None, own_database: true };
    dt_live_run(cx, DT_MYSQL, DT_MYSQL, &plan, |run| {
        assert_eq!((run.job.keyset.clone(), run.job.order.clone()), (Some(vec!["k".to_owned()]), RowOrder::Key));
        assert_eq!(dt_snapshots(&run.report), [SnapshotState::Held], "MySQL reads every table in one consistent snapshot");
        dt_same_rows(run, ConnectionKind::Mysql, ConnectionKind::Mysql, "k, v", 6_000);
    });
}

/// The seconds since 1970 in each line `id|seconds` of `text`, to compare instants across servers.
fn dt_epochs(text: &str) -> Vec<(String, f64)> {
    text.lines()
        .map(|line| {
            let (id, seconds) = line.split_once('|').expect("id|seconds");
            (id.to_owned(), seconds.parse().unwrap_or_else(|_| panic!("a number of seconds: {line}")))
        })
        .collect()
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI and TABLEX_TEST_MYSQL_URI (containers tablex-test-pg, tablex-test-mysql)"]
fn live_an_instant_keeps_its_moment_between_timestamptz_and_timestamp_whatever_the_zone_of_the_server(cx: &mut TestAppContext) {
    // PostgreSQL's zone is +07 for every new session; MySQL cannot be set (no privilege), its zone is the system's.
    fn pg(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(id bigint PRIMARY KEY, tstz timestamptz)"), format!("INSERT INTO {table} VALUES (1, '2024-06-01 00:00:00+07'), (2, '2024-02-29 13:14:15.5+00'), (3, '2030-12-31 23:59:59-05'), (4, NULL)")]
    }
    fn mysql_target(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(id BIGINT PRIMARY KEY, tstz TIMESTAMP(6) NULL) ENGINE=InnoDB")]
    }
    let zone = (false, "ALTER ROLE tablex SET timezone = 'Asia/Ho_Chi_Minh'".to_owned(), "ALTER ROLE tablex RESET timezone".to_owned());
    let plan = DtLivePlan { seed: pg, existing_target: Some(mysql_target), around: vec![zone], query: None, own_database: false };
    dt_live_run(cx, DT_PG, DT_MYSQL, &plan, |run| {
        let source = dt_select(&run.source, &format!("SELECT id, extract(epoch FROM tstz) FROM {} WHERE tstz IS NOT NULL ORDER BY id", run.source_table));
        let target = dt_select(&run.target, &format!("SELECT id, UNIX_TIMESTAMP(tstz) FROM {} WHERE tstz IS NOT NULL ORDER BY id", run.target_table));
        assert_eq!(dt_epochs(&target), dt_epochs(&source), "{target} <> {source}");
        assert_eq!(dt_epochs(&source)[0], ("1".to_owned(), 1_717_174_800.0));
        assert_eq!(dt_select(&run.target, &format!("SELECT count(*) FROM {} WHERE tstz IS NULL", run.target_table)), "1");
    });
    // And back: a MySQL `timestamp` is an instant, a PostgreSQL `timestamptz` (new table) at a session of +07.
    fn mysql(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(id BIGINT PRIMARY KEY, t TIMESTAMP(6) NULL) ENGINE=InnoDB"), format!("INSERT INTO {table} VALUES (1, FROM_UNIXTIME(1717174800)), (2, FROM_UNIXTIME(1709212455.5)), (3, NULL)")]
    }
    let zone = (true, "ALTER ROLE tablex SET timezone = 'Asia/Ho_Chi_Minh'".to_owned(), "ALTER ROLE tablex RESET timezone".to_owned());
    let plan = DtLivePlan { seed: mysql, existing_target: None, around: vec![zone], query: None, own_database: false };
    dt_live_run(cx, DT_MYSQL, DT_PG, &plan, |run| {
        assert!(run.job.sql.contains("ORDER BY"), "{}", run.job.sql);
        let source = dt_select(&run.source, &format!("SELECT id, UNIX_TIMESTAMP(t) FROM {} WHERE t IS NOT NULL ORDER BY id", run.source_table));
        let target = dt_select(&run.target, &format!("SELECT id, extract(epoch FROM t) FROM {} WHERE t IS NOT NULL ORDER BY id", run.target_table));
        assert_eq!(dt_epochs(&target), dt_epochs(&source), "{target} <> {source}");
        assert_eq!(dt_select(&run.target, &format!("SELECT data_type FROM information_schema.columns WHERE table_name = '{}' AND column_name = 't'", run.target_table.rsplit('.').next().unwrap())), "timestamp with time zone");
    });
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI and TABLEX_TEST_PG_URI (start tablex-test-clickhouse first, stop it after)"]
fn live_a_clickhouse_table_with_a_tie_heavy_sorting_key_arrives_once_page_after_page(cx: &mut TestAppContext) {
    // A sorting key of 5 values over 6,000 rows: ordered by the key alone, pages would repeat and miss rows.
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(k UInt32, v UInt64) ENGINE = MergeTree ORDER BY k"), format!("INSERT INTO {table} SELECT number % 5, number FROM numbers(6000)")]
    }
    let plan = DtLivePlan { seed, existing_target: None, around: Vec::new(), query: None, own_database: false };
    dt_live_run(cx, DT_CLICKHOUSE, DT_PG, &plan, |run| {
        assert_eq!((run.job.keyset.clone(), run.job.order.clone()), (None, RowOrder::Total), "{}", run.job.sql);
        assert!(run.job.sql.ends_with("ORDER BY `k`, `v`"), "{}", run.job.sql);
        assert_eq!(dt_rows(&run.report), [(6_000, 6_000)]);
        assert!(matches!(dt_snapshots(&run.report).as_slice(), [SnapshotState::Unavailable(reason)] if reason.contains("ClickHouse")), "{:?}", dt_snapshots(&run.report));
        let on_target = dt_select(&run.target, &format!("SELECT count(*), count(DISTINCT v), sum(v), count(DISTINCT k) FROM {}", run.target_table));
        assert_eq!(on_target, "6000|6000|17997000|5");
    });
}

// ---- live: a query may only read, a refused table does not poison the next, text the collation calls equal -----

/// The wizard on each of `queries` (written for the seeded table `{t}`), to its Mapping: what each was refused with.
fn dt_live_refusals(cx: &mut TestAppContext, engine: DtEngine, seed: fn(ConnectionKind, &str) -> Vec<String>, queries: &[&str], unchanged: &[(&str, &str)]) -> Option<()> {
    dt_live_refusals_around(cx, engine, seed, queries, unchanged, &[])
}

/// `around`: statements run first on the source's session, each with the one that undoes it when the test ends; a server that
/// refuses the first skips the test (`None`).
fn dt_live_refusals_around(cx: &mut TestAppContext, engine: DtEngine, seed: fn(ConnectionKind, &str) -> Vec<String>, queries: &[&str], unchanged: &[(&str, &str)], around: &[(&str, &str)]) -> Option<()> {
    let Ok(uri) = std::env::var(engine.2) else {
        eprintln!("skipped: set {} to the dedicated tablex-test-* container", engine.2);
        return None;
    };
    let (database, schema) = dt_namespace(engine.0, &uri);
    let table = format!("cf_r4b_ro_{}", dt_suffix());
    let qualified = format!("{schema}.{table}");
    let mut rig = nav_rig(cx, engine.0, engine.1, &uri);
    let key = rig.key.clone();
    let mut cleanup: Vec<_> = [table.clone(), format!("{table}_c"), format!("{table}_into")].iter().map(|name| (rig.session.clone(), format!("DROP TABLE IF EXISTS {schema}.{name}"))).collect();
    for (apply, undo) in around {
        if !dt_try_exec(&rig.session, apply) {
            eprintln!("skipped: the server refused `{apply}` (a role with the privilege is needed)");
            return None;
        }
        cleanup.push((rig.session.clone(), (*undo).to_owned()));
    }
    let _cleanup = DtCleanup(cleanup);
    for sql in seed(engine.0, &qualified) {
        exec_confirmed(&rig.session, &sql);
    }
    let origin = crate::app::structure_edit::apply::TableOrigin { connection: key, database, schema: schema.clone(), object: table.clone() };
    for template in queries {
        let sql = template.replace("{t}", &qualified).replace("{n}", &table);
        dt_live_open_query(&mut rig, &origin, &sql);
        dt_next(&mut rig);
        rig.visual.update(|window, cx| {
            let view = rig.app.read(cx).transfers.values().next().cloned().unwrap();
            view.update(cx, |view, cx| view.choose_target_kind_for_test(false, window, cx));
        });
        settle(rig.visual);
        dt_next(&mut rig);
        dt_wait(&mut rig, "the columns of the query or why not", |s| s.jobs.len() == 1 && (s.jobs[0].columns.is_some() || s.jobs[0].columns_error.is_some()));
        let error = dt_snapshot(&rig.app, rig.visual).jobs[0].columns_error.clone();
        assert!(error.as_deref().is_some_and(|error| !error.is_empty()), "{sql} was refused");
        dt_discard(&mut rig);
        for (check, expected) in unchanged {
            assert_eq!(dt_select(&rig.session, &check.replace("{t}", &qualified)), *expected, "{sql}: nothing was written ({check})");
        }
        // Nothing was created either.
        let created = dt_select(&rig.session, &format!("SELECT count(*) FROM information_schema.tables WHERE table_name IN ('{table}_c', '{table}_into')"));
        assert_eq!(created, "0", "{sql}");
    }
    Some(())
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_refuses_a_script_that_ends_the_transaction_a_write_and_a_function_that_writes(cx: &mut TestAppContext) {
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(a serial)"), format!("INSERT INTO {table} VALUES (1)")]
    }
    dt_live_refusals(
        cx,
        DT_PG,
        seed,
        &[
            "SELECT 1 AS a; COMMIT; UPDATE {t} SET a = 99",
            "SELECT 1 AS a; UPDATE {t} SET a = 99",
            "UPDATE {t} SET a = 99 RETURNING a",
            "SELECT * INTO {n}_into FROM {t}",
            "WITH d AS (UPDATE {t} SET a = 99 RETURNING a) SELECT * FROM d",
            "SELECT 1 AS a -- note\n; COMMIT; UPDATE {t} SET a = 99",
            // A locking clause would hold the rows of the source locked for the whole transfer.
            "SELECT a FROM {t} FOR UPDATE",
            "SELECT a FROM {t} FOR NO KEY UPDATE",
            "SELECT a FROM {t} FOR SHARE",
            "SELECT a FROM {t} FOR KEY SHARE",
            // A function that writes: the driver runs the read inside a read-only scope, which refuses it.
            "SELECT nextval('{t}_a_seq') AS a",
        ],
        &[("SELECT a FROM {t}", "1"), ("SELECT last_value || '|' || is_called FROM {t}_a_seq", "1|false")],
    );
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_refuses_a_statement_that_commits_by_itself_a_script_and_a_write(cx: &mut TestAppContext) {
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(a INT) ENGINE=InnoDB"), format!("INSERT INTO {table} VALUES (1)")]
    }
    dt_live_refusals(
        cx,
        DT_MYSQL,
        seed,
        &[
            "CREATE TABLE {t}_c AS SELECT a FROM {t}",
            "SELECT 1 AS a; COMMIT; UPDATE {t} SET a = 99",
            "SELECT 1 AS a; UPDATE {t} SET a = 99",
            "UPDATE {t} SET a = 99",
            "SELECT a FROM {t} INTO OUTFILE '/tmp/cf_r4b_out'",
            "CREATE TABLE {t}_c LIKE {t}",
            "SELECT a FROM {t} FOR UPDATE",
            "SELECT a FROM {t} FOR SHARE",
            "SELECT a FROM {t} LOCK IN SHARE MODE",
        ],
        &[("SELECT a FROM {t}", "1")],
    );
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_a_table_that_fails_to_read_does_not_poison_the_tables_after_it(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
    let suffix = dt_suffix();
    let names: Vec<String> = (1..=4).map(|n| format!("cf_r4b_poison_{suffix}_{n}")).collect();
    let role = format!("cf_r4b_reader_{suffix}");
    // The tables and the role are made by the owner of the server; the transfer reads as the role, which may not read the second.
    let (admin, _cleanup) = {
        let rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
        let admin = rig.session.clone();
        let mut cleanup: Vec<(std::sync::Arc<engine_driver_host::SharedDriverSession>, String)> = names.iter().map(|name| (admin.clone(), format!("DROP TABLE IF EXISTS public.{name}"))).collect();
        cleanup.push((admin.clone(), format!("DROP OWNED BY {role}")));
        cleanup.push((admin.clone(), format!("DROP ROLE IF EXISTS {role}")));
        let guard = DtCleanup(cleanup);
        exec_confirmed(&admin, &format!("CREATE ROLE {role} LOGIN PASSWORD 'cf_r4b'"));
        for name in &names {
            exec_confirmed(&admin, &format!("CREATE TABLE public.{name}(id int PRIMARY KEY, v text)"));
            exec_confirmed(&admin, &format!("INSERT INTO public.{name} SELECT i, 'row ' || i FROM generate_series(1, 2500) i"));
            exec_confirmed(&admin, &format!("GRANT SELECT ON public.{name} TO {role}"));
        }
        (admin, guard)
    };
    let limited = uri.replacen("tablex:tablex@", &format!("{role}:cf_r4b@"), 1);
    assert_ne!(limited, uri, "the URI names the user tablex");
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &limited);
    let key = rig.key.clone();
    let folder = tempfile::tempdir().unwrap();
    let init = crate::app::data_transfer::TransferInit {
        source: crate::app::data_transfer::Namespace { connection: key, database: "tablex".into(), schema: "public".into() },
        tables: names.clone(),
        all: false,
        to_file: true,
        query: None,
    };
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.open_data_transfer(init, window, cx)));
    settle(rig.visual);
    dt_wait(&mut rig, "the list of tables", |s| s.listing == "ready");
    dt_next(&mut rig);
    let view = rig.app.read_with(rig.visual, |this, _| this.transfers.values().next().cloned().expect("the wizard"));
    let out = folder.path().to_path_buf();
    rig.visual.update(|window, cx| view.update(cx, |view, cx| view.set_files_for_test(&out.display().to_string(), crate::app::data_transfer::sink::FileFormat::Csv, window, cx)));
    dt_next(&mut rig);
    dt_wait(&mut rig, "the columns of every table", |s| s.jobs.len() == 4 && s.jobs.iter().all(|job| job.columns.is_some() || job.columns_error.is_some()));
    let described: Vec<_> = dt_snapshot(&rig.app, rig.visual).jobs.iter().map(|job| (job.columns.is_some(), job.columns_error.clone())).collect();
    assert!(described.iter().all(|(columns, _)| *columns), "every table is described: {described:?}");
    // The right to read the second is taken away under the wizard: the server answers "permission denied" to its first page.
    exec_confirmed(&admin, &format!("REVOKE SELECT ON public.{} FROM {role}", names[1]));
    dt_next(&mut rig);
    di_choose(&mut rig, "transfer-on-error", 1);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    let outcomes: Vec<DtOutcome> = report.jobs.iter().map(|job| job.as_ref().unwrap().report.outcome.clone()).collect();
    assert_eq!(outcomes[0], DtOutcome::Completed);
    assert!(matches!(&outcomes[1], DtOutcome::Failed(reason) if reason.contains("permission denied")), "{outcomes:?}");
    assert_eq!((&outcomes[2], &outcomes[3]), (&DtOutcome::Completed, &DtOutcome::Completed), "the tables after the refused one are read: {outcomes:?}");
    let mut files: Vec<String> = std::fs::read_dir(folder.path()).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
    files.sort();
    assert_eq!(files, [format!("{}.csv", names[0]), format!("{}.csv", names[2]), format!("{}.csv", names[3])]);
    assert_eq!(dt_snapshots(&report), vec![SnapshotState::Held; 4], "each table, the refused one too, had its own snapshot");
    for name in [&names[0], &names[2], &names[3]] {
        let text = std::fs::read_to_string(folder.path().join(format!("{name}.csv"))).unwrap();
        assert_eq!(text.lines().count(), 2_501, "{name}");
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_a_keyless_table_with_text_a_collation_calls_equal_arrives_once_page_after_page(cx: &mut TestAppContext) {
    // `a`, `A`, `á` and `a ` are one value to utf8mb4_general_ci (a PAD SPACE, accent-insensitive collation): 1,500 of each.
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![
            format!("CREATE TABLE {table}(s VARCHAR(10) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci) ENGINE=InnoDB"),
            format!("INSERT INTO {table} SELECT ELT(1 + n % 4, 'a', 'A', 'á', 'a ') FROM {DT_MYSQL_NUMBERS} WHERE n <= 6000"),
        ]
    }
    let plan = DtLivePlan { seed, existing_target: None, around: Vec::new(), query: None, own_database: true };
    dt_live_run(cx, DT_MYSQL, DT_MYSQL, &plan, |run| {
        assert_eq!((run.job.keyset.clone(), run.job.order.clone()), (None, RowOrder::Total), "{}", run.job.sql);
        assert!(run.job.sql.ends_with("ORDER BY CAST(`s` AS BINARY)"), "{}", run.job.sql);
        let counts = |table: &str, session| dt_select(session, &format!("SELECT HEX(s), count(*) FROM {table} GROUP BY HEX(s) ORDER BY HEX(s)"));
        let on_source = counts(&run.source_table, &run.source);
        assert_eq!(on_source.lines().count(), 4, "{on_source}");
        assert_eq!(counts(&run.target_table, &run.target), on_source, "each of the four arrived 1,500 times");
        assert!(run.target_table.starts_with("cf_r4b_db_") || run.target_table.starts_with("tablex."), "{}", run.target_table);
    });
}

/// A query that reads a `timestamp` and says what zone its session is in: the transfer sets the session to UTC for a query as it
/// does for a table, so the instant arrives as it is stored and the query says so (run it against a server whose zone is not
/// UTC, e.g. `SET GLOBAL time_zone = '+07:00'`, to see the shift this prevents: 7 hours).
fn dt_live_query_in_utc(cx: &mut TestAppContext, source: DtEngine, target: DtEngine) {
    fn seed(kind: ConnectionKind, table: &str) -> Vec<String> {
        match kind {
            ConnectionKind::Clickhouse => vec![format!("CREATE TABLE {table}(a UInt32, ts Nullable(DateTime)) ENGINE = MergeTree ORDER BY a"), format!("INSERT INTO {table} VALUES (1, toDateTime(1717174800)), (2, NULL)")],
            _ => vec![format!("CREATE TABLE {table}(a INT, ts TIMESTAMP NULL) ENGINE=InnoDB"), format!("INSERT INTO {table} VALUES (1, FROM_UNIXTIME(1717174800)), (2, NULL)")],
        }
    }
    fn query(kind: ConnectionKind, table: &str) -> String {
        let zone = if kind == ConnectionKind::Clickhouse { "timezone()" } else { "@@session.time_zone" };
        format!("SELECT a, ts, {zone} AS zone FROM {table}")
    }
    let (source_kind, target_kind) = (source.0, target.0);
    let plan = DtLivePlan { seed, existing_target: None, around: Vec::new(), query: Some(query), own_database: true };
    dt_live_run(cx, source, target, &plan, |run| {
        assert_eq!(run.job.label, "Query");
        let zone = if source_kind == ConnectionKind::Clickhouse { "UTC" } else { "+00:00" };
        assert_eq!(dt_select(&run.target, &format!("SELECT a, zone FROM {} ORDER BY a", run.target_table)), format!("1|{zone}\n2|{zone}"), "the query ran in a session set to UTC");
        let epochs = |kind, table: &str, session| {
            let seconds = match kind {
                ConnectionKind::Clickhouse => "toUnixTimestamp(ts)",
                ConnectionKind::Postgres => "extract(epoch FROM ts)",
                _ => "UNIX_TIMESTAMP(ts)",
            };
            dt_epochs(&dt_select(session, &format!("SELECT a, {seconds} FROM {table} WHERE ts IS NOT NULL ORDER BY a")))
        };
        let (on_source, on_target) = (epochs(source_kind, &run.source_table, &run.source), epochs(target_kind, &run.target_table, &run.target));
        assert_eq!(on_target, on_source, "{on_target:?} <> {on_source:?}");
        assert_eq!(on_source, [("1".to_owned(), 1_717_174_800.0)]);
        assert_eq!(dt_select(&run.target, &format!("SELECT count(*) FROM {} WHERE ts IS NULL", run.target_table)), "1");
    });
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_a_query_on_mysql_is_read_in_utc_like_a_table_whatever_the_zone_of_the_server(cx: &mut TestAppContext) {
    dt_live_query_in_utc(cx, DT_MYSQL, DT_MYSQL);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_a_query_on_mariadb_is_read_in_utc_like_a_table_whatever_the_zone_of_the_server(cx: &mut TestAppContext) {
    dt_live_query_in_utc(cx, DT_MARIADB, DT_MARIADB);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI and TABLEX_TEST_PG_URI (start tablex-test-clickhouse first, stop it after)"]
fn live_a_query_on_clickhouse_is_read_in_utc_like_a_table_whatever_the_zone_of_the_server(cx: &mut TestAppContext) {
    dt_live_query_in_utc(cx, DT_CLICKHOUSE, DT_PG);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI and TABLEX_TEST_MYSQL_URI (containers tablex-test-pg, tablex-test-mysql)"]
fn live_a_postgresql_query_keeps_the_moment_of_a_timestamptz_at_a_session_of_plus_seven(cx: &mut TestAppContext) {
    // PostgreSQL's session is not pinned: a `timestamptz` arrives with its offset, so the writer stores the same instant.
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(id bigint, ts timestamptz)"), format!("INSERT INTO {table} VALUES (1, '2024-06-01 00:00:00+07'), (2, '2030-12-31 23:59:59-05'), (3, NULL)")]
    }
    fn query(_: ConnectionKind, table: &str) -> String {
        format!("SELECT id, ts FROM {table}")
    }
    fn mysql_target(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(id BIGINT, ts TIMESTAMP(6) NULL) ENGINE=InnoDB")]
    }
    let zone = (false, "ALTER ROLE tablex SET timezone = 'Asia/Ho_Chi_Minh'".to_owned(), "ALTER ROLE tablex RESET timezone".to_owned());
    let plan = DtLivePlan { seed, existing_target: Some(mysql_target), around: vec![zone], query: Some(query), own_database: false };
    dt_live_run(cx, DT_PG, DT_MYSQL, &plan, |run| {
        let source = dt_select(&run.source, &format!("SELECT id, extract(epoch FROM ts) FROM {} WHERE ts IS NOT NULL ORDER BY id", run.source_table));
        let target = dt_select(&run.target, &format!("SELECT id, UNIX_TIMESTAMP(ts) FROM {} WHERE ts IS NOT NULL ORDER BY id", run.target_table));
        assert_eq!(dt_epochs(&target), dt_epochs(&source), "{target} <> {source}");
        assert_eq!(dt_epochs(&source)[0], ("1".to_owned(), 1_717_174_800.0));
    });
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_a_query_with_a_versioned_comment_is_transferred_when_the_server_version_is_known(cx: &mut TestAppContext) {
    // `/*!80000 … */` is code to this server: with the version known the text is one plain read, not an ambiguous one.
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(a INT) ENGINE=InnoDB"), format!("INSERT INTO {table} VALUES (1)")]
    }
    fn query(_: ConnectionKind, table: &str) -> String {
        format!("SELECT a /*!50000 , a + 1 AS b */ FROM {table}")
    }
    let plan = DtLivePlan { seed, existing_target: None, around: Vec::new(), query: Some(query), own_database: true };
    dt_live_run(cx, DT_MYSQL, DT_MYSQL, &plan, |run| {
        assert!(matches!(run.job.order, RowOrder::Unordered(_)), "{:?}", run.job.order);
        assert_eq!(dt_select(&run.target, &format!("SELECT a, b FROM {}", run.target_table)), "1|2");
    });
}

/// `/*!99999 … */` is a comment to every server there is: with the server's version known, the wizard reads the query as the one
/// it is, orders it, and the rows arrive (read as code the `WHERE` would leave none, and the text would be left unordered).
fn dt_live_query_with_a_versioned_comment_the_server_skips(cx: &mut TestAppContext, engine: DtEngine) {
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(a INT) ENGINE=InnoDB"), format!("INSERT INTO {table} VALUES (1), (2)")]
    }
    fn query(_: ConnectionKind, table: &str) -> String {
        format!("SELECT a FROM {table} /*!99999 WHERE a > 100 */")
    }
    let plan = DtLivePlan { seed, existing_target: None, around: Vec::new(), query: Some(query), own_database: true };
    dt_live_run(cx, engine, engine, &plan, |run| {
        assert!(!matches!(run.job.order, RowOrder::Unordered(_)), "{:?}", run.job.order);
        assert_eq!(dt_select(&run.target, &format!("SELECT group_concat(a ORDER BY a) FROM {}", run.target_table)), "1,2");
    });
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_a_query_with_a_versioned_comment_the_server_skips_is_ordered_and_transferred_whole(cx: &mut TestAppContext) {
    dt_live_query_with_a_versioned_comment_the_server_skips(cx, DT_MYSQL);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_a_query_with_a_versioned_comment_the_server_skips_is_ordered_and_transferred_whole(cx: &mut TestAppContext) {
    dt_live_query_with_a_versioned_comment_the_server_skips(cx, DT_MARIADB);
}

/// A query's result columns carry the driver's bare type labels (`char`, `decimal`, `enum`, `varchar`, …) with no length or
/// precision: the table made for them must still hold every value (no `CHAR(1)`, no `DECIMAL(10,0)`, no `ENUM` with no values).
fn dt_live_query_with_bare_types(cx: &mut TestAppContext, engine: DtEngine) {
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![
            format!("CREATE TABLE {table}(id INT, c CHAR(3), d DECIMAL(12,4), v VARCHAR(30), e ENUM('a','bb'), s SET('x','y'), b BINARY(4), w VARBINARY(8)) ENGINE=InnoDB"),
            format!("INSERT INTO {table} VALUES (1, 'abc', 1234.5678, 'hello world', 'bb', 'x,y', X'01020304', X'0a0b'), (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL)"),
        ]
    }
    fn query(_: ConnectionKind, table: &str) -> String {
        format!("SELECT id, c, d, v, e, s, b, w, CONCAT(c, '-', v) AS joined FROM {table}")
    }
    let plan = DtLivePlan { seed, existing_target: None, around: Vec::new(), query: Some(query), own_database: true };
    dt_live_run(cx, engine, engine, &plan, |run| {
        // A decimal with no precision is made decimal(65,30): the same number, with the scale of the column.
        let on_target = dt_select(&run.target, &format!("SELECT id, c, CAST(d AS DECIMAL(12,4)), v, e, s, b, w, joined FROM {} ORDER BY id", run.target_table));
        assert_eq!(on_target, dt_select(&run.source, &format!("SELECT id, c, d, v, e, s, b, w, CONCAT(c, '-', v) FROM {} ORDER BY id", run.source_table)), "the target holds what the source query returned");
        assert!(on_target.starts_with("1|abc|1234.5678|hello world|bb|x,y|"), "{on_target}");
    });
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_a_query_with_columns_the_driver_labels_without_a_length_creates_a_target_that_holds_them(cx: &mut TestAppContext) {
    dt_live_query_with_bare_types(cx, DT_MYSQL);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_a_query_with_columns_the_driver_labels_without_a_length_creates_a_target_that_holds_them(cx: &mut TestAppContext) {
    dt_live_query_with_bare_types(cx, DT_MARIADB);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI of a role that may SET GLOBAL sql_mode (skips itself otherwise)"]
fn live_mariadb_in_mssql_mode_refuses_a_script_hidden_behind_a_bracket_name(cx: &mut TestAppContext) {
    fn seed(_: ConnectionKind, table: &str) -> Vec<String> {
        vec![format!("CREATE TABLE {table}(a INT) ENGINE=InnoDB"), format!("INSERT INTO {table} VALUES (1)")]
    }
    // New sessions (the transfer's) get the mode; the driver reports it in `sql_dialect.bracket_identifiers`.
    let around = [("SET GLOBAL sql_mode = CONCAT(@@GLOBAL.sql_mode, ',MSSQL')", "SET GLOBAL sql_mode = DEFAULT")];
    dt_live_refusals_around(cx, DT_MARIADB, seed, &["SELECT [a'], 1; COMMIT; DELETE FROM {t} WHERE a>0; -- ']"], &[("SELECT a FROM {t}", "1")], &around);
}

// ---- performance (ignored): `TABLEX_PERF_ROWS` rows (default 1,000,000) x 8 columns ---------------

fn dt_perf_rows() -> usize {
    std::env::var("TABLEX_PERF_ROWS").ok().and_then(|rows| rows.parse().ok()).unwrap_or(1_000_000)
}

/// Where a perf transfer writes.
enum DtPerfTarget {
    Table { namespace: crate::app::data_transfer::Namespace, kind: ConnectionKind, name: String },
    File { folder: std::path::PathBuf, format: crate::app::data_transfer::sink::FileFormat },
}

/// Transfers `source` (a table of the rig's connection) to `target` through the wizard and prints rows/s and peak RSS.
fn dt_perf(rig: &mut SqlRig<'_>, label: &str, source: crate::app::structure_edit::apply::TableOrigin, target: DtPerfTarget, rows: usize, batch: usize, whole: bool) -> std::time::Duration {
    let init = crate::app::data_transfer::TransferInit {
        source: crate::app::data_transfer::Namespace { connection: source.connection.clone(), database: source.database.clone(), schema: source.schema.clone() },
        tables: vec![source.object.clone()],
        all: false,
        to_file: matches!(target, DtPerfTarget::File { .. }),
        query: None,
    };
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.open_data_transfer(init, window, cx)));
    settle(rig.visual);
    dt_wait(rig, "the list of tables", |s| s.listing == "ready" || s.listing == "failed");
    let view = rig.app.read_with(rig.visual, |this, _| this.transfers.values().next().cloned().expect("the wizard"));
    let app = rig.app.clone();
    let step = |rig: &mut SqlRig<'_>, view: &gpui::Entity<crate::app::data_transfer::view::TransferView>| rig.visual.update(|window, cx| view.update(cx, |view, cx| view.next(window, cx)));
    step(rig, &view);
    match &target {
        DtPerfTarget::Table { namespace, kind, .. } => rig.visual.update(|_, cx| view.update(cx, |view, cx| view.set_target_for_test(namespace.clone(), *kind, cx))),
        DtPerfTarget::File { folder, format } => rig.visual.update(|window, cx| view.update(cx, |view, cx| view.set_files_for_test(&folder.display().to_string(), *format, window, cx))),
    }
    step(rig, &view);
    wait_until(rig.visual, "the columns", |visual| dt_snapshot(&app, visual).jobs.first().is_some_and(|job| job.columns.is_some()));
    if let DtPerfTarget::Table { name, .. } = &target {
        let name = name.clone();
        rig.visual.update(|_, cx| view.update(cx, |view, cx| view.rename_job_for_test(0, &name, cx)));
        wait_until(rig.visual, "the target table", |visual| dt_snapshot(&app, visual).jobs[0].target_ready && dt_snapshot(&app, visual).jobs[0].action == "append");
        let job = &dt_snapshot(&app, rig.visual).jobs[0];
        assert!(job.assignments.iter().all(Option::is_some), "every column found its namesake: {:?}", job.assignments);
    }
    let baseline = perf_rss_kb();
    let (stop, peak) = (std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)), std::sync::Arc::new(std::sync::atomic::AtomicU64::new(baseline)));
    let sampler = {
        let (stop, peak) = (stop.clone(), peak.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                peak.fetch_max(perf_rss_kb(), std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let started = std::time::Instant::now();
    rig.visual.update(|window, cx| view.update(cx, |view, cx| view.start_for_test(batch, whole, window, cx)));
    let start = std::time::Instant::now();
    loop {
        rig.visual.run_until_parked();
        if dt_snapshot(&app, rig.visual).report.is_some() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(3600), "the transfer did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    let elapsed = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    sampler.join().unwrap();
    let report = dt_snapshot(&app, rig.visual).report.unwrap();
    assert_eq!(report.outcome, DtTransfer::Completed, "{:?}", report.jobs);
    assert_eq!(dt_rows(&report), [(rows as u64, rows as u64)]);
    let peak = peak.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "PERF {label}: {rows} rows x 8 columns, batch {batch}{}: {:.1} s, {:.0} rows/s; RSS {:.0} MB before, peak {:.0} MB (+{:.0} MB)",
        if whole { ", one transaction" } else { "" },
        elapsed.as_secs_f64(),
        rows as f64 / elapsed.as_secs_f64(),
        baseline as f64 / 1024.0,
        peak as f64 / 1024.0,
        (peak - baseline) as f64 / 1024.0
    );
    let tab = rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().iter().find(|tab| tab.kind == TabKind::Transfer).map(|tab| tab.id));
    if let Some(tab) = tab {
        rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    }
    elapsed
}

/// The scratch table every perf run reads: `rows` rows of 8 typed columns, made on the server itself.
fn dt_perf_seed(kind: ConnectionKind, table: &str, rows: usize) -> Vec<String> {
    match kind {
        ConnectionKind::Postgres => vec![
            format!("CREATE TABLE {table}(id bigint PRIMARY KEY, name text, email text, qty int, price numeric(12,2), flag boolean, day date, note text)"),
            format!(
                "INSERT INTO {table} SELECT i, 'name ' || i, 'user' || i || '@example.com', i % 1000, ((i % 5000) || '.' || lpad((i % 100)::text, 2, '0'))::numeric(12,2), i % 2 = 0, date '2024-01-01' + (i % 365), 'a note, with a comma and \"quotes\" for row ' || i FROM generate_series(1, {rows}) AS i"
            ),
        ],
        _ => vec![
            format!("CREATE TABLE {table}(id BIGINT PRIMARY KEY, name TEXT, email TEXT, qty INT, price DECIMAL(12,2), flag BOOLEAN, day DATE, note TEXT)"),
        ],
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI; prints rows/s and peak RSS (run in release)"]
fn perf_transfer_postgresql_to_postgresql_and_to_csv(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
    let rows = dt_perf_rows();
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let key = rig.key.clone();
    let suffix = dt_suffix();
    let (source, copy) = (format!("cf_r4b_perf_{suffix}"), format!("cf_r4b_perf_copy_{suffix}"));
    let _cleanup = DtCleanup(vec![(rig.session.clone(), format!("DROP TABLE IF EXISTS public.{source}")), (rig.session.clone(), format!("DROP TABLE IF EXISTS public.{copy}"))]);
    for sql in dt_perf_seed(ConnectionKind::Postgres, &format!("public.{source}"), rows) {
        exec_confirmed(&rig.session, &sql);
    }
    exec_confirmed(&rig.session, &format!("CREATE TABLE public.{copy}(id bigint PRIMARY KEY, name text, email text, qty int, price numeric(12,2), flag boolean, day date, note text)"));
    let origin = crate::app::structure_edit::apply::TableOrigin { connection: key.clone(), database: "tablex".into(), schema: "public".into(), object: source.clone() };
    let namespace = crate::app::data_transfer::Namespace { connection: key.clone(), database: "tablex".into(), schema: "public".into() };
    dt_perf(&mut rig, "PostgreSQL to PostgreSQL", origin.clone(), DtPerfTarget::Table { namespace, kind: ConnectionKind::Postgres, name: copy.clone() }, rows, 2000, false);
    assert_eq!(dt_select(&rig.session, &format!("SELECT count(*), sum(qty), max(id) FROM public.{copy}")), dt_select(&rig.session, &format!("SELECT count(*), sum(qty), max(id) FROM public.{source}")));
    let folder = tempfile::tempdir().unwrap();
    for format in [crate::app::data_transfer::sink::FileFormat::Csv, crate::app::data_transfer::sink::FileFormat::Json, crate::app::data_transfer::sink::FileFormat::SqlInsert] {
        dt_perf(&mut rig, &format!("PostgreSQL to {} file", format.label()), origin.clone(), DtPerfTarget::File { folder: folder.path().to_path_buf(), format }, rows, 2000, false);
        let file = folder.path().join(format!("{source}.{}", format.extension()));
        eprintln!("    {} is {:.1} MB", file.display(), std::fs::metadata(&file).unwrap().len() as f64 / 1e6);
        std::fs::remove_file(file).unwrap();
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI; prints rows/s and peak RSS (run in release)"]
fn perf_transfer_postgresql_table_without_a_key_to_csv(cx: &mut TestAppContext) {
    // No key: pages are `ORDER BY` all the columns with an OFFSET, the cost of a total order on a table the server cannot walk by index.
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
    let rows = dt_perf_rows();
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let key = rig.key.clone();
    let suffix = dt_suffix();
    let source = format!("cf_r4b_perf_nokey_{suffix}");
    let _cleanup = DtCleanup(vec![(rig.session.clone(), format!("DROP TABLE IF EXISTS public.{source}"))]);
    for sql in dt_perf_seed(ConnectionKind::Postgres, &format!("public.{source}"), rows) {
        exec_confirmed(&rig.session, &sql);
    }
    exec_confirmed(&rig.session, &format!("ALTER TABLE public.{source} DROP CONSTRAINT {source}_pkey"));
    let origin = crate::app::structure_edit::apply::TableOrigin { connection: key.clone(), database: "tablex".into(), schema: "public".into(), object: source.clone() };
    let folder = tempfile::tempdir().unwrap();
    dt_perf(&mut rig, "PostgreSQL (no key) to CSV file", origin, DtPerfTarget::File { folder: folder.path().to_path_buf(), format: crate::app::data_transfer::sink::FileFormat::Csv }, rows, 2000, false);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI and TABLEX_TEST_MYSQL_URI; prints rows/s and peak RSS (run in release)"]
fn perf_transfer_postgresql_to_mysql(cx: &mut TestAppContext) {
    let (Ok(pg_uri), Ok(my_uri)) = (std::env::var("TABLEX_TEST_PG_URI"), std::env::var("TABLEX_TEST_MYSQL_URI")) else { return };
    let rows = dt_perf_rows();
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &pg_uri);
    let key = rig.key.clone();
    let (target_key, target_session) = dt_add_connection(&mut rig, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), &my_uri, "target");
    let suffix = dt_suffix();
    let (source, copy) = (format!("cf_r4b_perf_{suffix}"), format!("cf_r4b_perf_copy_{suffix}"));
    let _cleanup = DtCleanup(vec![(rig.session.clone(), format!("DROP TABLE IF EXISTS public.{source}")), (target_session.clone(), format!("DROP TABLE IF EXISTS tablex.{copy}"))]);
    for sql in dt_perf_seed(ConnectionKind::Postgres, &format!("public.{source}"), rows) {
        exec_confirmed(&rig.session, &sql);
    }
    exec_confirmed(&target_session, &format!("CREATE TABLE tablex.{copy}(id BIGINT PRIMARY KEY, name TEXT, email TEXT, qty INT, price DECIMAL(12,2), flag BOOLEAN, day DATE, note TEXT) ENGINE=InnoDB"));
    let origin = crate::app::structure_edit::apply::TableOrigin { connection: key, database: "tablex".into(), schema: "public".into(), object: source.clone() };
    let namespace = crate::app::data_transfer::Namespace { connection: target_key, database: "tablex".into(), schema: "tablex".into() };
    dt_perf(&mut rig, "PostgreSQL to MySQL", origin, DtPerfTarget::Table { namespace, kind: ConnectionKind::Mysql, name: copy.clone() }, rows, 2000, false);
    assert_eq!(dt_select(&target_session, &format!("SELECT count(*), sum(qty), max(id) FROM tablex.{copy}")), dt_select(&rig.session, &format!("SELECT count(*), sum(qty), max(id) FROM public.{source}")));
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI; prints rows/s and peak RSS (run in release)"]
fn perf_transfer_sqlite_to_postgresql(cx: &mut TestAppContext) {
    let Ok(pg_uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
    let rows = dt_perf_rows();
    let db = SqliteDb::new("CREATE TABLE perf(id INTEGER PRIMARY KEY, name TEXT, email TEXT, qty INTEGER, price NUMERIC, flag BOOLEAN, day DATE, note TEXT);");
    db.sql(&format!(
        "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < {rows}) INSERT INTO perf SELECT i, 'name ' || i, 'user' || i || '@example.com', i % 1000, (i % 5000) || '.' || substr('0' || (i % 100), -2), i % 2, date('2024-01-01', '+' || (i % 365) || ' days'), 'a note, with a comma and \"quotes\" for row ' || i FROM c"
    ));
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    let (target_key, target_session) = dt_add_connection(&mut rig, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &pg_uri, "target");
    let copy = format!("cf_r4b_perf_copy_{}", dt_suffix());
    let _cleanup = DtCleanup(vec![(target_session.clone(), format!("DROP TABLE IF EXISTS public.{copy}"))]);
    exec_confirmed(&target_session, &format!("CREATE TABLE public.{copy}(id bigint PRIMARY KEY, name text, email text, qty int, price numeric(12,2), flag boolean, day date, note text)"));
    let origin = crate::app::structure_edit::apply::TableOrigin { connection: key, database: "main".into(), schema: "main".into(), object: "perf".into() };
    let namespace = crate::app::data_transfer::Namespace { connection: target_key, database: "tablex".into(), schema: "public".into() };
    dt_perf(&mut rig, "SQLite to PostgreSQL", origin, DtPerfTarget::Table { namespace, kind: ConnectionKind::Postgres, name: copy.clone() }, rows, 2000, false);
    assert_eq!(dt_select(&target_session, &format!("SELECT count(*), sum(qty), max(id) FROM public.{copy}")), format!("{rows}|{}|{rows}", db.sql("SELECT sum(qty) FROM perf")));
}

#[gpui::test]
fn a_table_is_read_page_after_page_by_its_key_and_one_without_a_key_by_offset(cx: &mut TestAppContext) {
    // 6,000 rows (three pages) with a composite key of text and numbers whose text holds quotes; a table with no key; a table with a float key.
    let db = SqliteDb::new(
        "CREATE TABLE kv(a TEXT, b INTEGER, v TEXT, PRIMARY KEY(a, b)); \
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 6000) \
           INSERT INTO kv SELECT CASE i % 3 WHEN 0 THEN 'it''s ' || (i % 7) WHEN 1 THEN 'plain ' || (i % 11) ELSE 'ÉÀ ' || (i % 5) END, i, 'value ' || i FROM c; \
         CREATE TABLE loose(x INTEGER, y TEXT); \
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 3000) INSERT INTO loose SELECT i % 100, 'row ' || i FROM c; \
         CREATE TABLE floaty(f REAL PRIMARY KEY, y TEXT); INSERT INTO floaty VALUES (0.1, 'a'), (0.2, 'b'), (1e-7, 'c');",
    );
    let mut rig = nav_rig_sqlite(cx, &db);
    rig.expand_to_tables("main", "main");
    let key = rig.key.clone();
    dt_open(&mut rig, &schema_node(&key, "main", "main"), "object-menu-transfer");
    dt_to_mapping(&mut rig);
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    let keysets: Vec<(&str, Option<Vec<String>>)> = snapshot.jobs.iter().map(|job| (job.label.as_str(), job.keyset.clone())).collect();
    assert_eq!(keysets, [("floaty", None), ("kv", Some(vec!["a".to_owned(), "b".to_owned()])), ("loose", None)], "a float key and no key are read by offset");
    assert!(snapshot.jobs[1].sql.ends_with("ORDER BY \"a\", \"b\""), "{}", snapshot.jobs[1].sql);
    for (index, name) in ["floaty_copy", "kv_copy", "loose_copy"].into_iter().enumerate() {
        dt_rename(&mut rig, index, name);
    }
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert_eq!(dt_rows(&report), [(3, 3), (6_000, 6_000), (3_000, 3_000)]);
    // Every row once: the same sets both ways, and the same counts.
    for (table, columns) in [("floaty", "f, y"), ("kv", "a, b, v"), ("loose", "x, y")] {
        assert_eq!(db.sql(&format!("SELECT count(*) FROM (SELECT {columns} FROM {table} EXCEPT SELECT {columns} FROM {table}_copy)")), "0", "{table}");
        assert_eq!(db.sql(&format!("SELECT count(*) FROM {table}_copy")), db.sql(&format!("SELECT count(*) FROM {table}")), "{table}");
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI and TABLEX_TEST_CLICKHOUSE_URI (containers tablex-test-pg, tablex-test-clickhouse)"]
fn live_postgresql_to_clickhouse_keeps_every_typed_value(cx: &mut TestAppContext) {
    let Some((target, source)) = dt_live_transfer(cx, DT_PG, DT_CLICKHOUSE) else { return };
    assert_eq!(source, DT_PG_SOURCE);
    // A timestamp with a zone is a DateTime64(6, 'UTC'): the UTC wall clock; bytes travel as the text `\x...` of a String.
    assert_eq!(
        target,
        "1|1|2|3|1234.50|1.5|2.25|true|2024-02-29|2024-02-29 13:14:15.123456|2024-02-29 13:14:15|Zoë 😀 \"q\" 's\nline2|abc|{\"a\": [1, 2], \"b\": null}|123e4567-e89b-12d3-a456-426614174000|\\x00ff10\n\
         2|~|~|~|~|~|~|~|~|~|~|~|~|~|~|~\n\
         3|-32768|-2147483648|-9223372036854775808|-0.05|-10000000000|0|false|1999-12-31|1999-12-31 23:59:59|1999-12-31 23:59:59|||[]|00000000-0000-0000-0000-000000000000|\\x"
    );
}

#[gpui::test]
fn a_table_goes_to_another_connection_chosen_in_the_target_step(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let other = SqliteDb::new("CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL); INSERT INTO people VALUES (9, 'Old', 0.5);");
    let mut rig = oa_rig(cx, &db);
    let (other_key, _session) = dt_add_connection(&mut rig, ConnectionKind::Sqlite, ("dbengine-sqlite", ""), &other.uri(), "other file");
    // The navigator knows the other file once its tree is open: its schemas are what the target select lists.
    rig.expand(&connection_node(&other_key));
    rig.expand(&database_node(&other_key, "main"));
    rig.expand(&schema_node(&other_key, "main", "main"));
    let people = oa_table(&rig, "people");
    dt_open(&mut rig, &people, "object-menu-transfer");
    dt_next(&mut rig);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).connections, ["driver-test", "other file"]);
    di_choose(&mut rig, "transfer-target-connection", 1);
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.target.connection.as_str(), snapshot.namespaces.clone()), (other_key.as_str(), vec!["main".to_owned()]));
    dt_next(&mut rig);
    // The same name on another connection is not the same table: `people` exists there, with fewer columns; the rows go in by name.
    dt_wait(&mut rig, "both tables", |s| s.jobs.len() == 1 && s.jobs[0].columns.is_some() && s.jobs[0].target_ready);
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!((job.action, job.assignments[..3].to_vec()), ("append", vec![Some(0), Some(1), Some(2)]));
    assert_eq!(job.assignments.iter().flatten().count(), 3, "the columns the other table lacks are left out");
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert_eq!(dt_rows(&report), [(3, 3)]);
    assert_eq!(other.sql("SELECT id || ':' || name || ':' || ifnull(score, '~') FROM people ORDER BY id"), "1:Ann:1.5\n2:Zoë 😀 \"q\" 's:~\n3::3.25\n9:Old:0.5");
    assert_eq!(db.sql("SELECT count(*) FROM people"), "3", "the source is untouched");
}

#[gpui::test]
fn the_format_select_opens_chooses_dismisses_and_reopens_on_the_current_value(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let notes = oa_table(&rig, "notes");
    dt_open(&mut rig, &notes, "object-menu-export");
    dt_next(&mut rig);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).format, "CSV");
    // Open, move, Escape: dismissed, nothing chosen.
    rig.click("transfer-format");
    rig.visual.simulate_keystrokes("down down escape");
    settle(rig.visual);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).format, "CSV");
    // Open, move one, Enter: the model follows.
    rig.click("transfer-format");
    rig.visual.simulate_keystrokes("down enter");
    settle(rig.visual);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).format, "JSON");
    // Reopened, it stands on the current value: Enter chooses it again, and one step down is the next format.
    rig.click("transfer-format");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).format, "JSON");
    rig.click("transfer-format");
    rig.visual.simulate_keystrokes("down enter");
    settle(rig.visual);
    assert_eq!(dt_snapshot(&rig.app, rig.visual).format, "SQL INSERT");
    assert!(rig.visual.debug_bounds("transfer-sql-dialect").is_some(), "the dialect select belongs to SQL files only");
}

// ---- the order the pages are read in, the snapshot, and the rows left out of several tables -------------------

#[gpui::test]
fn tables_with_ties_or_a_key_that_cannot_be_compared_are_read_in_a_total_order_and_arrive_once(cx: &mut TestAppContext) {
    // `tie` has no key and heavy duplicates (35 distinct rows in 5,000); `untyped` has a primary key of no declared type, mixed
    // integers and text, which a literal cannot page by.
    let db = SqliteDb::new(
        "CREATE TABLE tie(x INTEGER, y TEXT, z TEXT); \
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 5000) INSERT INTO tie SELECT i % 7, 'r' || (i % 5), 'same' FROM c; \
         CREATE TABLE untyped(k PRIMARY KEY, v); \
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 4500) INSERT INTO untyped SELECT CASE i % 2 WHEN 0 THEN 'k' || i ELSE i END, 'v' || i FROM c;",
    );
    let mut rig = nav_rig_sqlite(cx, &db);
    rig.expand_to_tables("main", "main");
    let key = rig.key.clone();
    dt_open(&mut rig, &schema_node(&key, "main", "main"), "object-menu-transfer");
    dt_to_mapping(&mut rig);
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    let orders: Vec<_> = snapshot.jobs.iter().map(|job| (job.label.as_str(), job.keyset.is_some(), job.order.clone())).collect();
    assert_eq!(orders, [("tie", false, RowOrder::Total), ("untyped", false, RowOrder::UniqueKey)], "{orders:?}");
    assert!(snapshot.jobs[0].sql.ends_with("ORDER BY \"x\" COLLATE BINARY, \"y\" COLLATE BINARY, \"z\" COLLATE BINARY"), "{}", snapshot.jobs[0].sql);
    assert!(snapshot.jobs[1].sql.ends_with("ORDER BY \"k\""), "{}", snapshot.jobs[1].sql);
    dt_rename(&mut rig, 0, "tie_copy");
    dt_rename(&mut rig, 1, "untyped_copy");
    dt_next(&mut rig);
    assert!(rig.visual.debug_bounds("transfer-order-warning-0").is_none(), "a total order needs no warning");
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert_eq!(dt_rows(&report), [(5_000, 5_000), (4_500, 4_500)]);
    // The same rows the same number of times, both ways.
    assert_eq!(db.sql("SELECT count(*) FROM (SELECT x, y, z, count(*) FROM tie GROUP BY 1, 2, 3 EXCEPT SELECT x, y, z, count(*) FROM tie_copy GROUP BY 1, 2, 3)"), "0");
    assert_eq!(db.sql("SELECT count(*), count(DISTINCT k) FROM untyped_copy"), "4500|4500");
    assert_eq!(db.sql("SELECT count(*) FROM (SELECT CAST(k AS TEXT), v FROM untyped EXCEPT SELECT CAST(k AS TEXT), v FROM untyped_copy)"), "0");
}

#[gpui::test]
fn a_query_without_an_order_is_wrapped_in_one_and_may_not_read_the_table_it_writes(cx: &mut TestAppContext) {
    let db = SqliteDb::new("CREATE TABLE tie(x INTEGER, y TEXT); WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 4100) INSERT INTO tie SELECT i % 3, 'r' || (i % 2) FROM c;");
    let mut rig = oa_rig(cx, &db);
    dt_open_query(&mut rig, "SELECT y, x FROM tie");
    dt_next(&mut rig);
    rig.visual.update(|window, cx| {
        let view = rig.app.read(cx).transfers.values().next().cloned().unwrap();
        view.update(cx, |view, cx| view.choose_target_kind_for_test(false, window, cx));
    });
    settle(rig.visual);
    dt_next(&mut rig);
    dt_wait(&mut rig, "the columns of the query", |s| s.jobs.len() == 1 && s.jobs[0].columns.is_some());
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!(job.order, RowOrder::Total);
    assert!(job.sql.starts_with("SELECT * FROM (\nSELECT y, x FROM tie\n) AS transfer_page ORDER BY 1 COLLATE BINARY, 2 COLLATE BINARY"), "{}", job.sql);
    // Onto `tie`, the table the query reads, is refused with its own words; elsewhere it goes.
    oa_type(rig.visual, "transfer-job-name", "tie");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.click("transfer-next");
    assert_eq!(dt_snapshot(&rig.app, rig.visual).step, "mapping", "the query reads the table it would write to");
    assert!(rig.visual.debug_bounds("transfer-note-general-0").is_some());
    oa_type(rig.visual, "transfer-job-name", "swapped");
    rig.visual.simulate_keystrokes("enter");
    dt_wait(&mut rig, "the table to be looked for", |s| s.jobs[0].action == "create" && s.jobs[0].exists == Some(false));
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert_eq!(dt_rows(&report), [(4_100, 4_100)]);
    assert_eq!(db.sql("SELECT count(*) FROM (SELECT y, x, count(*) FROM tie GROUP BY 1, 2 EXCEPT SELECT y, x, count(*) FROM swapped GROUP BY 1, 2)"), "0");
}

#[gpui::test]
fn the_tables_are_read_as_one_snapshot_when_the_database_can(cx: &mut TestAppContext) {
    // A database in WAL mode lets a read transaction run beside its writers; one in the default mode would block them.
    let wal = SqliteDb::new(&format!("{DT_SEED} PRAGMA journal_mode=WAL;"));
    let mut rig = oa_rig(cx, &wal);
    let people = oa_table(&rig, "people");
    dt_open(&mut rig, &people, "object-menu-transfer");
    dt_to_mapping(&mut rig);
    dt_rename(&mut rig, 0, "people_copy");
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!((report.outcome.clone(), dt_snapshots(&report)), (DtTransfer::Completed, vec![SnapshotState::Held]));
    assert!(rig.visual.debug_bounds("transfer-snapshot").is_some() && rig.visual.debug_bounds("transfer-snapshot-table").is_some(), "said of the run and of the selected table");
    assert_eq!(wal.sql("SELECT count(*) FROM people_copy"), "3");
}

#[gpui::test]
fn a_database_that_cannot_hold_a_snapshot_says_so_in_the_summary(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let people = oa_table(&rig, "people");
    dt_open(&mut rig, &people, "object-menu-transfer");
    dt_to_mapping(&mut rig);
    dt_rename(&mut rig, 0, "people_copy");
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert!(matches!(dt_snapshots(&report).as_slice(), [SnapshotState::Unavailable(reason)] if reason.contains("WAL")), "{:?}", dt_snapshots(&report));
    assert!(rig.visual.debug_bounds("transfer-snapshot").is_some() && rig.visual.debug_bounds("transfer-snapshot-table").is_some());
    assert_eq!(db.sql("SELECT count(*) FROM people_copy"), "3", "not holding a snapshot costs the rows nothing");
}

#[gpui::test]
fn the_rows_left_out_of_every_table_are_listed_together_with_the_table_they_belong_to(cx: &mut TestAppContext) {
    let db = SqliteDb::new(
        "CREATE TABLE a1(id INTEGER PRIMARY KEY, name TEXT); INSERT INTO a1 VALUES (1,'x'),(2,''),(3,'y'); \
         CREATE TABLE a2(id INTEGER PRIMARY KEY, name TEXT); INSERT INTO a2 VALUES (1,''),(2,'z'),(3,''); \
         CREATE TABLE t1(id INTEGER PRIMARY KEY, name TEXT NOT NULL CHECK(length(name) > 0)); \
         CREATE TABLE t2(id INTEGER PRIMARY KEY, name TEXT NOT NULL CHECK(length(name) > 0));",
    );
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    click_node_part(&rig.app, rig.visual, &schema_node(&key, "main", "main"), "sidebar-row");
    rig.click("global-transfer");
    dt_wait(&mut rig, "the list of tables", |s| s.listing == "ready");
    rig.click("transfer-select-none");
    rig.click("transfer-table-0");
    rig.click("transfer-table-1");
    dt_to_mapping(&mut rig);
    dt_rename(&mut rig, 0, "t1");
    dt_rename(&mut rig, 1, "t2");
    dt_next(&mut rig);
    di_choose(&mut rig, "transfer-on-error", 1);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert_eq!(dt_rows(&report), [(3, 2), (3, 1)]);
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.error_lines.len(), 3, "{:?}", snapshot.error_lines);
    assert!(snapshot.error_lines[0].starts_with("a1 \u{00B7} Row 2"), "{:?}", snapshot.error_lines);
    assert!(snapshot.error_lines[1].starts_with("a2 \u{00B7} Row 1") && snapshot.error_lines[2].starts_with("a2 \u{00B7} Row 3"), "{:?}", snapshot.error_lines);
    // The list says which tables left rows out (both did), in the plural.
    assert_eq!(snapshot.errors_heading.as_deref(), Some("Rows left out of 2 tables (a1, a2):"));
    assert!(rig.visual.debug_bounds("transfer-errors-heading").is_some());
    for selector in ["transfer-errors", "transfer-error-0", "transfer-error-2"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
}

// ---- a query may only read; the pages of a table are read apart from each other -----------------------------

/// The wizard on the query `sql` (as the Export menu of a result opens it, but without running the query first).
fn dt_open_query_text(rig: &mut SqlRig<'_>, sql: &str) {
    let init = crate::app::data_transfer::TransferInit {
        source: crate::app::data_transfer::Namespace { connection: rig.key.clone(), database: String::new(), schema: String::new() },
        tables: Vec::new(),
        all: false,
        to_file: true,
        query: Some(sql.to_owned()),
    };
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.open_data_transfer(init, window, cx)));
    settle(rig.visual);
}

/// Closes the transfer tab from wherever it is.
fn dt_discard(rig: &mut SqlRig<'_>) {
    let tab = rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().iter().find(|tab| tab.kind == TabKind::Transfer).map(|tab| tab.id));
    if let Some(tab) = tab {
        rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    }
    settle(rig.visual);
    assert!(rig.app.read_with(rig.visual, |this, _| this.transfers.is_empty()));
}

/// From the Source step of a query to its Mapping (to a table), waiting for the columns of the query or why not.
fn dt_query_to_mapping(rig: &mut SqlRig<'_>) {
    dt_next(rig);
    rig.visual.update(|window, cx| {
        let view = rig.app.read(cx).transfers.values().next().cloned().unwrap();
        view.update(cx, |view, cx| view.choose_target_kind_for_test(false, window, cx));
    });
    settle(rig.visual);
    dt_next(rig);
    dt_wait(rig, "the columns of the query or why not", |s| s.jobs.len() == 1 && (s.jobs[0].columns.is_some() || s.jobs[0].columns_error.is_some()));
}

#[gpui::test]
fn a_query_that_could_write_is_refused_before_anything_is_sent_and_nothing_is_written(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let names = "SELECT group_concat(name) FROM (SELECT name FROM people ORDER BY id)";
    let before = db.sql(names);
    for sql in [
        "SELECT 1 AS a; UPDATE people SET name = 'changed'",
        "SELECT 1 AS a; COMMIT; UPDATE people SET name = 'changed'",
        "SELECT 1 AS a; -- ;\nDELETE FROM people",
        "UPDATE people SET name = 'changed' RETURNING id",
        "CREATE TABLE made AS SELECT 1 AS a",
        "WITH gone AS (DELETE FROM people RETURNING id) SELECT * FROM gone",
        "DROP TABLE notes",
        "SELECT id FROM people FOR UPDATE",
        "SELECT id FROM people FOR SHARE",
    ] {
        dt_open_query_text(&mut rig, sql);
        dt_query_to_mapping(&mut rig);
        let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
        let error = job.columns_error.clone().unwrap_or_else(|| panic!("{sql} was refused, got columns {:?}", job.columns));
        assert!(error.contains("Only a"), "{sql}: {error}");
        if sql.contains("FOR ") && !sql.contains("RETURNING") {
            assert!(error.contains("locking clause"), "{sql}: {error}");
        }
        assert_eq!(db.sql(names), before, "{sql}: nothing was written");
        assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name IN ('made') OR (name = 'notes' AND type = 'table') AND 0"), "0");
        dt_discard(&mut rig);
    }
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'made'"), "0");
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'notes'"), "1");
}

#[gpui::test]
fn a_query_with_a_comment_and_semicolons_after_it_is_transferred(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    dt_open_query_text(&mut rig, "SELECT id, name FROM people WHERE id < 3 -- the first two\n; ;\n-- the end");
    dt_query_to_mapping(&mut rig);
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!(job.columns_error, None);
    assert_eq!(job.order, RowOrder::Total);
    assert!(job.sql.ends_with("\n) AS transfer_page ORDER BY 1 COLLATE BINARY, 2 COLLATE BINARY"), "{}", job.sql);
    oa_type(rig.visual, "transfer-job-name", "firsts");
    rig.visual.simulate_keystrokes("enter");
    dt_wait(&mut rig, "the table to be looked for", |s| s.jobs[0].action == "create" && s.jobs[0].exists == Some(false));
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    assert_eq!(db.sql("SELECT group_concat(id) FROM firsts"), "1,2");
}

#[gpui::test]
fn a_versioned_comment_of_a_query_is_code_or_a_comment_by_the_version_of_the_source_server(cx: &mut TestAppContext) {
    // The connection is MySQL while the wizard opens (it takes the kind and the version of the session then) and SQLite again when
    // the query is run, so that the text is read by the database at hand: its `/*! … */` is a comment to SQLite.
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let set_kind = |rig: &mut SqlRig<'_>, kind: ConnectionKind| {
        rig.app.update(rig.visual, |this, _| this.saved_connections.iter_mut().filter(|saved| saved.matches_id(&key)).for_each(|saved| saved.kind = kind));
    };
    let sql = "SELECT id, name FROM people /*!50700 WHERE id > 0 */";
    // (the server's version, whether the comment is code to it)
    for (version, runs) in [(Some(50_600), false), (Some(50_700), true), (Some(80_036), true), (None, true)] {
        rig.app.update(rig.visual, |this, _| match version {
            Some(version) => drop(this.driver_runtime.server_versions.insert(key.clone(), version)),
            None => drop(this.driver_runtime.server_versions.remove(&key)),
        });
        set_kind(&mut rig, ConnectionKind::Mysql);
        dt_open_query_text(&mut rig, sql);
        set_kind(&mut rig, ConnectionKind::Sqlite);
        dt_next(&mut rig);
        dt_next(&mut rig);
        dt_wait(&mut rig, "the columns of the query or why not", |s| s.jobs.len() == 1 && (s.jobs[0].columns.is_some() || s.jobs[0].columns_error.is_some()));
        let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
        assert_eq!(job.columns_error, None, "{version:?}");
        if runs {
            // Code that cannot be cut around: read as it is, and said to be unsure.
            assert!(matches!(job.order, RowOrder::Unordered(reason) if reason.contains("versioned comment")), "{version:?}: {:?}", job.order);
            assert_eq!(job.sql, sql, "{version:?}");
        } else {
            assert!(matches!(job.order, RowOrder::Total | RowOrder::Partial(_)), "{version:?}: a comment to this server is paged in an order of its own: {:?}", job.order);
            // A comment after the last token is cut off, as one is: it is not code.
            assert!(job.sql.starts_with("SELECT * FROM (\nSELECT id, name FROM people\n) AS transfer_page ORDER BY "), "{version:?}: {}", job.sql);
        }
        dt_discard(&mut rig);
    }
}

#[gpui::test]
fn an_order_the_transfer_cannot_extend_a_limit_without_one_and_a_limit_after_one_are_warned_about(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    for (sql, expected) in [
        ("SELECT name FROM people ORDER BY score", "its own ORDER BY"),
        ("SELECT name FROM people LIMIT 2", "LIMIT and no ORDER BY"),
        // Wrapped and paged in a total order, but the LIMIT picks its rows by an order that may tie: said as well.
        ("SELECT id, name FROM people ORDER BY id LIMIT 2", "its LIMIT picks rows"),
    ] {
        dt_open_query_text(&mut rig, sql);
        dt_query_to_mapping(&mut rig);
        oa_type(rig.visual, "transfer-job-name", "elsewhere");
        rig.visual.simulate_keystrokes("enter");
        dt_wait(&mut rig, "the table to be looked for", |s| s.jobs[0].action == "create" && s.jobs[0].exists == Some(false));
        let order = dt_snapshot(&rig.app, rig.visual).jobs[0].order.clone();
        assert!(matches!(order, RowOrder::OwnUnmapped | RowOrder::Unordered(_) | RowOrder::OwnLimited), "{sql}: {order:?}");
        assert_eq!(order == RowOrder::OwnLimited, expected == "its LIMIT picks rows", "{sql}");
        dt_next(&mut rig);
        assert!(rig.visual.debug_bounds("transfer-order-warning-0").is_some(), "{sql}: said on the Run step");
        // SQLite's session is not set to a zone: the note about the zone of a query is not shown.
        assert!(rig.visual.debug_bounds("transfer-run-query-utc").is_none(), "{sql}");
        dt_discard(&mut rig);
    }
}

#[gpui::test]
fn typing_another_table_name_forgets_what_was_known_of_the_old_one(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let people = oa_table(&rig, "people");
    dt_open(&mut rig, &people, "object-menu-transfer");
    dt_to_mapping(&mut rig);
    dt_rename(&mut rig, 0, "notes");
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!((job.exists, job.target_ready), (Some(true), true), "notes exists and was described");
    // A character typed: the answer about `notes` says nothing of the name now in the box.
    oa_type(rig.visual, "transfer-job-name", "people_copy");
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!((job.exists, job.target_ready), (None, false), "not known of the new name until it is looked for");
    rig.visual.simulate_keystrokes("enter");
    dt_wait(&mut rig, "the table to be looked for", |s| s.jobs[0].exists == Some(false) && s.jobs[0].action == "create");
}

#[gpui::test]
fn a_table_with_text_that_a_collation_calls_equal_is_read_by_the_bytes_of_the_text(cx: &mut TestAppContext) {
    // `a` and `A` are equal to NOCASE: ordered by it the pages could mix the two. 3,000 of each and 1,000 `b`.
    let db = SqliteDb::new(
        "CREATE TABLE nc(s TEXT COLLATE NOCASE); \
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 7000) INSERT INTO nc SELECT CASE WHEN i % 7 = 0 THEN 'b' WHEN i % 2 = 0 THEN 'a' ELSE 'A' END FROM c;",
    );
    let mut rig = nav_rig_sqlite(cx, &db);
    rig.expand_to_tables("main", "main");
    let key = rig.key.clone();
    dt_open(&mut rig, &schema_node(&key, "main", "main"), "object-menu-transfer");
    dt_to_mapping(&mut rig);
    let job = &dt_snapshot(&rig.app, rig.visual).jobs[0];
    assert_eq!(job.order, RowOrder::Total);
    assert!(job.sql.ends_with("ORDER BY \"s\" COLLATE BINARY"), "{}", job.sql);
    dt_rename(&mut rig, 0, "nc_copy");
    dt_next(&mut rig);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    let counts = |table: &str| db.sql(&format!("SELECT hex(s) || ':' || count(*) FROM {table} GROUP BY hex(s) ORDER BY 1"));
    assert_eq!(counts("nc_copy"), counts("nc"));
}

#[gpui::test]
fn a_text_key_holding_bytes_that_are_not_text_does_not_derail_the_read_by_key(cx: &mut TestAppContext) {
    // 3,000 text keys and one that is not valid UTF-8 (it sorts last: on the second page).
    let db = SqliteDb::new(
        "CREATE TABLE bk(k TEXT PRIMARY KEY, v INTEGER); \
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 3000) INSERT INTO bk SELECT 'k' || printf('%05d', i), i FROM c; \
         INSERT INTO bk VALUES (CAST(x'c328' AS TEXT), 0);",
    );
    let mut rig = nav_rig_sqlite(cx, &db);
    rig.expand_to_tables("main", "main");
    let key = rig.key.clone();
    dt_open(&mut rig, &schema_node(&key, "main", "main"), "object-menu-transfer");
    dt_to_mapping(&mut rig);
    dt_rename(&mut rig, 0, "bk_copy");
    dt_next(&mut rig);
    di_choose(&mut rig, "transfer-on-error", 1);
    let report = dt_run(&mut rig);
    assert_eq!(report.outcome, DtTransfer::Completed);
    let result = report.jobs[0].as_ref().unwrap();
    assert_eq!(result.report.rows_read, 3_001, "{:?}", result.report);
    assert_eq!(result.report.inserted + result.report.skipped, 3_001);
    assert_eq!(db.sql("SELECT count(*), count(DISTINCT k) FROM bk_copy WHERE k LIKE 'k%'"), "3000|3000", "every readable key once");
}
