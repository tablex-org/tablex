// R4a: importing a file into a table, through the real navigator menu, the real file prompt (stubbed
// answer), the wizard tab, the shared review dialog (new tables) and the REAL SQLite driver on a second
// session. The database is read back with the `sqlite3` CLI.
//
// * The live tests at the end are `#[ignore]`d (dedicated `tablex-test-*` containers only).

use crate::app::data_import::run::Outcome as ImportOutcome;
use crate::app::data_import::view::Snapshot as ImportSnapshot;

const PEOPLE_SEED: &str = "CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, born DATE, active BOOLEAN); \
     INSERT INTO people VALUES (1,'Old',1.5,'2000-01-01',1);";

fn di_write(dir: &tempfile::TempDir, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn di_snapshot(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> ImportSnapshot {
    app.read_with(visual, |this, cx| this.imports.values().next().expect("an import tab is open").read(cx).snapshot())
}

fn di_wait(rig: &mut SqlRig<'_>, label: &'static str, ready: impl Fn(&ImportSnapshot) -> bool) {
    let app = rig.app.clone();
    wait_until(rig.visual, label, |visual| ready(&di_snapshot(&app, visual)));
    settle(rig.visual);
}

/// Waits for the file to be read (and the columns of the table to be known).
fn di_wait_sample(rig: &mut SqlRig<'_>) {
    di_wait(rig, "the file to be read", |snapshot| {
        !snapshot.reading && snapshot.targets_ready && (snapshot.rows > 0 || snapshot.sample_error.is_some() || snapshot.bad > 0)
    });
}

/// Import Data on the navigator row `id`, then the file prompt answered with `path`.
fn di_open(rig: &mut SqlRig<'_>, id: &NavigatorNodeId, path: &std::path::Path) {
    oa_menu(rig, id, "object-menu-import");
    assert!(rig.visual.did_prompt_for_paths(), "Import Data asks the platform for a file");
    let path = path.to_owned();
    rig.visual.simulate_path_prompt_response(move |options| {
        assert!(options.files && !options.multiple);
        Some(vec![path.clone()])
    });
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(rig.visual);
    di_wait_sample(rig);
}

/// Next, Next: the Load step.
fn di_to_load(rig: &mut SqlRig<'_>) {
    rig.wait_rendered("import-next");
    rig.click("import-next");
    rig.wait_rendered("import-next");
    rig.click("import-next");
    assert_eq!(di_snapshot(&rig.app, rig.visual).step, "load");
}

/// Types into the input drawn at `selector` (replacing what it holds).
fn di_type(rig: &mut SqlRig<'_>, selector: &'static str, text: &str) {
    oa_type(rig.visual, selector, text);
}

/// Opens the select drawn at `selector` and moves `steps` rows (positive: down) before choosing.
fn di_choose(rig: &mut SqlRig<'_>, selector: &'static str, steps: i32) {
    rig.click(selector);
    let key = if steps >= 0 { "down" } else { "up" };
    for _ in 0..steps.unsigned_abs() {
        rig.visual.simulate_keystrokes(key);
    }
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
}

/// Import, then wait for the report.
fn di_run(rig: &mut SqlRig<'_>) -> crate::app::data_import::run::Report {
    rig.click("import-start");
    di_wait(rig, "the import to end", |snapshot| snapshot.report.is_some());
    di_snapshot(&rig.app, rig.visual).report.unwrap()
}

fn di_import_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> u64 {
    app.read_with(visual, |this, _| this.workspace.tabs().iter().find(|tab| tab.kind == TabKind::Import).expect("an import tab").id)
}

fn di_grid_rows(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab: u64) -> usize {
    app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().grid.row_count())
}

#[gpui::test]
fn a_csv_goes_into_an_existing_table_by_the_wizard_and_the_open_tab_follows(cx: &mut TestAppContext) {
    let db = SqliteDb::new(PEOPLE_SEED);
    let mut rig = oa_rig(cx, &db);
    oa_open_table_tab(&mut rig, "people");
    let table_tab = rig.app.read_with(rig.visual, |this, _| this.workspace.active_tab().unwrap().id);
    let dir = tempfile::tempdir().unwrap();
    let path = di_write(&dir, "people.csv", "id,name,score,born,active\n2,Ann,3.5,1990-05-17,true\n3,\"Bob, Jr\",4,2001-02-03,false\n4,Cy,,,\n");
    let people = oa_table(&rig, "people");
    di_open(&mut rig, &people, &path);

    // The tab, the file and its preview in the app's own grid.
    let (kind, title) = rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        (tab.kind, tab.title.clone())
    });
    assert_eq!((kind, title.as_str()), (TabKind::Import, "Import into people"));
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.columns, ["id", "name", "score", "born", "active"]);
    assert_eq!((snapshot.rows, snapshot.step), (3, "source"));
    let import_tab = di_import_tab(&rig.app, rig.visual);
    assert_eq!(di_grid_rows(&rig.app, rig.visual, import_tab), 3, "the preview is the result grid");
    for selector in ["import-source-panel", "import-format", "import-encoding", "import-header", "import-null-text", "import-preview", "import-next"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }

    // Mapping: every column found its namesake; nothing to flag.
    rig.click("import-next");
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.step, snapshot.assignments.clone()), ("mapping", vec![Some(0), Some(1), Some(2), Some(3), Some(4)]));
    assert!(snapshot.notes.is_empty(), "{:?}", snapshot.notes);
    assert!(rig.visual.debug_bounds("import-map-4").is_some() && rig.visual.debug_bounds("import-map-type-1").is_some());

    // Load.
    rig.click("import-next");
    assert!(rig.visual.debug_bounds("import-batch").is_some() && rig.visual.debug_bounds("import-transaction").is_some());
    let report = di_run(&mut rig);
    assert_eq!(report.outcome, ImportOutcome::Completed);
    assert_eq!((report.inserted, report.skipped, report.rows_read), (3, 0, 3));
    assert_eq!(
        db.sql("SELECT id || '|' || name || '|' || ifnull(score, '~') || '|' || ifnull(born, '~') || '|' || ifnull(active, '~') FROM people ORDER BY id"),
        "1|Old|1.5|2000-01-01|1\n2|Ann|3.5|1990-05-17|1\n3|Bob, Jr|4.0|2001-02-03|0\n4|Cy|~|~|~"
    );
    for selector in ["import-summary", "import-headline", "import-stats", "import-again", "import-close"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // The tab that shows the table reloads on its own.
    let app = rig.app.clone();
    wait_until(rig.visual, "the open table tab to reload", |visual| di_grid_rows(&app, visual, table_tab) == 4);
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
    // Close ends the wizard.
    rig.click("import-close");
    assert!(rig.app.read_with(rig.visual, |this, _| this.imports.is_empty()), "the wizard is gone with its tab");
}

#[gpui::test]
fn the_options_change_what_is_read_and_the_preview_follows(cx: &mut TestAppContext) {
    let db = SqliteDb::new(PEOPLE_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = di_write(&dir, "export.txt", "exported 2024-03-01\nid\tname\n1\tAnn\n2\t\\N\n3\t  Cy  \n");
    let people = oa_table(&rig, "people");
    di_open(&mut rig, &people, &path);
    // Read as comma separated, the file is one column.
    assert_eq!(di_snapshot(&rig.app, rig.visual).columns, ["exported 2024-03-01"]);

    // Tab separated, the first line skipped: id and name, three rows.
    di_choose(&mut rig, "import-format", 1);
    di_type(&mut rig, "import-skip", "1");
    di_wait(&mut rig, "the columns of the skipped file", |s| s.columns == ["id", "name"] && s.rows == 3);

    // No header: the first record is a row and the columns are numbered.
    rig.click("import-header");
    di_wait(&mut rig, "the numbered columns", |s| s.columns == ["column1", "column2"] && s.rows == 4);
    rig.click("import-header");
    di_wait(&mut rig, "the header back", |s| s.columns == ["id", "name"] && s.rows == 3);
    // Enter in an option field goes on to the mapping.
    oa_type(rig.visual, "import-skip", "1");
    di_wait(&mut rig, "the same rows", |s| s.rows == 3 && !s.reading);
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    assert_eq!(di_snapshot(&rig.app, rig.visual).step, "mapping");

}

#[gpui::test]
fn encodings_and_custom_delimiters_are_chosen_in_the_options(cx: &mut TestAppContext) {
    let db = SqliteDb::new(PEOPLE_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let people = oa_table(&rig, "people");

    // Latin-1: not valid UTF-8 as it is, text once the encoding is chosen (select: Auto, UTF-8, Latin-1, ...).
    let latin = dir.path().join("latin.csv");
    std::fs::write(&latin, b"id,caf\xe9\n1,cr\xe8me\n").unwrap();
    di_open(&mut rig, &people, &latin);
    assert!(di_snapshot(&rig.app, rig.visual).sample_error.is_some() || di_snapshot(&rig.app, rig.visual).bad > 0);
    di_choose(&mut rig, "import-encoding", 2);
    di_wait(&mut rig, "the Latin-1 text", |s| s.columns == ["id", "café"] && s.rows == 1);
    let tab = di_import_tab(&rig.app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(rig.visual);

    // UTF-16 with a byte order mark is recognised on Auto.
    let utf16 = dir.path().join("wide.csv");
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend("id,name\n1,Zoë\n".encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&utf16, bytes).unwrap();
    di_open(&mut rig, &people, &utf16);
    assert_eq!(di_snapshot(&rig.app, rig.visual).columns, ["id", "name"]);
    let tab = di_import_tab(&rig.app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(rig.visual);

    // A custom delimiter and quote (select: CSV, TSV, Delimited).
    let custom = di_write(&dir, "pipes.txt", "id|name\n1|'a|b'\n");
    di_open(&mut rig, &people, &custom);
    di_choose(&mut rig, "import-format", 2);
    di_type(&mut rig, "import-delimiter", "|");
    di_type(&mut rig, "import-quote", "'");
    di_wait(&mut rig, "the pipe separated rows", |s| s.columns == ["id", "name"] && s.rows == 1);
    di_type(&mut rig, "import-delimiter", "||");
    di_wait(&mut rig, "the refusal of two characters", |s| s.sample_error.as_deref().is_some_and(|message| message.contains("single ASCII character")));
}

#[gpui::test]
fn the_null_text_and_trim_options_reach_the_table(cx: &mut TestAppContext) {
    let db = SqliteDb::new("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT);");
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = di_write(&dir, "notes.csv", "id,body\n1,\\N\n2,  padded  \n3,\n4,plain\n");
    let notes = oa_table(&rig, "notes");
    di_open(&mut rig, &notes, &path);
    di_type(&mut rig, "import-null-text", "\\N");
    rig.click("import-trim");
    rig.click("import-empty-null");
    di_wait(&mut rig, "the options to be read", |s| s.rows == 4 && !s.reading);
    di_to_load(&mut rig);
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted), (ImportOutcome::Completed, 4));
    assert_eq!(db.sql("SELECT id || ':' || ifnull(body, '~') FROM notes ORDER BY id"), "1:~\n2:padded\n3:~\n4:plain");
}

#[gpui::test]
fn json_arrays_and_ndjson_load_by_key_and_leave_out_what_the_table_lacks(cx: &mut TestAppContext) {
    let db = SqliteDb::new(PEOPLE_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let array = di_write(&dir, "people.json", r#"[{"id": 5, "name": "Eve", "score": 2.5, "born": null, "active": true, "extra": [1, 2]}, {"id": 6, "name": "Fay"}]"#);
    let people = oa_table(&rig, "people");
    di_open(&mut rig, &people, &array);
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.columns, ["id", "name", "score", "born", "active", "extra"], "the format came from the file name");
    rig.click("import-next");
    assert_eq!(di_snapshot(&rig.app, rig.visual).assignments, [Some(0), Some(1), Some(2), Some(3), Some(4), None], "extra has no namesake");
    rig.click("import-next");
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted), (ImportOutcome::Completed, 2));
    assert_eq!(db.sql("SELECT id || '|' || name || '|' || ifnull(score, '~') || '|' || ifnull(active, '~') FROM people WHERE id > 1 ORDER BY id"), "5|Eve|2.5|1\n6|Fay|~|~");

    // One object per line, through "Import another file".
    let lines = di_write(&dir, "more.ndjson", "{\"id\": 7, \"name\": \"Gus\"}\n{\"id\": 8, \"name\": \"Hal\", \"score\": 9}\n");
    rig.click("import-again");
    assert!(rig.visual.did_prompt_for_paths());
    let chosen = lines.clone();
    rig.visual.simulate_path_prompt_response(move |_| Some(vec![chosen.clone()]));
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    di_wait(&mut rig, "the second file", |s| s.columns == ["id", "name", "score"] && s.rows == 2 && s.step == "source");
    di_to_load(&mut rig);
    let report = di_run(&mut rig);
    assert_eq!(report.inserted, 2);
    assert_eq!(db.sql("SELECT count(*) FROM people"), "5");
}

#[gpui::test]
fn the_mapping_flags_what_the_columns_would_refuse_and_a_target_takes_one_source(cx: &mut TestAppContext) {
    let db = SqliteDb::new(PEOPLE_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = di_write(&dir, "p.csv", "id,label,score\nx,Ann,abc\n2,Bob,1.5\n");
    let people = oa_table(&rig, "people");
    di_open(&mut rig, &people, &path);
    rig.click("import-next");
    let snapshot = di_snapshot(&rig.app, rig.visual);
    // `label` has no namesake: skipped; id and score are flagged from the preview; `name` is NOT NULL and unmapped.
    assert_eq!(snapshot.assignments, [Some(0), None, Some(2)]);
    assert!(snapshot.notes.iter().any(|note| note.contains("is not a whole number") && note.contains("\"x\"")), "{:?}", snapshot.notes);
    assert!(snapshot.notes.iter().any(|note| note.contains("is not a number") && note.contains("\"abc\"")), "{:?}", snapshot.notes);
    assert!(snapshot.notes.iter().any(|note| note.contains("\"name\" does not accept NULL")), "{:?}", snapshot.notes);
    for selector in ["import-map-note-0", "import-map-note-2", "import-note-general-0"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // `label` goes to `name` (the select lists (skip), id, name, ...): the notes follow.
    di_choose(&mut rig, "import-map-1", 2);
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.assignments, [Some(0), Some(1), Some(2)]);
    assert!(!snapshot.notes.iter().any(|note| note.contains("does not accept NULL")), "{:?}", snapshot.notes);
    // Another column taking `name` skips the first one.
    di_choose(&mut rig, "import-map-2", -1);
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.assignments, [Some(0), None, Some(1)]);
    // Next needs a mapped column: skip them all.
    di_choose(&mut rig, "import-map-0", -1);
    di_choose(&mut rig, "import-map-2", -2);
    assert!(di_snapshot(&rig.app, rig.visual).assignments.iter().all(Option::is_none));
    rig.click("import-next");
    assert_eq!(di_snapshot(&rig.app, rig.visual).step, "mapping", "nothing mapped, nowhere to go");
}

#[gpui::test]
fn a_new_table_is_created_through_the_review_and_then_loaded(cx: &mut TestAppContext) {
    let db = SqliteDb::new(PEOPLE_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let dir = tempfile::tempdir().unwrap();
    let path = di_write(&dir, "readings.csv", "id,label,price,seen\n1,a,1.5,2024-01-02\n2,b,2.25,2024-02-03\n3,,3,2024-03-04\n");
    let tables = group_node(&key, "main", "main", ObjectGroup::Tables);
    di_open(&mut rig, &tables, &path);
    rig.click("import-next");
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert_eq!(
        snapshot.new_columns,
        [(true, "id".into(), "INTEGER".into()), (true, "label".into(), "TEXT".into()), (true, "price".into(), "REAL".into()), (true, "seen".into(), "DATE".into())]
    );
    // Name it, leave a column out, rename another, retype a third.
    di_type(&mut rig, "import-new-name", "readings");
    rig.click("import-include-1");
    di_type(&mut rig, "import-new-column-3", "seen on");
    di_type(&mut rig, "import-new-type-2", "NUMERIC");
    rig.click("import-next");
    assert_eq!(di_snapshot(&rig.app, rig.visual).step, "load");
    // The table does not exist: nothing can be loaded yet, and nothing was created.
    assert!(rig.visual.debug_bounds("import-start").is_none());
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'readings'"), "0");

    // A bad table name is refused where it is typed.
    rig.click("import-back");
    di_type(&mut rig, "import-new-name", "items ");
    rig.click("import-next");
    rig.click("import-create");
    assert!(di_snapshot(&rig.app, rig.visual).message.is_some(), "a padded name is refused");
    assert!(rig.visual.debug_bounds("changes-subject").is_none(), "no review for it");
    rig.click("import-back");
    di_type(&mut rig, "import-new-name", "readings");
    rig.click("import-next");

    rig.click("import-create");
    assert!(rig.visual.debug_bounds("changes-subject").is_some(), "the review of the table is open");
    let script = oa_script(rig.visual);
    assert!(script.starts_with("CREATE TABLE \"main\".\"readings\" ("), "{script}");
    assert!(script.contains("\"id\" INTEGER") && script.contains("\"price\" NUMERIC") && script.contains("\"seen on\" DATE") && !script.contains("label"), "{script}");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    di_wait(&mut rig, "the wizard to hear the table exists", |s| s.created);
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'readings'"), "1");
    assert_eq!(db.sql("SELECT count(*) FROM readings"), "0", "nothing loaded yet");

    assert!(rig.visual.debug_bounds("import-start").is_some());
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted), (ImportOutcome::Completed, 3));
    assert_eq!(db.sql("SELECT id || '|' || price || '|' || \"seen on\" FROM readings ORDER BY id"), "1|1.5|2024-01-02\n2|2.25|2024-02-03\n3|3|2024-03-04");
    // The navigator lists it.
    let (app, group) = (rig.app.clone(), tables.clone());
    let wanted = object_node(&key, "main", "main", "readings");
    wait_until(rig.visual, "the Tables group to list the new table", |visual| {
        app.read_with(visual, |this, _| this.navigator.loaded_children(&group).is_some_and(|children| children.contains(&wanted)))
    });
}

const NUMS_SEED: &str = "CREATE TABLE nums(id INTEGER PRIMARY KEY, qty INTEGER NOT NULL);";
const NUMS_BAD_ROW: &str = "id,qty\n1,10\n2,20\n3,30\n4,abc\n5,50\n6,60\n";

/// Opens `nums` with `text` and goes to the Load step with batches of `batch` rows.
fn di_nums(rig: &mut SqlRig<'_>, dir: &tempfile::TempDir, text: &str, batch: &str) {
    let path = di_write(dir, "nums.csv", text);
    let nums = oa_table(rig, "nums");
    di_open(rig, &nums, &path);
    di_to_load(rig);
    di_type(rig, "import-batch", batch);
}

#[gpui::test]
fn stop_keeps_the_committed_batches_and_one_transaction_rolls_everything_back(cx: &mut TestAppContext) {
    let db = SqliteDb::new(NUMS_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();

    // Commit every batch, stop at the first error: rows 1 and 2 (the first batch) stay.
    di_nums(&mut rig, &dir, NUMS_BAD_ROW, "2");
    let report = di_run(&mut rig);
    let ImportOutcome::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
    assert!(reason.contains("Row 4 (line 5)") && reason.contains("\"qty\" is not a whole number"), "{reason}");
    assert_eq!((report.inserted, report.rolled_back), (2, 0));
    assert_eq!(db.sql("SELECT group_concat(id) FROM nums"), "1,2");
    assert!(rig.visual.debug_bounds("import-kept").is_some(), "the summary says what stays");
    rig.click("import-close");
    db.sql("DELETE FROM nums");

    // One transaction: the same file keeps nothing.
    di_nums(&mut rig, &dir, NUMS_BAD_ROW, "2");
    di_choose(&mut rig, "import-transaction", 1);
    let report = di_run(&mut rig);
    assert!(matches!(report.outcome, ImportOutcome::Stopped(_)), "{:?}", report.outcome);
    assert_eq!((report.inserted, report.rolled_back), (0, 2));
    assert_eq!(db.sql("SELECT count(*) FROM nums"), "0", "rolled back");
    // The two stops say the same thing: one entry of the Error Log (a warning).
    assert_eq!(rig.navigator_errors(), [None]);
}

#[gpui::test]
fn skip_logs_the_rows_the_file_and_the_server_refuse_and_loads_the_rest(cx: &mut TestAppContext) {
    let db = SqliteDb::new(NUMS_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    // Row 4 is not a number; the second `2` breaks the primary key, which only the server can say.
    di_nums(&mut rig, &dir, "id,qty\n1,10\n2,20\n2,21\n3,30\n4,abc\n5,50\n6\n7,70\n", "3");
    di_choose(&mut rig, "import-on-error", 1);
    let report = di_run(&mut rig);
    assert_eq!(report.outcome, ImportOutcome::Completed);
    assert_eq!((report.inserted, report.skipped, report.rows_read), (5, 3, 8));
    assert_eq!(db.sql("SELECT group_concat(id) FROM nums ORDER BY id"), "1,2,3,5,7");
    let errors: Vec<(u64, String)> = report.errors.iter().map(|error| (error.row, error.message.clone())).collect();
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert_eq!(errors[0].0, 3);
    assert!(errors[0].1.contains("UNIQUE constraint failed"), "{errors:?}");
    assert!(errors[1].1.contains("not a whole number") && errors[2].1.contains("has 1 values, the header has 2"), "{errors:?}");
    // The summary lists them.
    for selector in ["import-errors", "import-error-0", "import-error-2"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // Skipped rows are in the Error Log as one warning.
    let warnings = rig.app.read_with(rig.visual, |this, _| (0..this.error_log.log.len()).filter_map(|index| this.error_log.log.get(index)).filter(|entry| entry.severity == crate::error_log::Severity::Warning).count());
    assert_eq!(warnings, 1);
}

/// A CSV of `rows` rows for `big`, every id unique.
fn di_big_file(dir: &tempfile::TempDir, rows: usize) -> std::path::PathBuf {
    use std::io::Write as _;
    let path = dir.path().join("big.csv");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    writeln!(out, "id,v").unwrap();
    for id in 1..=rows {
        writeln!(out, "{id},value {id}").unwrap();
    }
    out.flush().unwrap();
    path
}

fn di_cancel_midway(cx: &mut TestAppContext, transaction_steps: i32) -> (SqliteDb, crate::app::data_import::run::Report) {
    let db = SqliteDb::new("CREATE TABLE big(id INTEGER PRIMARY KEY, v TEXT);");
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = di_big_file(&dir, 300_000);
    let big = oa_table(&rig, "big");
    di_open(&mut rig, &big, &path);
    di_to_load(&mut rig);
    // One row per batch: slow enough to cancel in the middle.
    di_type(&mut rig, "import-batch", "1");
    if transaction_steps > 0 {
        di_choose(&mut rig, "import-transaction", transaction_steps);
    }
    rig.click("import-start");
    di_wait(&mut rig, "some rows to go in", |s| s.progress.is_some_and(|progress| progress.inserted > 50));
    assert!(rig.visual.debug_bounds("import-progress").is_some() && rig.visual.debug_bounds("import-cancel").is_some());
    rig.click("import-cancel");
    di_wait(&mut rig, "the cancelled import to end", |s| s.report.is_some());
    let report = di_snapshot(&rig.app, rig.visual).report.unwrap();
    assert_eq!(report.outcome, ImportOutcome::Cancelled);
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.diagram.applies.len()), 0, "a load holds no structure-apply slot");
    // What went in before the cancel is said once in the Error Log.
    assert_eq!(di_warnings(&rig.app, rig.visual), usize::from(report.inserted > 0));
    drop(rig);
    (db, report)
}

#[gpui::test]
fn cancel_stops_the_load_between_batches_and_keeps_what_was_committed(cx: &mut TestAppContext) {
    let (db, report) = di_cancel_midway(cx, 0);
    let stored: u64 = db.sql("SELECT count(*) FROM big").parse().unwrap();
    assert!(stored > 50 && stored < 300_000, "{stored}");
    assert_eq!((report.inserted, report.rolled_back), (stored, 0));
}

#[gpui::test]
fn cancel_in_one_transaction_rolls_everything_back(cx: &mut TestAppContext) {
    let (db, report) = di_cancel_midway(cx, 1);
    assert_eq!(db.sql("SELECT count(*) FROM big"), "0");
    assert_eq!(report.inserted, 0);
    assert!(report.rolled_back > 50, "{}", report.rolled_back);
}

#[gpui::test]
fn a_read_only_connection_refuses_and_a_production_one_asks_before_loading(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let db = SqliteDb::new(NUMS_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let dir = tempfile::tempdir().unwrap();
    let nums = oa_table(&rig, "nums");

    // Read-only: refused with the reason, no tab, no prompt.
    publish_types(&rig.app, rig.visual, true);
    give_profile(&rig.app, rig.visual, &key, typed(PRODUCTION));
    oa_menu(&mut rig, &nums, "object-menu-import");
    assert!(!rig.visual.did_prompt_for_paths(), "nothing is asked of a read-only connection");
    let notice = rig.app.read_with(rig.visual, |this, _| this.sidebar.notice.clone()).expect("the refusal is shown");
    assert!(notice.contains("Importing data is") && notice.contains("read-only"), "{notice}");
    assert!(rig.app.read_with(rig.visual, |this, _| this.imports.is_empty()));

    // Production, writable: the wizard opens, Import asks first.
    publish_types(&rig.app, rig.visual, false);
    let path = di_write(&dir, "nums.csv", "id,qty\n1,1\n2,2\n");
    di_open(&mut rig, &nums, &path);
    di_to_load(&mut rig);
    rig.click("import-start");
    assert!(rig.visual.debug_bounds("import-confirm").is_some(), "Production asks for its confirmation");
    assert_eq!(db.sql("SELECT count(*) FROM nums"), "0", "nothing before the answer");
    assert!(!di_snapshot(&rig.app, rig.visual).running);
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert_eq!(db.sql("SELECT count(*) FROM nums"), "0");
    assert!(di_snapshot(&rig.app, rig.visual).report.is_none(), "cancelled: nothing ran");
    rig.click("import-start");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    di_wait(&mut rig, "the confirmed import to end", |s| s.report.is_some());
    assert_eq!(db.sql("SELECT count(*) FROM nums"), "2");

    // The connection turns read-only while the wizard is open: Import refuses.
    rig.click("import-again");
    let again = di_write(&dir, "more.csv", "id,qty\n3,3\n");
    rig.visual.simulate_path_prompt_response(move |_| Some(vec![again.clone()]));
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    di_wait(&mut rig, "the second file", |s| s.rows == 1 && s.step == "source");
    di_to_load(&mut rig);
    publish_types(&rig.app, rig.visual, true);
    rig.click("import-start");
    let message = di_snapshot(&rig.app, rig.visual).message.expect("the refusal is shown in the wizard");
    assert!(message.contains("read-only"), "{message}");
    assert_eq!(db.sql("SELECT count(*) FROM nums"), "2");
}

#[gpui::test]
fn hostile_table_and_column_names_are_quoted_not_interpreted(cx: &mut TestAppContext) {
    let table = "t\"x; DROP TABLE decoy; --";
    let db = SqliteDb::new(
        "CREATE TABLE decoy(id INTEGER); INSERT INTO decoy VALUES (1); CREATE TABLE \"t\"\"x; DROP TABLE decoy; --\"(\"a\"\"b\" TEXT, \"c d; --\" TEXT);",
    );
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let dir = tempfile::tempdir().unwrap();
    let path = di_write(&dir, "h.csv", "a\"b,c d; --\n\"it's\",\"x\"\"y\"\n'; DELETE FROM decoy; --,z\n");
    let hostile = oa_table(&rig, table);
    di_open(&mut rig, &hostile, &path);
    assert_eq!(di_snapshot(&rig.app, rig.visual).columns, ["a\"b", "c d; --"]);
    di_to_load(&mut rig);
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted), (ImportOutcome::Completed, 2));
    assert_eq!(db.sql("SELECT count(*) FROM decoy"), "1", "no statement was smuggled in");
    assert_eq!(db.sql("SELECT \"a\"\"b\" || '|' || \"c d; --\" FROM \"t\"\"x; DROP TABLE decoy; --\" ORDER BY rowid"), "it's|x\"y\n'; DELETE FROM decoy; --|z");

    // A new table with such names: the review shows them quoted and the load fills it.
    rig.click("import-close");
    let tables = group_node(&key, "main", "main", ObjectGroup::Tables);
    di_open(&mut rig, &tables, &path);
    rig.click("import-next");
    di_type(&mut rig, "import-new-name", "n\"w; DROP TABLE decoy; --");
    rig.click("import-next");
    rig.click("import-create");
    let script = oa_script(rig.visual);
    assert!(script.contains("\"n\"\"w; DROP TABLE decoy; --\"") && script.contains("\"a\"\"b\""), "{script}");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    di_wait(&mut rig, "the table to exist", |s| s.created);
    let report = di_run(&mut rig);
    assert_eq!(report.inserted, 2);
    assert_eq!(db.sql("SELECT count(*) FROM decoy"), "1");
    assert_eq!(db.sql("SELECT count(*) FROM \"n\"\"w; DROP TABLE decoy; --\""), "2");
}

#[gpui::test]
fn closing_a_running_import_asks_and_cancelling_it_stops_the_load(cx: &mut TestAppContext) {
    let db = SqliteDb::new("CREATE TABLE big(id INTEGER PRIMARY KEY, v TEXT);");
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = di_big_file(&dir, 300_000);
    let big = oa_table(&rig, "big");
    di_open(&mut rig, &big, &path);
    di_to_load(&mut rig);
    di_type(&mut rig, "import-batch", "1");
    rig.click("import-start");
    di_wait(&mut rig, "the load to run", |s| s.progress.is_some_and(|progress| progress.inserted > 20));
    let tab = di_import_tab(&rig.app, rig.visual);
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.confirm_close_tab(tab, window, cx)));
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("discard-pending-body").is_some(), "closing a running import asks");
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.tab(tab).is_some()), "kept");
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.confirm_close_tab(tab, window, cx)));
    settle(rig.visual);
    rig.click("discard-pending-confirm");
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.tab(tab).is_none() && this.imports.is_empty()));
    // The load stops on its own: the count settles below the file's.
    let mut last = String::new();
    for _ in 0..200 {
        std::thread::sleep(Duration::from_millis(25));
        let now = db.sql("SELECT count(*) FROM big");
        if now == last {
            break;
        }
        last = now;
    }
    let stored: u64 = last.parse().unwrap();
    assert!(stored < 300_000, "the load was cancelled with the tab: {stored}");
}

/// Answers the file prompt the wizard just raised and waits for the file to be read.
fn di_answer_prompt(rig: &mut SqlRig<'_>, path: &std::path::Path) {
    assert!(rig.visual.did_prompt_for_paths(), "the wizard asks the platform for a file");
    let path = path.to_owned();
    rig.visual.simulate_path_prompt_response(move |_| Some(vec![path.clone()]));
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(rig.visual);
    di_wait_sample(rig);
}

fn di_warnings(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> usize {
    app.read_with(visual, |this, _| {
        (0..this.error_log.log.len()).filter_map(|index| this.error_log.log.get(index)).filter(|entry| entry.severity == crate::error_log::Severity::Warning).count()
    })
}

#[gpui::test]
fn a_file_chosen_or_edited_after_the_table_was_created_is_mapped_by_name_and_a_changed_file_is_refused(cx: &mut TestAppContext) {
    let db = SqliteDb::new(PEOPLE_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let dir = tempfile::tempdir().unwrap();
    let tables = group_node(&key, "main", "main", ObjectGroup::Tables);
    di_open(&mut rig, &tables, &di_write(&dir, "first.csv", "n,label\n1,a\n"));
    rig.click("import-next");
    di_type(&mut rig, "import-new-name", "late");
    rig.click("import-next");
    rig.click("import-create");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    di_wait(&mut rig, "the table to exist", |s| s.created);

    // Another file, its columns in another order and one the table lacks: mapped by name, nothing of the old mapping.
    rig.click("import-browse");
    di_answer_prompt(&mut rig, &di_write(&dir, "second.csv", "label,n,extra\nx,5,zzz\n"));
    rig.click("import-next");
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.step, snapshot.assignments.clone()), ("mapping", vec![Some(1), Some(0), None]));
    assert!(rig.visual.debug_bounds("import-new-name").is_none(), "the table exists: no name to type any more");
    rig.click("import-next");
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted), (ImportOutcome::Completed, 1));
    assert_eq!(db.sql("SELECT n || '|' || label FROM late"), "5|x");

    // A third file is previewed, mapped, and then edited on disk: the load refuses it before writing anything.
    let third = di_write(&dir, "third.csv", "n,label\n9,q\n");
    rig.click("import-again");
    di_answer_prompt(&mut rig, &third);
    di_to_load(&mut rig);
    std::fs::write(&third, "other,cols\n1,2\n").unwrap();
    let report = di_run(&mut rig);
    let ImportOutcome::Failed(reason) = &report.outcome else { panic!("a refusal, got {:?}", report.outcome) };
    assert!(reason.contains("changed since it was previewed") && reason.contains("\"other\""), "{reason}");
    assert_eq!(db.sql("SELECT count(*) FROM late"), "1");
}

#[gpui::test]
fn json_numbers_keep_their_digits_and_keys_after_the_preview_go_to_their_column_or_follow_the_policy(cx: &mut TestAppContext) {
    use std::io::Write as _;
    let db = SqliteDb::new("CREATE TABLE late(id INTEGER PRIMARY KEY, qty INTEGER, big TEXT);");
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late.ndjson");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    for id in 1..=1100 {
        let extra = match id {
            1 => ",\"big\": 12345678901234567890123".to_owned(),
            1050 => ",\"qty\": 7".to_owned(),
            1060 => ",\"zzz\": 1".to_owned(),
            _ => String::new(),
        };
        writeln!(out, "{{\"id\": {id}{extra}}}").unwrap();
    }
    out.flush().unwrap();
    drop(out);
    let late = oa_table(&rig, "late");
    di_open(&mut rig, &late, &path);
    assert_eq!(di_snapshot(&rig.app, rig.visual).columns, ["id", "big"], "qty and zzz are past the first 1000 rows");
    di_to_load(&mut rig);
    // Stop: the key that is nowhere is refused, not dropped.
    let report = di_run(&mut rig);
    let ImportOutcome::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
    assert!(reason.contains("Row 1060") && reason.contains("\"zzz\""), "{reason}");
    assert_eq!(db.sql("SELECT big FROM late WHERE id = 1"), "12345678901234567890123", "the digits as written");
    db.sql("DELETE FROM late");
    rig.click("import-close");

    // Skip: the row with the unknown key is logged, and the key that is a column of the table is loaded by name.
    di_open(&mut rig, &late, &path);
    di_to_load(&mut rig);
    di_choose(&mut rig, "import-on-error", 1);
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted, report.skipped), (ImportOutcome::Completed, 1099, 1));
    assert_eq!(db.sql("SELECT qty FROM late WHERE id = 1050"), "7");
}

#[gpui::test]
fn old_mac_line_breaks_a_decimal_comma_and_a_wrapped_json_array_are_handled_or_refused_clearly(cx: &mut TestAppContext) {
    let db = SqliteDb::new("CREATE TABLE prices(id INTEGER PRIMARY KEY, price REAL);");
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let prices = oa_table(&rig, "prices");
    di_open(&mut rig, &prices, &di_write(&dir, "p.csv", "id,price\r1,\"1.234,50\"\r2,\"1.5\"\r"));
    assert_eq!(di_snapshot(&rig.app, rig.visual).rows, 2, "a lone carriage return ends a record");
    // A decimal comma: dots only group thousands, so 1.5 is not a number in this file.
    di_choose(&mut rig, "import-decimal", 1);
    rig.click("import-next");
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert!(snapshot.notes.iter().any(|note| note.contains("1 of 2 values is not a number") && note.contains("\"1.5\"")), "{:?}", snapshot.notes);
    rig.click("import-next");
    di_choose(&mut rig, "import-on-error", 1);
    let report = di_run(&mut rig);
    assert_eq!((report.inserted, report.skipped), (1, 1));
    assert_eq!(db.sql("SELECT price FROM prices"), "1234.5");
    rig.click("import-close");

    // One object holding the rows is not rows: said so, nothing is guessed.
    let wrapped = di_write(&dir, "wrapped.json", "{\"data\": [{\"id\": 1}, {\"id\": 2}]}");
    di_open(&mut rig, &prices, &wrapped);
    let message = di_snapshot(&rig.app, rig.visual).sample_error.expect("the file is refused");
    assert!(message.contains("\"data\"") && message.contains("array of objects"), "{message}");
}

#[gpui::test]
fn the_mapping_makes_widgets_for_the_rows_on_screen_only(cx: &mut TestAppContext) {
    let db = SqliteDb::new(PEOPLE_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let dir = tempfile::tempdir().unwrap();
    let header: Vec<String> = (0..1500).map(|column| format!("c{column}")).collect();
    let file = di_write(&dir, "wide.csv", &format!("{}\n{}\n", header.join(","), vec!["1"; 1500].join(",")));
    // An existing table: a select per drawn row.
    let people = oa_table(&rig, "people");
    di_open(&mut rig, &people, &file);
    rig.click("import-next");
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.assignments.len(), 1500);
    assert!(snapshot.widgets > 0 && snapshot.widgets < 60, "{} selects for 1500 columns", snapshot.widgets);
    let tab = di_import_tab(&rig.app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(rig.visual);
    // A new table: the name and type inputs of the drawn rows.
    let tables = group_node(&key, "main", "main", ObjectGroup::Tables);
    di_open(&mut rig, &tables, &file);
    rig.click("import-next");
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.new_columns.len(), 1500);
    assert!(snapshot.widgets > 0 && snapshot.widgets < 60, "{} input pairs for 1500 columns", snapshot.widgets);
}

#[gpui::test]
fn a_cancelled_load_that_added_rows_is_logged_also_when_its_tab_is_closed(cx: &mut TestAppContext) {
    let db = SqliteDb::new("CREATE TABLE big(id INTEGER PRIMARY KEY, v TEXT);");
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = di_big_file(&dir, 300_000);
    let big = oa_table(&rig, "big");
    di_open(&mut rig, &big, &path);
    di_to_load(&mut rig);
    di_type(&mut rig, "import-batch", "1");
    rig.click("import-start");
    di_wait(&mut rig, "some rows to go in", |s| s.progress.is_some_and(|progress| progress.inserted > 20));
    let tab = di_import_tab(&rig.app, rig.visual);
    // The tab goes away mid-load (the load is cancelled with it): the warning still reaches the Error Log.
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    assert!(rig.app.read_with(rig.visual, |this, _| this.imports.is_empty()));
    let app = rig.app.clone();
    wait_until(rig.visual, "the cancelled load to be logged", |visual| di_warnings(&app, visual) == 1);
}

#[gpui::test]
fn choosing_another_file_after_a_load_starts_over_at_the_source_step(cx: &mut TestAppContext) {
    let db = SqliteDb::new(NUMS_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    di_nums(&mut rig, &dir, "id,qty\n1,1\n", "10");
    let report = di_run(&mut rig);
    assert_eq!(report.inserted, 1);
    assert!(rig.visual.debug_bounds("import-again").is_some() && rig.visual.debug_bounds("import-close").is_some());
    // "Choose file…" in the header: the old summary goes, and the Source step has its own buttons.
    rig.click("import-browse");
    di_answer_prompt(&mut rig, &di_write(&dir, "next.csv", "id,qty\n2,2\n"));
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.step, "source");
    assert!(snapshot.report.is_none(), "the last load's report is cleared");
    assert!(rig.visual.debug_bounds("import-next").is_some());
    assert!(rig.visual.debug_bounds("import-again").is_none() && rig.visual.debug_bounds("import-close").is_none());
}

#[gpui::test]
fn the_gates_are_looked_at_again_when_the_confirmed_load_starts(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let db = SqliteDb::new(NUMS_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let dir = tempfile::tempdir().unwrap();
    publish_types(&rig.app, rig.visual, false);
    give_profile(&rig.app, rig.visual, &key, typed(PRODUCTION));
    di_nums(&mut rig, &dir, "id,qty\n1,1\n2,2\n", "10");
    rig.click("import-start");
    assert!(rig.visual.debug_bounds("import-confirm").is_some(), "Production asks first");
    // While the dialog is open the connection becomes read-only: OK must not load.
    publish_types(&rig.app, rig.visual, true);
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert!(snapshot.message.as_deref().is_some_and(|message| message.contains("read-only")), "{:?}", snapshot.message);
    assert!(!snapshot.running && snapshot.report.is_none());
    assert_eq!(db.sql("SELECT count(*) FROM nums"), "0");
}

// ---- live servers (ignored: they need the dedicated tablex-test-* containers) --------------------

/// Opens the import wizard on `origin` (the menu is covered on SQLite) and answers the file prompt.
fn di_live_open(rig: &mut SqlRig<'_>, origin: crate::app::structure_edit::apply::TableOrigin, path: &std::path::Path) {
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.open_import_data(origin, window, cx)));
    settle(rig.visual);
    let path = path.to_owned();
    assert!(rig.visual.did_prompt_for_paths());
    rig.visual.simulate_path_prompt_response(move |_| Some(vec![path.clone()]));
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    di_wait_sample(rig);
}

/// Brings the import tab back (the checks ran SQL in the rig's own tab) and closes the wizard.
fn di_close(rig: &mut SqlRig<'_>) {
    let tab = di_import_tab(&rig.app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.activate_tab_ui(tab, cx));
    settle(rig.visual);
    rig.click("import-close");
}

/// Loads a CSV with quoting, line breaks, unicode, NULLs and typed columns into a scratch table, once
/// with the default policy, once skipping a duplicate, then creates a new table from a file; checks the
/// rows with SQL; drops both tables (also when the test fails).
fn live_import(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let postgres = kind == ConnectionKind::Postgres;
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let schema = if postgres { "public".to_owned() } else { database.clone() };
    let suffix = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let (existing, created) = (format!("cf_r4a_{suffix}"), format!("cf_r4a_new_{suffix}"));
    let mut rig = nav_rig(cx, kind, driver, &uri);
    let key = rig.key.clone();
    let qualified = |name: &str| if postgres { format!("public.{name}") } else { format!("{database}.{name}") };
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP TABLE IF EXISTS {}", qualified(&existing)), format!("DROP TABLE IF EXISTS {}", qualified(&created))]);
    let text = |column: &str| if postgres { format!("{column}::text") } else { format!("CAST({column} AS CHAR)") };
    run_confirmed(
        &mut rig,
        &format!(
            "CREATE TABLE {} (id bigint PRIMARY KEY, name varchar(100) NOT NULL, qty int, price numeric(12,2), ok boolean, born date, seen timestamp)",
            qualified(&existing)
        ),
    );
    let origin = |object: &str| crate::app::structure_edit::apply::TableOrigin { connection: key.clone(), database: database.clone(), schema: schema.clone(), object: object.to_owned() };
    let dir = tempfile::tempdir().unwrap();

    // 1. A file with everything awkward in it, into the existing table, three rows to a batch.
    let file = di_write(
        &dir,
        "live.csv",
        "id,name,qty,price,ok,born,seen\n1,Zoë 😀,5,1234.50,true,2024-02-29,2024-02-29 13:45:10\n2,\"two\nlines, \"\"quoted\"\"\",,0.05,false,,\n3,'; DROP TABLE x; --,7,-3,1,2000-01-01,2000-01-01T00:00:00\n4,four,,,,,\n5,five,9,99999999.99,t,1999-12-31,1999-12-31 23:59:59\n",
    );
    di_live_open(&mut rig, origin(&existing), &file);
    di_to_load(&mut rig);
    di_type(&mut rig, "import-batch", "3");
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted, report.skipped), (ImportOutcome::Completed, 5, 0), "{:?}", report.errors);
    let t = qualified(&existing);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM {t}")), "5");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT name FROM {t} WHERE id = 1")), "Zoë 😀");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT name FROM {t} WHERE id = 2")), "two\nlines, \"quoted\"");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT name FROM {t} WHERE id = 3")), "'; DROP TABLE x; --");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT {} FROM {t} WHERE id = 1", text("price"))), "1234.50");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT {} FROM {t} WHERE id = 5", text("price"))), "99999999.99");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT {} FROM {t} WHERE id = 1", text("born"))), "2024-02-29");
    assert!(oa_scalar(&mut rig, &format!("SELECT {} FROM {t} WHERE id = 1", text("seen"))).starts_with("2024-02-29 13:45:10"));
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM {t} WHERE qty IS NULL AND born IS NULL AND price IS NOT NULL")), "1", "row 2: empty cells of typed columns are NULL");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM {t} WHERE ok = {}", if postgres { "true" } else { "1" })), "3", "true, 1 and t");
    di_close(&mut rig);

    // 2. One transaction, skipping: a duplicate key and a bad number are logged, the rest commits.
    let again = di_write(&dir, "again.csv", "id,name,qty\n6,six,6\n1,duplicate,1\n7,seven,x\n8,eight,8\n");
    di_live_open(&mut rig, origin(&existing), &again);
    di_to_load(&mut rig);
    di_choose(&mut rig, "import-transaction", 1);
    di_choose(&mut rig, "import-on-error", 1);
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted, report.skipped), (ImportOutcome::Completed, 2, 2), "{:?}", report.errors);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM {t}")), "7");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT name FROM {t} WHERE id = 1")), "Zoë 😀", "the duplicate did not overwrite");
    di_close(&mut rig);

    // 3. One transaction, stop: nothing of the file stays.
    let doomed = di_write(&dir, "doomed.csv", "id,name\n20,a\n21,b\n1,duplicate\n22,c\n");
    di_live_open(&mut rig, origin(&existing), &doomed);
    di_to_load(&mut rig);
    di_type(&mut rig, "import-batch", "2");
    di_choose(&mut rig, "import-transaction", 1);
    let report = di_run(&mut rig);
    assert!(matches!(report.outcome, ImportOutcome::Stopped(_)), "{:?}", report.outcome);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM {t}")), "7", "rolled back");
    di_close(&mut rig);

    // 4. A new table from a file, through the review.
    let fresh = di_write(&dir, "fresh.csv", "n,label,price,day,ok\n1,a,1.5,2024-01-02,true\n2,b,2.25,2024-02-03,false\n");
    di_live_open(&mut rig, origin(""), &fresh);
    rig.click("import-next");
    di_type(&mut rig, "import-new-name", &created);
    rig.click("import-next");
    rig.click("import-create");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    di_wait(&mut rig, "the table to exist", |s| s.created);
    if matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb) {
        // The engine of the new table is the server's default: read from the table, not assumed.
        di_wait(&mut rig, "the new table to be described", |s| s.engine.is_some());
        let snapshot = di_snapshot(&rig.app, rig.visual);
        assert_eq!(snapshot.engine.as_deref(), Some("InnoDB"));
        assert!(snapshot.transactions);
    }
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted), (ImportOutcome::Completed, 2), "{:?}", report);
    let c = qualified(&created);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM {c}")), "2");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT {} FROM {c} WHERE n = 2", text("price"))).trim_end_matches('0').trim_end_matches('.'), "2.25");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT {} FROM {c} WHERE n = 2", text("day"))), "2024-02-03");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_import(cx: &mut TestAppContext) {
    live_import(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_import(cx: &mut TestAppContext) {
    live_import(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_import(cx: &mut TestAppContext) {
    live_import(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI (container tablex-test-clickhouse)"]
fn live_clickhouse_import(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_CLICKHOUSE_URI") else {
        eprintln!("skipped: set TABLEX_TEST_CLICKHOUSE_URI to the dedicated tablex-test-clickhouse container");
        return;
    };
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let suffix = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let (existing, created) = (format!("cf_r4a_{suffix}"), format!("cf_r4a_new_{suffix}"));
    let mut rig = nav_rig(cx, ConnectionKind::Clickhouse, CLICKHOUSE_DRIVER, &uri);
    let key = rig.key.clone();
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP TABLE IF EXISTS {database}.{existing}"), format!("DROP TABLE IF EXISTS {database}.{created}")]);
    run_confirmed(
        &mut rig,
        &format!("CREATE TABLE {database}.{existing} (id Int64, name String, qty Nullable(Int32), price Decimal(12,2), born Nullable(Date)) ENGINE = MergeTree ORDER BY id"),
    );
    let origin = |object: &str| crate::app::structure_edit::apply::TableOrigin { connection: key.clone(), database: database.clone(), schema: database.clone(), object: object.to_owned() };
    let dir = tempfile::tempdir().unwrap();
    let file = di_write(&dir, "ch.csv", "id,name,qty,price,born\n1,\"Zoë, \"\"Z\"\"\",5,1234.50,2024-02-29\n2,two,,0.05,\n3,three,7,-3,2000-01-01\n");
    di_live_open(&mut rig, origin(&existing), &file);
    di_to_load(&mut rig);
    // ClickHouse has no transactions: the choice is off and every batch stands alone.
    di_type(&mut rig, "import-batch", "2");
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted), (ImportOutcome::Completed, 3), "{:?}", report.errors);
    let t = format!("{database}.{existing}");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count() FROM {t}")), "3");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT name FROM {t} WHERE id = 1")), "Zoë, \"Z\"");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT toString(price) FROM {t} WHERE id = 1")), "1234.5");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count() FROM {t} WHERE qty IS NULL AND born IS NULL")), "1");
    di_close(&mut rig);

    // A new table.
    let fresh = di_write(&dir, "fresh.csv", "n,label,price,day\n1,a,1.5,2024-01-02\n2,b,2.25,2024-02-03\n");
    di_live_open(&mut rig, origin(""), &fresh);
    rig.click("import-next");
    di_type(&mut rig, "import-new-name", &created);
    rig.click("import-next");
    rig.click("import-create");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    di_wait(&mut rig, "the table to exist", |s| s.created);
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted), (ImportOutcome::Completed, 2), "{:?}", report);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count() FROM {database}.{created}")), "2");
}

// ---- performance (ignored): `TABLEX_PERF_ROWS` rows (default 1,000,000) x 8 columns ---------------

fn perf_rss_kb() -> u64 {
    let output = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().expect("ps");
    String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0)
}

fn perf_file(dir: &tempfile::TempDir, rows: usize) -> std::path::PathBuf {
    use std::io::Write as _;
    let path = dir.path().join("perf.csv");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    writeln!(out, "id,name,email,qty,price,flag,day,note").unwrap();
    for id in 1..=rows {
        writeln!(
            out,
            "{id},name {id},user{id}@example.com,{},{}.{:02},{},2024-{:02}-{:02},\"a note, with a comma and \"\"quotes\"\" for row {id}\"",
            id % 1000,
            id % 5000,
            id % 100,
            if id % 2 == 0 { "true" } else { "false" },
            id % 12 + 1,
            id % 28 + 1
        )
        .unwrap();
    }
    out.flush().unwrap();
    path
}

/// Imports `rows` generated rows through the wizard into the table `origin` names and prints rows/s and peak RSS.
fn perf_import(rig: &mut SqlRig<'_>, label: &str, origin: crate::app::structure_edit::apply::TableOrigin, rows: usize, batch: &str, whole: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = perf_file(&dir, rows);
    di_live_open(rig, origin, &path);
    let view = rig.app.read_with(rig.visual, |this, _| this.imports.values().next().cloned().expect("the wizard"));
    let (batch_rows, app) = (batch.parse::<usize>().unwrap(), rig.app.clone());
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
    rig.visual.update(|window, cx| view.update(cx, |view, cx| view.start_for_test(batch_rows, whole, window, cx)));
    let start = std::time::Instant::now();
    loop {
        rig.visual.run_until_parked();
        if di_snapshot(&app, rig.visual).report.is_some() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(3600), "the import did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    let elapsed = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    sampler.join().unwrap();
    let report = di_snapshot(&app, rig.visual).report.unwrap();
    assert_eq!((report.outcome.clone(), report.inserted), (ImportOutcome::Completed, rows as u64), "{:?}", report.errors);
    let peak = peak.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "PERF {label}: {rows} rows x 8 columns ({:.1} MB), batch {batch}{}: {:.1} s, {:.0} rows/s; RSS {:.0} MB before, peak {:.0} MB (+{:.0} MB)",
        std::fs::metadata(&path).unwrap().len() as f64 / 1e6,
        if whole { ", one transaction" } else { "" },
        elapsed.as_secs_f64(),
        rows as f64 / elapsed.as_secs_f64(),
        baseline as f64 / 1024.0,
        peak as f64 / 1024.0,
        (peak - baseline) as f64 / 1024.0
    );
    let tab = di_import_tab(&app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
}

fn perf_rows() -> usize {
    std::env::var("TABLEX_PERF_ROWS").ok().and_then(|rows| rows.parse().ok()).unwrap_or(1_000_000)
}

#[gpui::test]
#[ignore = "generates a large file; prints rows/s and peak RSS"]
fn perf_import_into_sqlite(cx: &mut TestAppContext) {
    let db = SqliteDb::new("CREATE TABLE perf(id INTEGER PRIMARY KEY, name TEXT, email TEXT, qty INTEGER, price REAL, flag BOOLEAN, day DATE, note TEXT);");
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    let origin = crate::app::structure_edit::apply::TableOrigin { connection: key, database: "main".into(), schema: "main".into(), object: "perf".into() };
    perf_import(&mut rig, "SQLite", origin.clone(), perf_rows(), "1000", false);
    assert_eq!(db.sql("SELECT count(*) FROM perf"), perf_rows().to_string());
    db.sql("DELETE FROM perf");
    perf_import(&mut rig, "SQLite", origin.clone(), perf_rows(), "5000", true);
    assert_eq!(db.sql("SELECT count(*) FROM perf"), perf_rows().to_string());
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI; generates a large file; prints rows/s and peak RSS"]
fn perf_import_into_postgresql(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let key = rig.key.clone();
    let suffix = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let (first, second) = (format!("cf_r4a_perf_{suffix}"), format!("cf_r4a_perf2_{suffix}"));
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP TABLE IF EXISTS public.{first}"), format!("DROP TABLE IF EXISTS public.{second}")]);
    for table in [&first, &second] {
        run_confirmed(&mut rig, &format!("CREATE TABLE public.{table}(id bigint PRIMARY KEY, name text, email text, qty int, price numeric(12,2), flag boolean, day date, note text)"));
    }
    let origin = |object: &str| crate::app::structure_edit::apply::TableOrigin { connection: key.clone(), database: "tablex".into(), schema: "public".into(), object: object.to_owned() };
    perf_import(&mut rig, "PostgreSQL", origin(&first), perf_rows(), "1000", false);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM public.{first}")), perf_rows().to_string());
    // The one-transaction mode on a tenth of the rows: the same rate, ten times sooner.
    let tenth = (perf_rows() / 10).max(1);
    perf_import(&mut rig, "PostgreSQL", origin(&second), tenth, "5000", true);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM public.{second}")), tenth.to_string());
}

/// MySQL and MariaDB: a MyISAM table has no transactions. "One transaction" is off for it, and a batch the server
/// takes part of is reported as it is (the rows it wrote counted, the load ended, nothing retried even when skipping);
/// an InnoDB table keeps the option.
fn live_partial(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let suffix = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let (myisam, innodb) = (format!("cf_r4a_my_{suffix}"), format!("cf_r4a_in_{suffix}"));
    let mut rig = nav_rig(cx, kind, driver, &uri);
    let key = rig.key.clone();
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP TABLE IF EXISTS {database}.{myisam}"), format!("DROP TABLE IF EXISTS {database}.{innodb}")]);
    run_confirmed(&mut rig, &format!("CREATE TABLE {database}.{myisam} (id INT PRIMARY KEY, v INT) ENGINE=MyISAM"));
    run_confirmed(&mut rig, &format!("CREATE TABLE {database}.{innodb} (id INT PRIMARY KEY, v INT) ENGINE=InnoDB"));
    let origin = |object: &str| crate::app::structure_edit::apply::TableOrigin { connection: key.clone(), database: database.clone(), schema: database.clone(), object: object.to_owned() };
    let dir = tempfile::tempdir().unwrap();
    let file = di_write(&dir, "dups.csv", "id,v\n1,1\n2,2\n3,3\n2,99\n5,5\n6,6\n");

    di_live_open(&mut rig, origin(&myisam), &file);
    let snapshot = di_snapshot(&rig.app, rig.visual);
    assert!(!snapshot.transactions && snapshot.engine.as_deref() == Some("MyISAM"));
    di_to_load(&mut rig);
    assert!(rig.visual.debug_bounds("import-transaction-note").is_some(), "the note says why One transaction is off");
    di_choose(&mut rig, "import-on-error", 1);
    let report = di_run(&mut rig);
    let ImportOutcome::Failed(reason) = &report.outcome else { panic!("a failed load, got {:?}", report.outcome) };
    assert!(reason.contains("only partly applied") && reason.contains("MyISAM"), "{reason}");
    assert_eq!((report.inserted, report.skipped, report.uncertain), (3, 0, 0), "{:?}", report.errors);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT COUNT(*) FROM {database}.{myisam}")), "3", "the rest of the batch was not tried again");
    di_close(&mut rig);

    di_live_open(&mut rig, origin(&innodb), &file);
    assert!(di_snapshot(&rig.app, rig.visual).transactions);
    di_to_load(&mut rig);
    di_choose(&mut rig, "import-transaction", 1);
    di_choose(&mut rig, "import-on-error", 1);
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted, report.skipped), (ImportOutcome::Completed, 5, 1), "{:?}", report.errors);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_partly_applied_batch(cx: &mut TestAppContext) {
    live_partial(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_partly_applied_batch(cx: &mut TestAppContext) {
    live_partial(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_bytea_takes_hex_and_never_stores_text_as_bytes(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI to the dedicated tablex-test-pg container");
        return;
    };
    let table = format!("cf_r4a_bytes_{}", &uuid::Uuid::new_v4().to_simple().to_string()[..8]);
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let key = rig.key.clone();
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP TABLE IF EXISTS public.{table}")]);
    run_confirmed(&mut rig, &format!("CREATE TABLE public.{table} (id int PRIMARY KEY, data bytea)"));
    let origin = crate::app::structure_edit::apply::TableOrigin { connection: key, database: "tablex".into(), schema: "public".into(), object: table.clone() };
    let dir = tempfile::tempdir().unwrap();
    // \x00ff and 0xDEAD are the same bytes spelled two ways; an empty cell is NULL.
    let file = di_write(&dir, "bytes.csv", "id,data\n1,\\x00ff\n2,0xDEAD\n3,\n4,\\x\n");
    di_live_open(&mut rig, origin.clone(), &file);
    di_to_load(&mut rig);
    let report = di_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.inserted), (ImportOutcome::Completed, 4), "{:?}", report.outcome);
    let t = format!("public.{table}");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT encode(data, 'hex') FROM {t} WHERE id = 1")), "00ff");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT encode(data, 'hex') FROM {t} WHERE id = 2")), "dead");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM {t} WHERE id = 3 AND data IS NULL")), "1");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT length(data)::text FROM {t} WHERE id = 4")), "0");
    di_close(&mut rig);

    // Text that is not hex is refused in the preview's notes and by the load, never stored as its own ASCII bytes.
    let bad = di_write(&dir, "bad.csv", "id,data\n5,00ff\n6,hello\n");
    di_live_open(&mut rig, origin, &bad);
    rig.click("import-next");
    assert!(di_snapshot(&rig.app, rig.visual).notes.iter().any(|note| note.contains("is not binary")), "{:?}", di_snapshot(&rig.app, rig.visual).notes);
    rig.click("import-next");
    let report = di_run(&mut rig);
    assert!(matches!(report.outcome, ImportOutcome::Stopped(_)), "{:?}", report.outcome);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM {t}")), "4");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI; generates a large file; prints rows/s and peak RSS"]
fn perf_import_into_mysql(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_MYSQL_URI") else { return };
    let mut rig = nav_rig(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), &uri);
    let key = rig.key.clone();
    let suffix = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let (first, second) = (format!("cf_r4a_perf_{suffix}"), format!("cf_r4a_perf2_{suffix}"));
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP TABLE IF EXISTS tablex.{first}"), format!("DROP TABLE IF EXISTS tablex.{second}")]);
    for table in [&first, &second] {
        run_confirmed(&mut rig, &format!("CREATE TABLE tablex.{table}(id BIGINT PRIMARY KEY, name TEXT, email TEXT, qty INT, price DECIMAL(12,2), flag BOOLEAN, day DATE, note TEXT)"));
    }
    let origin = |object: &str| crate::app::structure_edit::apply::TableOrigin { connection: key.clone(), database: "tablex".into(), schema: "tablex".into(), object: object.to_owned() };
    perf_import(&mut rig, "MySQL", origin(&first), perf_rows(), "1000", false);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT COUNT(*) FROM tablex.{first}")), perf_rows().to_string());
    let tenth = (perf_rows() / 10).max(1);
    perf_import(&mut rig, "MySQL", origin(&second), tenth, "5000", true);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT COUNT(*) FROM tablex.{second}")), tenth.to_string());
}
