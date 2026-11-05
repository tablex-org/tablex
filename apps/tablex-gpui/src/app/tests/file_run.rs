// R5: running a SQL file, through the real navigator menu, the real file prompt (stubbed answer), the wizard tab and the
// REAL SQLite driver on a second session (read back with the `sqlite3` CLI). The live tests at the end (PostgreSQL, MySQL,
// MariaDB) are `#[ignore]`d: dedicated `tablex-test-*` containers only, with drop guards that clean up on a failure too.

use crate::app::data_transfer::Namespace as FrNamespace;
use crate::app::file_run::run::{EntryKind, Outcome as FrOutcome, Report as FrReport};
use crate::app::file_run::view::Snapshot as FrSnapshot;
use crate::app::file_run::FileRunInit;

fn fr_write(dir: &tempfile::TempDir, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn fr_snapshot(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> FrSnapshot {
    app.read_with(visual, |this, cx| this.file_runs.values().next().expect("a SQL file tab is open").read(cx).snapshot())
}

fn fr_wait(rig: &mut SqlRig<'_>, label: &'static str, ready: impl Fn(&FrSnapshot) -> bool) {
    let app = rig.app.clone();
    wait_until(rig.visual, label, |visual| ready(&fr_snapshot(&app, visual)));
    settle(rig.visual);
}

fn fr_wait_sample(rig: &mut SqlRig<'_>) {
    fr_wait(rig, "the start of the file to be read", |snapshot| !snapshot.reading && (snapshot.size.is_some() || snapshot.sample_error.is_some()));
}

/// "Run SQL File…" on the navigator row `id`, then the file prompt answered with `path`.
fn fr_open(rig: &mut SqlRig<'_>, id: &NavigatorNodeId, path: &std::path::Path) {
    oa_menu(rig, id, "object-menu-run-file");
    assert!(rig.visual.did_prompt_for_paths(), "Run SQL File asks the platform for a file");
    let path = path.to_owned();
    rig.visual.simulate_path_prompt_response(move |options| {
        assert!(options.files && !options.multiple);
        Some(vec![path.clone()])
    });
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(rig.visual);
    fr_wait_sample(rig);
}

/// The wizard opened on the file `path` for the namespace `database`.`schema` of the rig's connection (a path handed over, as a drop would).
fn fr_open_path(rig: &mut SqlRig<'_>, database: &str, schema: &str, path: &std::path::Path) {
    let target = FrNamespace { connection: rig.key.clone(), database: database.into(), schema: schema.into() };
    let path = path.to_owned();
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.open_file_run(FileRunInit { target, path: Some(path) }, window, cx)));
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(rig.visual);
    fr_wait_sample(rig);
}

fn fr_tables(rig: &SqlRig<'_>) -> NavigatorNodeId {
    group_node(&rig.key, "main", "main", ObjectGroup::Tables)
}

/// Next, Next: the Run step.
fn fr_to_run(rig: &mut SqlRig<'_>) {
    rig.click("filerun-next");
    rig.wait_rendered("filerun-next");
    rig.click("filerun-next");
    assert_eq!(fr_snapshot(&rig.app, rig.visual).step, "run");
}

/// Run, then wait for the report.
fn fr_run(rig: &mut SqlRig<'_>) -> FrReport {
    rig.click("filerun-start");
    fr_wait(rig, "the run to end", |snapshot| snapshot.report.is_some());
    fr_snapshot(&rig.app, rig.visual).report.unwrap()
}

/// The Settings step with the transaction row (0 autocommit, 1 whole file, 2 every N) and the error row (0 stop, 1 continue)
/// chosen, then the Run step.
fn fr_settings(rig: &mut SqlRig<'_>, transactions: i32, every: Option<&str>, on_error: i32) {
    rig.click("filerun-next");
    rig.wait_rendered("filerun-transactions");
    if transactions > 0 {
        di_choose(rig, "filerun-transactions", transactions);
    }
    if let Some(every) = every {
        di_type(rig, "filerun-every", every);
    }
    if on_error > 0 {
        di_choose(rig, "filerun-on-error", on_error);
    }
    rig.click("filerun-next");
    assert_eq!(fr_snapshot(&rig.app, rig.visual).step, "run");
}

fn fr_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> u64 {
    app.read_with(visual, |this, _| this.workspace.tabs().iter().find(|tab| tab.kind == TabKind::SqlFile).expect("a SQL file tab").id)
}

/// A script of `inserts` single-row INSERTs into `big`, one per line.
fn fr_big_file(dir: &tempfile::TempDir, inserts: usize) -> std::path::PathBuf {
    use std::io::Write as _;
    let path = dir.path().join("big.sql");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    for id in 1..=inserts {
        writeln!(out, "INSERT INTO big VALUES ({id}, 'value {id}');").unwrap();
    }
    out.flush().unwrap();
    path
}

const FR_SEED: &str = "CREATE TABLE keep(a INTEGER); INSERT INTO keep VALUES (7);";

#[gpui::test]
fn a_sql_file_runs_through_the_wizard_on_a_session_of_its_own(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let script = "-- a dump\nCREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT);\n\nINSERT INTO people VALUES (1, 'Ann; the first');\nINSERT INTO people VALUES (2, 'it''s');\n/* c; */ SELECT * FROM people;\nINSERT INTO people VALUES (3, 'Cy');\nINSERT INTO people VALUES (4, 'Di');\n";
    let path = fr_write(&dir, "dump.sql", script);
    let tables = fr_tables(&rig);
    fr_open(&mut rig, &tables, &path);

    // The tab and the start of the file.
    let (kind, title) = rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        (tab.kind, tab.title.clone())
    });
    assert_eq!((kind, title.as_str()), (TabKind::SqlFile, "Run SQL file"));
    let snapshot = fr_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.step, snapshot.size, snapshot.more), ("source", Some(script.len() as u64), true));
    assert_eq!(snapshot.first[0], "-- a dump CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT)");
    assert_eq!(snapshot.first.len(), 5);
    assert_eq!(snapshot.connections, ["driver-test"]);
    for selector in ["filerun-view", "filerun-source-panel", "filerun-connection", "filerun-namespace", "filerun-encoding", "filerun-start-at", "filerun-sample", "filerun-sample-0", "filerun-next", "filerun-browse"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }

    // Settings, then Run.
    rig.click("filerun-next");
    for selector in ["filerun-settings-panel", "filerun-transactions", "filerun-on-error", "filerun-transaction-note", "filerun-back"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(rig.visual.debug_bounds("filerun-every").is_none(), "the count is asked only for a commit every N");
    rig.click("filerun-next");
    assert!(rig.visual.debug_bounds("filerun-ready").is_some() && rig.visual.debug_bounds("filerun-start").is_some());
    let report = fr_run(&mut rig);
    assert_eq!(report.outcome, FrOutcome::Completed);
    assert_eq!((report.ok, report.failed, report.skipped, report.result_sets, report.last, report.resume_from), (6, 0, 0, 1, 6, None));
    assert_eq!(db.sql("SELECT group_concat(id || ':' || name, '|') FROM (SELECT * FROM people ORDER BY id)"), "1:Ann; the first|2:it's|3:Cy|4:Di");
    assert_eq!(db.sql("SELECT a FROM keep"), "7", "the rest of the database is as it was");
    for selector in ["filerun-summary", "filerun-headline", "filerun-stats", "filerun-again", "filerun-close"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(rig.visual.debug_bounds("filerun-resume").is_none(), "nothing to resume after a complete run");
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());

    // The navigator follows: the new table is listed.
    let key = rig.key.clone();
    let app = rig.app.clone();
    wait_until(rig.visual, "the new table to be listed", |visual| {
        app.read_with(visual, |this, _| {
            this.navigator
                .loaded_children(&group_node(&key, "main", "main", ObjectGroup::Tables))
                .into_iter()
                .flatten()
                .any(|id| matches!(id, NavigatorNodeId::Object { object, .. } if object == "people"))
        })
    });

    // Close.
    rig.click("filerun-close");
    assert!(rig.app.read_with(rig.visual, |this, _| this.file_runs.is_empty()), "the wizard is gone with its tab");
}

const FR_ERRORS: &str = "DROP TABLE IF EXISTS t;\nCREATE TABLE t(id INTEGER PRIMARY KEY);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);\n\nINSERT INTO nope VALUES (3);\nINSERT INTO t VALUES (4);\n";

#[gpui::test]
fn stop_ends_at_the_refused_statement_and_continue_logs_and_goes_on(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = fr_write(&dir, "errors.sql", FR_ERRORS);
    let tables = fr_tables(&rig);
    fr_open(&mut rig, &tables, &path);
    fr_to_run(&mut rig);
    let report = fr_run(&mut rig);
    let FrOutcome::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
    assert!(reason.contains("statement 4") && reason.contains("UNIQUE constraint failed"), "{reason}");
    assert_eq!((report.ok, report.failed, report.last, report.resume_from), (3, 1, 4, Some(4)));
    assert_eq!(db.sql("SELECT group_concat(id) FROM t"), "1", "nothing after the refused statement ran");
    assert_eq!((report.entries.len(), report.entries[0].number, report.entries[0].line, report.entries[0].kind), (1, 4, 4, EntryKind::Failed));
    assert_eq!(report.entries[0].excerpt, "INSERT INTO t VALUES (1)");
    for selector in ["filerun-entries", "filerun-entry-0", "filerun-resume", "filerun-resume-hint", "filerun-kept"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // One entry in the Error Log: a warning.
    assert_eq!(rig.navigator_errors(), [None]);

    // Again, now continuing: the refused statements are listed and the rest ran.
    rig.click("filerun-again");
    let again = path.clone();
    rig.visual.simulate_path_prompt_response(move |_| Some(vec![again.clone()]));
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    fr_wait(&mut rig, "the file again", |s| s.step == "source" && s.size.is_some() && s.report.is_none());
    rig.click("filerun-next");
    di_choose(&mut rig, "filerun-on-error", 1);
    rig.click("filerun-next");
    let report = fr_run(&mut rig);
    assert_eq!(report.outcome, FrOutcome::Completed);
    assert_eq!((report.ok, report.failed, report.last, report.resume_from), (5, 2, 7, None));
    assert_eq!(report.entries.iter().map(|entry| (entry.number, entry.line)).collect::<Vec<_>>(), [(4, 4), (6, 7)]);
    assert!(report.entries[1].message.contains("no such table"), "{:?}", report.entries[1]);
    assert_eq!(db.sql("SELECT group_concat(id) FROM (SELECT id FROM t ORDER BY id)"), "1,2,4");
    assert!(rig.visual.debug_bounds("filerun-entry-1").is_some());
}

#[gpui::test]
fn a_run_resumes_at_the_statement_it_stopped_at_and_start_at_leaves_the_first_ones_out(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = fr_write(&dir, "resume.sql", "DROP TABLE IF EXISTS t;\nCREATE TABLE t(id INTEGER PRIMARY KEY);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);\nINSERT INTO t VALUES (3);\n");
    let tables = fr_tables(&rig);
    fr_open(&mut rig, &tables, &path);
    fr_to_run(&mut rig);
    let report = fr_run(&mut rig);
    assert_eq!((report.resume_from, report.ok), (Some(4), 3));

    // The file is fixed (statement 4 only), and the run goes on from there: what stands is not run twice.
    std::fs::write(&path, "DROP TABLE IF EXISTS t;\nCREATE TABLE t(id INTEGER PRIMARY KEY);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (9);\nINSERT INTO t VALUES (2);\nINSERT INTO t VALUES (3);\n").unwrap();
    rig.click("filerun-resume");
    assert_eq!(fr_snapshot(&rig.app, rig.visual).step, "run");
    let report = fr_run(&mut rig);
    assert_eq!(report.outcome, FrOutcome::Completed);
    assert_eq!((report.passed_over, report.ok, report.last), (3, 3, 6));
    assert_eq!(db.sql("SELECT group_concat(id) FROM (SELECT id FROM t ORDER BY id)"), "1,2,3,9");

    // The start can also be typed on Source.
    rig.click("filerun-again");
    let again = path.clone();
    rig.visual.simulate_path_prompt_response(move |_| Some(vec![again.clone()]));
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    fr_wait(&mut rig, "the file again", |s| s.step == "source" && s.size.is_some() && s.report.is_none());
    di_type(&mut rig, "filerun-start-at", "5");
    fr_to_run_via_settings(&mut rig);
    let report = fr_run(&mut rig);
    // Statements 5 and 6 are in the table already: the first of them is refused, which is the proof that 1 to 4 were left out.
    assert_eq!((report.passed_over, report.ok, report.failed, report.resume_from), (4, 0, 1, Some(5)));

    // A start that is not a number is refused where it is typed.
    rig.click("filerun-again");
    let third = path.clone();
    rig.visual.simulate_path_prompt_response(move |_| Some(vec![third.clone()]));
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    fr_wait(&mut rig, "the file a third time", |s| s.step == "source" && s.size.is_some() && s.report.is_none());
    di_type(&mut rig, "filerun-start-at", "abc");
    rig.click("filerun-next");
    let snapshot = fr_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.step, "source");
    assert!(snapshot.message.is_some_and(|message| message.contains("number")));
}

fn fr_to_run_via_settings(rig: &mut SqlRig<'_>) {
    rig.click("filerun-next");
    rig.wait_rendered("filerun-next");
    rig.click("filerun-next");
}

#[gpui::test]
fn one_transaction_rolls_everything_back_on_a_refusal_and_every_n_keeps_the_batches_it_committed(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = fr_write(&dir, "tx.sql", "CREATE TABLE w(id INTEGER PRIMARY KEY);\nBEGIN;\nINSERT INTO w VALUES (1);\nINSERT INTO w VALUES (2);\nCOMMIT;\nINSERT INTO w VALUES (3);\nINSERT INTO w VALUES (3);\nINSERT INTO w VALUES (5);\n");
    let tables = fr_tables(&rig);

    // One transaction: the table itself is undone (SQLite has transactional DDL); the file's BEGIN and COMMIT are left out.
    fr_open(&mut rig, &tables, &path);
    fr_settings(&mut rig, 1, None, 0);
    let report = fr_run(&mut rig);
    let FrOutcome::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
    assert!(reason.contains("statement 7"), "{reason}");
    assert_eq!((report.ok, report.failed, report.skipped, report.rolled_back, report.resume_from), (4, 1, 2, 4, Some(1)));
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'w'"), "0", "rolled back, DDL included");
    assert!(report.entries.iter().any(|entry| entry.kind == EntryKind::Skipped && entry.message.contains("its own transaction")));
    assert!(rig.visual.debug_bounds("filerun-kept").is_some());
    let tab = fr_tab(&rig.app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(rig.visual);

    // Every 2 statements: [create, insert 1] [insert 2, insert 3] committed; [insert 3 again] refused, nothing to undo.
    let path = fr_write(&dir, "tx2.sql", "CREATE TABLE w(id INTEGER PRIMARY KEY);\nINSERT INTO w VALUES (1);\nINSERT INTO w VALUES (2);\nINSERT INTO w VALUES (3);\nINSERT INTO w VALUES (4);\nINSERT INTO w VALUES (3);\nINSERT INTO w VALUES (6);\n");
    fr_open(&mut rig, &tables, &path);
    fr_settings(&mut rig, 2, Some("2"), 0);
    let report = fr_run(&mut rig);
    assert!(matches!(report.outcome, FrOutcome::Stopped(_)), "{:?}", report.outcome);
    // Batches: [1 create, 2 insert 1] [3, 4] [5 insert 4 ok, 6 refused] -> the third is rolled back.
    assert_eq!((report.ok, report.rolled_back, report.resume_from), (5, 1, Some(5)));
    assert_eq!(db.sql("SELECT group_concat(id) FROM (SELECT id FROM w ORDER BY id)"), "1,2,3", "two batches were committed; the third was rolled back");
    assert!(rig.visual.debug_bounds("filerun-resume").is_some());

    // Resume at 5 with the file fixed: the third batch is run again.
    std::fs::write(&path, "CREATE TABLE w(id INTEGER PRIMARY KEY);\nINSERT INTO w VALUES (1);\nINSERT INTO w VALUES (2);\nINSERT INTO w VALUES (3);\nINSERT INTO w VALUES (4);\nINSERT INTO w VALUES (5);\nINSERT INTO w VALUES (6);\n").unwrap();
    rig.click("filerun-resume");
    let report = fr_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.passed_over, report.ok), (FrOutcome::Completed, 4, 3));
    assert_eq!(db.sql("SELECT group_concat(id) FROM (SELECT id FROM w ORDER BY id)"), "1,2,3,4,5,6");
}

#[gpui::test]
fn cancel_interrupts_the_run_and_a_transaction_rolls_back(cx: &mut TestAppContext) {
    for whole in [false, true] {
        let db = SqliteDb::new("CREATE TABLE big(id INTEGER PRIMARY KEY, v TEXT);");
        let mut rig = oa_rig(cx, &db);
        let dir = tempfile::tempdir().unwrap();
        let path = fr_big_file(&dir, 300_000);
        let tables = fr_tables(&rig);
        fr_open(&mut rig, &tables, &path);
        if whole {
            fr_settings(&mut rig, 1, None, 0);
        } else {
            fr_to_run(&mut rig);
        }
        rig.click("filerun-start");
        fr_wait(&mut rig, "some statements to run", |s| s.progress.is_some_and(|progress| progress.ok > 50));
        assert!(rig.visual.debug_bounds("filerun-progress").is_some() && rig.visual.debug_bounds("filerun-cancel").is_some() && rig.visual.debug_bounds("filerun-progress-bar").is_some());
        rig.click("filerun-cancel");
        fr_wait(&mut rig, "the cancelled run to end", |s| s.report.is_some());
        let report = fr_snapshot(&rig.app, rig.visual).report.unwrap();
        assert_eq!(report.outcome, FrOutcome::Cancelled, "whole: {whole}");
        let stored: u64 = db.sql("SELECT count(*) FROM big").parse().unwrap();
        if whole {
            assert_eq!(stored, 0, "rolled back");
            assert!(report.rolled_back > 50, "{}", report.rolled_back);
            assert_eq!(report.resume_from, Some(1));
        } else {
            assert!(stored > 50 && stored < 300_000, "{stored}");
            assert_eq!(report.rolled_back, 0);
            assert_eq!(report.resume_from, Some(stored + 1), "the cancelled statement did not go in");
        }
        assert_eq!(rig.app.read_with(rig.visual, |this, _| this.diagram.applies.len()), 0, "a run holds no structure-apply slot");
        drop(rig);
    }
}

#[gpui::test]
fn closing_a_running_file_asks_and_cancelling_it_stops_the_run(cx: &mut TestAppContext) {
    let db = SqliteDb::new("CREATE TABLE big(id INTEGER PRIMARY KEY, v TEXT);");
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = fr_big_file(&dir, 300_000);
    let tables = fr_tables(&rig);
    fr_open(&mut rig, &tables, &path);
    fr_to_run(&mut rig);
    rig.click("filerun-start");
    fr_wait(&mut rig, "the run to go", |s| s.progress.is_some_and(|progress| progress.ok > 20));
    let tab = fr_tab(&rig.app, rig.visual);
    assert!(rig.app.read_with(rig.visual, |this, cx| this.file_run_running(tab, cx)));
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.confirm_close_tab(tab, window, cx)));
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("discard-pending-body").is_some(), "closing a running file asks");
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.tab(tab).is_some()), "kept");
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.confirm_close_tab(tab, window, cx)));
    settle(rig.visual);
    rig.click("discard-pending-confirm");
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.tab(tab).is_none() && this.file_runs.is_empty()));
    // The run stops on its own: the count settles below the file's.
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
    assert!(stored < 300_000, "the run was cancelled with the tab: {stored}");
}

#[gpui::test]
fn a_read_only_connection_refuses_and_a_production_one_asks_before_running(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let dir = tempfile::tempdir().unwrap();
    let tables = fr_tables(&rig);
    let path = fr_write(&dir, "one.sql", "CREATE TABLE ran(a INTEGER);\nINSERT INTO ran VALUES (1);\n");

    // Read-only: refused with the reason, no tab, no prompt.
    publish_types(&rig.app, rig.visual, true);
    give_profile(&rig.app, rig.visual, &key, typed(PRODUCTION));
    oa_menu(&mut rig, &tables, "object-menu-run-file");
    assert!(!rig.visual.did_prompt_for_paths(), "nothing is asked of a read-only connection");
    let notice = rig.app.read_with(rig.visual, |this, _| this.sidebar.notice.clone()).expect("the refusal is shown");
    assert!(notice.contains("Running a SQL file is") && notice.contains("read-only"), "{notice}");
    assert!(rig.app.read_with(rig.visual, |this, _| this.file_runs.is_empty()));

    // Production, writable: the wizard opens, Run asks first.
    publish_types(&rig.app, rig.visual, false);
    fr_open(&mut rig, &tables, &path);
    fr_to_run(&mut rig);
    rig.click("filerun-start");
    assert!(rig.visual.debug_bounds("filerun-confirm").is_some(), "Production asks for its confirmation");
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'ran'"), "0", "nothing before the answer");
    assert!(!fr_snapshot(&rig.app, rig.visual).running);
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'ran'"), "0");
    assert!(fr_snapshot(&rig.app, rig.visual).report.is_none(), "declined: nothing ran");
    rig.click("filerun-start");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    fr_wait(&mut rig, "the confirmed run to end", |s| s.report.is_some());
    assert_eq!(db.sql("SELECT count(*) FROM ran"), "1");

    // The connection turns read-only while the wizard is open: Run refuses.
    rig.click("filerun-again");
    let again = path.clone();
    rig.visual.simulate_path_prompt_response(move |_| Some(vec![again.clone()]));
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    fr_wait(&mut rig, "the file again", |s| s.step == "source" && s.size.is_some() && s.report.is_none());
    fr_to_run(&mut rig);
    publish_types(&rig.app, rig.visual, true);
    rig.click("filerun-start");
    let message = fr_snapshot(&rig.app, rig.visual).message.expect("the refusal is shown in the wizard");
    assert!(message.contains("read-only"), "{message}");
    assert_eq!(db.sql("SELECT count(*) FROM ran"), "1");
}

#[gpui::test]
fn encodings_are_chosen_on_source_and_the_text_reaches_the_server_as_it_was_written(cx: &mut TestAppContext) {
    let db = SqliteDb::new("CREATE TABLE words(w TEXT);");
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let tables = fr_tables(&rig);

    // UTF-16 with a byte order mark is recognised on Auto; CRLF line ends and a BOM-less UTF-8 file too.
    let utf16 = dir.path().join("wide.sql");
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend("INSERT INTO words VALUES ('Zo\u{eb} \u{1f600}');\r\nINSERT INTO words VALUES ('b');\r\n".encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&utf16, bytes).unwrap();
    fr_open_path(&mut rig, "main", "main", &utf16);
    assert_eq!(fr_snapshot(&rig.app, rig.visual).first, ["INSERT INTO words VALUES ('Zo\u{eb} \u{1f600}')", "INSERT INTO words VALUES ('b')"]);
    fr_to_run(&mut rig);
    let report = fr_run(&mut rig);
    assert_eq!((report.outcome.clone(), report.ok), (FrOutcome::Completed, 2));
    assert_eq!(db.sql("SELECT group_concat(w, '|') FROM words"), "Zo\u{eb} \u{1f600}|b");
    let tab = fr_tab(&rig.app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(rig.visual);

    // Latin-1: not UTF-8 as it is; chosen in the select (Auto, UTF-8, Latin-1, ...), the sample shows the text.
    db.sql("DELETE FROM words");
    let latin = dir.path().join("latin.sql");
    std::fs::write(&latin, b"INSERT INTO words VALUES ('caf\xe9');\n").unwrap();
    fr_open(&mut rig, &tables, &latin);
    di_choose(&mut rig, "filerun-encoding", 2);
    fr_wait(&mut rig, "the Latin-1 text", |s| s.first == ["INSERT INTO words VALUES ('caf\u{e9}')"]);
    fr_to_run(&mut rig);
    let report = fr_run(&mut rig);
    assert_eq!(report.outcome, FrOutcome::Completed);
    assert_eq!(db.sql("SELECT w FROM words"), "caf\u{e9}");

    // The same bytes read as UTF-8 are refused by the run with the hint (the statement is not text).
    let tab = fr_tab(&rig.app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(rig.visual);
    db.sql("DELETE FROM words");
    fr_open(&mut rig, &tables, &latin);
    fr_to_run(&mut rig);
    let report = fr_run(&mut rig);
    assert!(matches!(report.outcome, FrOutcome::Stopped(_)), "{:?}", report.outcome);
    assert!(report.entries[0].message.contains("not valid UTF-8"), "{:?}", report.entries[0]);
    assert_eq!(db.sql("SELECT count(*) FROM words"), "0");
}

#[gpui::test]
fn a_file_with_no_statement_a_missing_file_and_an_empty_namespace_say_so(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let empty = fr_write(&dir, "empty.sql", "-- nothing here;\n\n");
    fr_open_path(&mut rig, "main", "main", &empty);
    let snapshot = fr_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.size, snapshot.first.len(), snapshot.more), (Some(18), 0, false));
    assert!(rig.visual.debug_bounds("filerun-sample").is_some());
    let tab = fr_tab(&rig.app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(rig.visual);

    fr_open_path(&mut rig, "main", "main", &dir.path().join("missing.sql"));
    let snapshot = fr_snapshot(&rig.app, rig.visual);
    assert!(snapshot.sample_error.is_some_and(|message| message.contains("Could not open")), "the start of the file is read at once");
    assert!(rig.visual.debug_bounds("filerun-source-status").is_some());
    // Next stays off.
    rig.click("filerun-next");
    assert_eq!(fr_snapshot(&rig.app, rig.visual).step, "source");
}

#[gpui::test]
fn the_entry_points_open_the_wizard_on_the_connection_they_are_asked_on(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let rig = oa_rig(cx, &db);
    let key = rig.key.clone();

    // The menu of a namespace, of the Tables group and of a database offers it; a table does not.
    let table = oa_table(&rig, "keep");
    let (tables, database) = (fr_tables(&rig), database_node(&key, "main"));
    for id in [&tables, &database] {
        let index = rig.app.read_with(rig.visual, |this, _| this.navigator.visible_index(id)).expect("a visible row");
        right_click_row(rig.visual, index);
        settle(rig.visual);
        assert!(rig.visual.debug_bounds("object-menu-run-file").is_some(), "{id:?}");
        rig.visual.simulate_keystrokes("escape");
        settle(rig.visual);
    }
    let index = rig.app.read_with(rig.visual, |this, _| this.navigator.visible_index(&table)).unwrap();
    right_click_row(rig.visual, index);
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("object-menu-run-file").is_none(), "a table is not a place to run a file");
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);

    // The File menu's and the palette's command: on the connection of the active tab, no file chosen yet (the prompt is asked).
    rig.app.update(rig.visual, |this, cx| {
        let tab = this.workspace.tabs().iter().find(|tab| tab.kind == TabKind::Query).map(|tab| tab.id).unwrap();
        this.activate_tab_ui(tab, cx);
    });
    settle(rig.visual);
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| crate::app::command_palette::StaticAction::RunSqlFile.run(this, window, cx)));
    settle(rig.visual);
    assert!(rig.visual.did_prompt_for_paths());
    let view = rig.app.read_with(rig.visual, |this, _| this.file_runs.values().next().cloned().expect("the wizard"));
    assert_eq!(view.read_with(rig.visual, |view, _| view.snapshot().target.connection), key);
    rig.visual.simulate_path_prompt_response(|_| None);
    settle(rig.visual);

    // The connection menu's target when the navigator has a database.
    let target = rig.app.read_with(rig.visual, |this, _| this.file_run_target_of_connection(&key));
    assert_eq!((target.connection.as_str(), target.database.as_str()), (key.as_str(), "main"));
}

#[gpui::test]
fn a_select_in_the_file_runs_for_every_row_not_for_the_page_of_one(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    // Row 1000 raises an error (`abs` of the smallest integer): a read that stopped after its first rows would never reach it.
    let path = fr_write(&dir, "rows.sql", "SELECT CASE WHEN x = 1000 THEN abs(-9223372036854775808) ELSE x END FROM (WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 1000) SELECT x FROM c);\nINSERT INTO keep VALUES (8);\n");
    fr_open_path(&mut rig, "main", "main", &path);
    fr_to_run(&mut rig);
    let report = fr_run(&mut rig);
    let FrOutcome::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
    assert!(reason.contains("statement 1") && reason.contains("overflow"), "{reason}");
    assert_eq!(db.sql("SELECT count(*) FROM keep"), "1", "nothing after the refused SELECT ran");
}

#[gpui::test]
fn a_new_tab_follows_the_connection_and_namespace_chosen_in_the_wizard(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let path = fr_write(&dir, "one.sql", "SELECT 1;");
    fr_open_path(&mut rig, "main", "main", &path);
    let tab = fr_tab(&rig.app, rig.visual);
    let (connection, database) = rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.tab(tab).unwrap();
        (tab.connection_id.clone(), tab.database.clone())
    });
    assert_eq!((connection.as_str(), database.as_str()), (rig.key.as_str(), "main"));
    // The wizard's target changes: the tab says where the file will run.
    let target = FrNamespace { connection: rig.key.clone(), database: "other".into(), schema: "s".into() };
    rig.app.update(rig.visual, |this, cx| this.file_run_target_changed(tab, &target, cx));
    let (database, schema) = rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.tab(tab).unwrap();
        (tab.database.clone(), tab.schema.clone())
    });
    assert_eq!((database.as_str(), schema.as_deref()), ("other", Some("s")));
}

#[gpui::test]
fn clickhouse_is_refused_and_a_tab_closed_the_moment_the_run_starts_cancels_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new("CREATE TABLE big(id INTEGER PRIMARY KEY, v TEXT);");
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    // A ClickHouse connection is not a place to run a file.
    rig.app.update(rig.visual, |this, _| {
        if let Some(saved) = this.saved_connections.iter_mut().find(|saved| saved.matches_id(&key)) {
            saved.kind = ConnectionKind::Clickhouse;
        }
    });
    let refused = rig.app.read_with(rig.visual, |this, _| this.file_run_gate(&key).err().map(|error| error.to_string()));
    assert!(refused.is_some_and(|message| message.contains("ClickHouse")));
    rig.app.update(rig.visual, |this, _| {
        if let Some(saved) = this.saved_connections.iter_mut().find(|saved| saved.matches_id(&key)) {
            saved.kind = ConnectionKind::Sqlite;
        }
    });

    // The control of a run is kept when the worker starts: the tab's removal cancels it even before the wizard heard of the run.
    let dir = tempfile::tempdir().unwrap();
    let path = fr_big_file(&dir, 300_000);
    fr_open_path(&mut rig, "main", "main", &path);
    fr_to_run(&mut rig);
    rig.click("filerun-start");
    let tab = fr_tab(&rig.app, rig.visual);
    assert!(rig.app.read_with(rig.visual, |this, cx| this.file_run_running(tab, cx)));
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(rig.visual);
    let mut last = String::new();
    for _ in 0..200 {
        std::thread::sleep(Duration::from_millis(25));
        let now = db.sql("SELECT count(*) FROM big");
        if now == last {
            break;
        }
        last = now;
    }
    assert!(last.parse::<u64>().unwrap() < 300_000, "the run stopped with its tab: {last}");
    assert!(rig.app.read_with(rig.visual, |this, _| this.file_run_controls.is_empty()), "the tab took its control with it");
}

// ---- drag and drop of .sql files, and the editor's Run SQL File button --------------------------------------------------

fn fd_notes(rig: &mut SqlRig<'_>) -> usize {
    rig.visual.update(|window, cx| window.notifications(cx).len())
}

fn fd_wizards(rig: &SqlRig<'_>) -> usize {
    rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().iter().filter(|tab| tab.kind == TabKind::SqlFile).count())
}

fn fd_external(paths: &[&std::path::Path]) -> gpui::ExternalPaths {
    gpui::ExternalPaths(paths.iter().map(|path| path.to_path_buf()).collect())
}

/// The platform's side of a drag from the OS: the files enter the window at `at`, then move to it.
fn fd_enter(rig: &mut SqlRig<'_>, paths: &[&std::path::Path], at: gpui::Point<gpui::Pixels>) {
    rig.visual.simulate_event(gpui::FileDropEvent::Entered { position: at, paths: fd_external(paths) });
    rig.visual.simulate_event(gpui::FileDropEvent::Pending { position: at });
    settle(rig.visual);
}

fn fd_move(rig: &mut SqlRig<'_>, at: gpui::Point<gpui::Pixels>) {
    rig.visual.simulate_event(gpui::FileDropEvent::Pending { position: at });
    settle(rig.visual);
}

fn fd_release(rig: &mut SqlRig<'_>, at: gpui::Point<gpui::Pixels>) {
    rig.visual.simulate_event(gpui::FileDropEvent::Submit { position: at });
    rig.visual.simulate_event(gpui::FileDropEvent::Ended);
    settle(rig.visual);
}

fn fd_leave(rig: &mut SqlRig<'_>) {
    rig.visual.simulate_event(gpui::FileDropEvent::Exited);
    rig.visual.simulate_event(gpui::FileDropEvent::Ended);
    settle(rig.visual);
}

fn fd_row_center(rig: &mut SqlRig<'_>, id: &NavigatorNodeId) -> gpui::Point<gpui::Pixels> {
    let index = rig.app.read_with(rig.visual, |this, _| this.navigator.visible_index(id)).unwrap_or_else(|| panic!("{id:?} is a visible row"));
    let selector: &'static str = Box::leak(format!("sidebar-row-{index}").into_boxed_str());
    rig.visual.debug_bounds(selector).expect("row bounds are measured").center()
}

fn fd_editor_point(rig: &mut SqlRig<'_>) -> gpui::Point<gpui::Pixels> {
    rig.visual.debug_bounds("query-editor-area").expect("the editor area").center()
}

fn fd_highlighted(rig: &mut SqlRig<'_>) -> bool {
    rig.visual.debug_bounds("file-drop-highlight").is_some()
}

fn fd_activate_editor(rig: &mut SqlRig<'_>) {
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| this.activate_tab_ui(tab, cx));
    settle(rig.visual);
}

/// The tabs bound to a file: `(path, connection, database, schema, text)`, sorted by path.
fn fd_file_tabs(rig: &SqlRig<'_>) -> Vec<(std::path::PathBuf, String, String, Option<String>, String)> {
    let mut tabs: Vec<_> = rig.app.read_with(rig.visual, |this, _| {
        this.workspace
            .tabs()
            .iter()
            .filter_map(|tab| tab.file.as_ref().map(|file| (file.path.clone(), tab.connection_id.clone(), tab.database.clone(), tab.schema.clone(), tab.sql.clone())))
            .collect()
    });
    tabs.sort();
    tabs
}

/// Waits for the files of a drop to be read and shown (the read is off the UI thread).
fn fd_wait_file_tabs(rig: &mut SqlRig<'_>, count: usize) {
    let app = rig.app.clone();
    wait_until(rig.visual, "the dropped files to open in editors", |visual| app.read_with(visual, |this, _| this.workspace.tabs().iter().filter(|tab| tab.file.is_some()).count() >= count));
    settle(rig.visual);
}

/// A script over the editor's limit that costs no disk: the start is text, the rest a hole.
fn fd_huge_file(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    let path = fr_write(dir, name, "SELECT 1;\n");
    std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(crate::sql_file::MAX_BYTES + 1).unwrap();
    path
}

#[gpui::test]
fn a_sql_file_dropped_on_the_editor_opens_in_an_editor_on_the_tabs_connection(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let script = "CREATE TABLE dropped(a INTEGER);\nINSERT INTO dropped VALUES (1);\n";
    let path = fr_write(&dir, "dropped.sql", script);
    fd_activate_editor(&mut rig);
    assert!(!fd_highlighted(&mut rig));

    // Dragging a .sql file over the editor highlights it; moving off the editor, or out of the window, clears it.
    let editor = fd_editor_point(&mut rig);
    fd_enter(&mut rig, &[&path], editor);
    assert!(fd_highlighted(&mut rig), "the editor shows it takes the file");
    let key = rig.key.clone();
    let sidebar = fd_row_center(&mut rig, &connection_node(&key));
    fd_move(&mut rig, sidebar);
    assert!(!fd_highlighted(&mut rig), "the pointer left the editor");
    fd_move(&mut rig, editor);
    assert!(fd_highlighted(&mut rig));
    fd_leave(&mut rig);
    assert!(!fd_highlighted(&mut rig), "the drag left the window");

    // Dropping it opens the file in an editor bound to the tab's connection: no wizard, nothing runs.
    fd_enter(&mut rig, &[&path], editor);
    assert!(fd_highlighted(&mut rig));
    fd_release(&mut rig, editor);
    assert!(!fd_highlighted(&mut rig), "the highlight is gone after the drop");
    fd_wait_file_tabs(&mut rig, 1);
    assert_eq!(fd_file_tabs(&rig), [(path.clone(), key.clone(), String::new(), None, script.to_owned())]);
    assert_eq!(fd_wizards(&rig), 0);
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'dropped'"), "0", "opening the file does not run it");
    let (tab, active) = rig.app.read_with(rig.visual, |this, _| (this.workspace.tabs().iter().find(|tab| tab.file.is_some()).unwrap().id, this.workspace.active_tab().unwrap().id));
    assert_eq!((tab, active), (rig.tab, rig.tab), "the empty tab it was dropped on takes the file");

    // The same file dropped again, while another tab is in front: that tab is brought back, no copy is opened.
    let other = rig.app.update(rig.visual, |this, cx| {
        let other = this.workspace.open_query_tab_with_metadata("other", TabMetadata::default());
        this.activate_tab_ui(other, cx);
        other
    });
    settle(rig.visual);
    let editor = fd_editor_point(&mut rig);
    fd_enter(&mut rig, &[&path], editor);
    fd_release(&mut rig, editor);
    rig.visual.run_until_parked();
    settle(rig.visual);
    assert_eq!(fd_file_tabs(&rig).len(), 1, "no second tab for the same file");
    let active = rig.app.read_with(rig.visual, |this, _| this.workspace.active_tab().unwrap().id);
    assert_eq!(active, rig.tab);
    assert_ne!(active, other);
}

#[gpui::test]
fn files_that_are_not_sql_are_refused_with_a_message_and_a_mixed_drop_opens_each_sql_file_in_its_own_tab(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let notes = fr_write(&dir, "notes.txt", "not a script");
    let folder = dir.path().join("archive.sql.d");
    std::fs::create_dir(&folder).unwrap();
    let (one, two) = (fr_write(&dir, "one.sql", "SELECT 1;"), fr_write(&dir, "TWO.SQL", "SELECT 2;\nSELECT 3;\n"));
    fd_activate_editor(&mut rig);
    let editor = fd_editor_point(&mut rig);

    // Nothing to open: no highlight, a notification, no tab, no crash.
    fd_enter(&mut rig, &[&notes, &folder], editor);
    assert!(!fd_highlighted(&mut rig), "a file the editor will not take gets no highlight");
    let before = fd_notes(&mut rig);
    fd_release(&mut rig, editor);
    assert_eq!(fd_notes(&mut rig), before + 1, "the user is told why nothing happened");
    assert!(fd_file_tabs(&rig).is_empty());

    // Two scripts and a text file: one tab per script (the extension in any case), the other file named as skipped.
    fd_enter(&mut rig, &[&one, &notes, &two], editor);
    assert!(fd_highlighted(&mut rig));
    let before = fd_notes(&mut rig);
    fd_release(&mut rig, editor);
    assert_eq!(fd_notes(&mut rig), before + 1, "the skipped file is reported once");
    fd_wait_file_tabs(&mut rig, 2);
    let key = rig.key.clone();
    let tabs = fd_file_tabs(&rig);
    assert_eq!(tabs.iter().map(|tab| (tab.0.clone(), tab.1.clone(), tab.4.clone())).collect::<Vec<_>>(), [(two, key.clone(), "SELECT 2;\nSELECT 3;\n".to_owned()), (one, key, "SELECT 1;".to_owned())]);
    assert_eq!(fd_wizards(&rig), 0);
}

#[gpui::test]
fn a_sql_file_dropped_on_a_navigator_row_opens_in_an_editor_on_that_row_and_the_row_shows_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let (on_connection, on_tables, on_table) = (fr_write(&dir, "c.sql", "SELECT 1;\n"), fr_write(&dir, "t.sql", "SELECT 2;\n"), fr_write(&dir, "o.sql", "SELECT 3;\n"));
    let (key, tables, table) = (rig.key.clone(), fr_tables(&rig), oa_table(&rig, "keep"));

    // The connection row: the row gets the drop border while the file is over it, and not once it left.
    let connection = fd_row_center(&mut rig, &connection_node(&key));
    fd_enter(&mut rig, &[&on_connection], connection);
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.navigator_ui.drop_target.clone()), Some(connection_node(&key)));
    let tables_at = fd_row_center(&mut rig, &tables);
    fd_move(&mut rig, tables_at);
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.navigator_ui.drop_target.clone()), Some(tables.clone()));
    let editor_at = fd_editor_point(&mut rig);
    fd_move(&mut rig, editor_at);
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.navigator_ui.drop_target.clone()), None, "the pointer left the navigator");
    fd_move(&mut rig, connection);
    fd_leave(&mut rig);
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.navigator_ui.drop_target.clone()), None, "the drag left the window");

    // Dropped on the connection: an editor on its first database; on the Tables group: on that schema; on a table: the same schema.
    fd_enter(&mut rig, &[&on_connection], connection);
    fd_release(&mut rig, connection);
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.navigator_ui.drop_target.clone()), None);
    fd_wait_file_tabs(&mut rig, 1);
    let tables_row = fd_row_center(&mut rig, &tables);
    fd_enter(&mut rig, &[&on_tables], tables_row);
    fd_release(&mut rig, tables_row);
    fd_wait_file_tabs(&mut rig, 2);
    let table_row = fd_row_center(&mut rig, &table);
    fd_enter(&mut rig, &[&on_table], table_row);
    fd_release(&mut rig, table_row);
    fd_wait_file_tabs(&mut rig, 3);
    let bound: Vec<_> = fd_file_tabs(&rig).into_iter().map(|tab| (tab.1, tab.2, tab.3)).collect();
    // Sorted by path: c.sql, o.sql, t.sql. (The schema of a connection's first database is its default.)
    assert_eq!(bound[0].0, key);
    assert_eq!(bound[0].1, "main");
    assert_eq!(bound[1], (key.clone(), "main".to_owned(), Some("main".to_owned())));
    assert_eq!(bound[2], (key.clone(), "main".to_owned(), Some("main".to_owned())));
    assert_eq!(fd_wizards(&rig), 0);

    // Below the last row there is nothing to open on: a message, no tab.
    let rows = rig.app.read_with(rig.visual, |this, _| this.navigator.visible_row_count());
    let last_id = rig.app.read_with(rig.visual, |this, _| this.navigator.visible_row_at(rows - 1).unwrap().id);
    let last = fd_row_center(&mut rig, &last_id);
    let below = gpui::point(last.x, last.y + gpui::px(theme::SIDEBAR_ROW_HEIGHT * 2.0));
    let before = fd_notes(&mut rig);
    fd_enter(&mut rig, &[&on_connection], below);
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.navigator_ui.drop_target.clone()), None, "no row, no border");
    fd_release(&mut rig, below);
    assert_eq!(fd_notes(&mut rig), before + 1);
    assert_eq!(fd_file_tabs(&rig).len(), 3, "nothing opened below the rows");
}

#[gpui::test]
fn a_file_over_the_editors_limit_goes_to_the_wizard_and_a_connection_that_cannot_run_it_says_so_once(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let (huge, small) = (fd_huge_file(&dir, "huge.sql"), fr_write(&dir, "small.sql", "SELECT 2;\n"));
    fd_activate_editor(&mut rig);
    let editor = fd_editor_point(&mut rig);

    // The small file opens in an editor, the huge one in the Run SQL File wizard on the tab's connection; neither runs.
    fd_enter(&mut rig, &[&huge, &small], editor);
    fd_release(&mut rig, editor);
    fd_wait_file_tabs(&mut rig, 1);
    let app = rig.app.clone();
    wait_until(rig.visual, "the wizard for the file over the limit", |visual| app.read_with(visual, |this, _| !this.file_runs.is_empty()));
    settle(rig.visual);
    fr_wait_sample(&mut rig);
    let tabs = fd_file_tabs(&rig);
    assert_eq!(tabs.iter().map(|tab| tab.0.clone()).collect::<Vec<_>>(), [small]);
    assert_eq!(fd_wizards(&rig), 1);
    let snapshot = fr_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.target.connection.as_str(), snapshot.target.database.as_str(), snapshot.size), (rig.key.as_str(), "main", Some(crate::sql_file::MAX_BYTES + 1)));
    assert_eq!(snapshot.first[0], "SELECT 1");

    // On a connection no file can run on: one message for the files over the limit, not a wizard.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        if let Some(saved) = this.saved_connections.iter_mut().find(|saved| saved.matches_id(&key)) {
            saved.kind = ConnectionKind::Clickhouse;
        }
    });
    let wizard = fr_tab(&rig.app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[wizard], None, cx));
    fd_activate_editor(&mut rig);
    let again = fd_huge_file(&dir, "again.sql");
    let editor = fd_editor_point(&mut rig);
    let before = fd_notes(&mut rig);
    fd_enter(&mut rig, &[&again], editor);
    fd_release(&mut rig, editor);
    wait_until(rig.visual, "the refusal of the file over the limit", |visual| visual.update(|window, cx| window.notifications(cx).len()) > before);
    settle(rig.visual);
    assert_eq!(fd_notes(&mut rig), before + 1);
    assert_eq!(fd_wizards(&rig), 0);
}

/// The controls of the editor's header, left to right, as measured.
fn fd_header_controls(rig: &mut SqlRig<'_>, selectors: &[&'static str]) -> Vec<gpui::Bounds<gpui::Pixels>> {
    selectors.iter().map(|selector| rig.visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"))).collect()
}

fn fd_assert_header_fits(rig: &mut SqlRig<'_>, selectors: &[&'static str], right_edge: gpui::Pixels) {
    let bounds = fd_header_controls(rig, selectors);
    for (selector, bound) in selectors.iter().zip(&bounds) {
        assert!(bound.size.width > gpui::px(0.0) && bound.right() <= right_edge, "{selector} fits: {bound:?}");
    }
    for pair in bounds.windows(2).zip(selectors.windows(2)) {
        assert!(pair.0[0].right() <= pair.0[1].left() + gpui::px(0.5), "{:?} do not overlap: {:?}", pair.1, pair.0);
    }
}

#[gpui::test]
fn the_editor_header_offers_run_sql_file_without_crowding_run_save_history_and_the_transaction_controls(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FR_SEED);
    let mut rig = oa_rig(cx, &db);
    let dir = tempfile::tempdir().unwrap();
    let script = "SELECT 1;\nSELECT 2;\n";
    let path = fr_write(&dir, "toolbar.sql", script);
    fd_activate_editor(&mut rig);
    let controls = ["tx-mode-auto", "tx-mode-manual", "run-query", "query-history", "run-sql-file", "save-sql-script"];
    let window_width = gpui::px(1100.0);
    fd_assert_header_fits(&mut rig, &controls, window_width);
    // Manual mode adds the transaction's state beside the mode; with a transaction open the Commit and Roll Back buttons are
    // there too. The header still holds everything whole (the status text is what gives way), in a narrower one too.
    rig.click("tx-mode-manual");
    fd_assert_header_fits(&mut rig, &controls, window_width);
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| {
        this.transactions.set_for_capture(tab, open_tx_state());
        cx.notify();
    });
    settle(rig.visual);
    let open = ["tx-mode-auto", "tx-mode-manual", "tx-status", "tx-commit", "tx-rollback", "run-query", "query-history", "run-sql-file", "save-sql-script"];
    fd_assert_header_fits(&mut rig, &open, window_width);
    rig.visual.simulate_resize(gpui::size(gpui::px(980.0), gpui::px(850.0)));
    settle(rig.visual);
    fd_assert_header_fits(&mut rig, &["tx-commit", "tx-rollback", "run-query", "query-history", "run-sql-file", "save-sql-script"], gpui::px(980.0));

    // Clicking it asks for the file on the tab's connection; the answer opens the wizard on it.
    rig.click("run-sql-file");
    assert!(rig.visual.did_prompt_for_paths(), "the button asks the platform for a file");
    rig.visual.simulate_path_prompt_response(|options| {
        assert!(options.files && !options.multiple);
        Some(vec![path.clone()])
    });
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(rig.visual);
    fr_wait_sample(&mut rig);
    let snapshot = fr_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.target.connection.as_str(), snapshot.target.database.as_str(), snapshot.size), (rig.key.as_str(), "main", Some(script.len() as u64)));
}

// ---- performance (ignored): `TABLEX_PERF_SQL_ROWS` single-row INSERT statements (default 1,000,000) ---------------------

fn fr_perf_rows() -> usize {
    std::env::var("TABLEX_PERF_SQL_ROWS").ok().and_then(|rows| rows.parse().ok()).unwrap_or(1_000_000)
}

/// Writes `statements` statements of `table` into `path`: `rows_per` rows in each INSERT (1: one statement per row).
fn fr_perf_file(path: &std::path::Path, table: &str, statements: usize, rows_per: usize) -> u64 {
    use std::io::Write as _;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path).unwrap());
    let mut id = 0usize;
    for _ in 0..statements {
        write!(out, "INSERT INTO {table} (id, name, email, qty, note) VALUES ").unwrap();
        for row in 0..rows_per {
            id += 1;
            if row > 0 {
                out.write_all(b", ").unwrap();
            }
            write!(out, "({id}, 'name {id}', 'user{id}@example.com', {}, 'a note; with ''quotes'' for row {id}')", id % 1000).unwrap();
        }
        out.write_all(b";\n").unwrap();
    }
    out.flush().unwrap();
    std::fs::metadata(path).unwrap().len()
}

/// Runs `path` through the wizard (`transactions` row: 0 autocommit, 1 whole file, 2 every `every`) and prints statements/s, MB/s and RSS.
fn fr_perf_run(rig: &mut SqlRig<'_>, label: &str, target: (&str, &str), path: &std::path::Path, transactions: usize, every: u64) -> FrReport {
    fr_open_path(rig, target.0, target.1, path);
    let view = rig.app.read_with(rig.visual, |this, _| this.file_runs.values().next().cloned().expect("the wizard"));
    let app = rig.app.clone();
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
    rig.visual.update(|window, cx| view.update(cx, |view, cx| view.start_for_test(transactions, every, window, cx)));
    loop {
        rig.visual.run_until_parked();
        if fr_snapshot(&app, rig.visual).report.is_some() {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(4 * 3600), "the run did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    let elapsed = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    sampler.join().unwrap();
    let report = fr_snapshot(&app, rig.visual).report.unwrap();
    let peak = peak.load(std::sync::atomic::Ordering::Relaxed);
    let size = std::fs::metadata(path).unwrap().len();
    eprintln!(
        "PERF {label}: {} statements ({:.1} MB), {}: {:.1} s, {:.0} statements/s, {:.1} MB/s; RSS {:.0} MB before, peak {:.0} MB (+{:.0} MB); {:?}",
        report.ok + report.failed,
        size as f64 / 1e6,
        ["autocommit", "one transaction", "commit every N"][transactions],
        elapsed.as_secs_f64(),
        (report.ok + report.failed) as f64 / elapsed.as_secs_f64(),
        size as f64 / 1e6 / elapsed.as_secs_f64(),
        baseline as f64 / 1024.0,
        peak as f64 / 1024.0,
        (peak - baseline) as f64 / 1024.0,
        report.outcome
    );
    let tab = fr_tab(&app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(rig.visual);
    report
}

#[gpui::test]
#[ignore = "generates a large file; prints statements/s and peak RSS"]
fn perf_run_sql_file_on_sqlite(cx: &mut TestAppContext) {
    let db = SqliteDb::new("CREATE TABLE perf(id INTEGER PRIMARY KEY, name TEXT, email TEXT, qty INTEGER, note TEXT);");
    let mut rig = nav_rig_sqlite(cx, &db);
    let dir = std::env::var("TABLEX_PERF_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let path = dir.join(format!("tablex-r5-{}.sql", uuid::Uuid::new_v4()));
    let rows = fr_perf_rows();
    let rows_per: usize = std::env::var("TABLEX_PERF_SQL_ROWS_PER").ok().and_then(|rows| rows.parse().ok()).unwrap_or(1);
    let statements = rows / rows_per;
    let size = fr_perf_file(&path, "perf", statements, rows_per);
    eprintln!("generated {} ({:.1} MB, {statements} statements of {rows_per} rows)", path.display(), size as f64 / 1e6);
    let _remove = scopeguard_remove(path.clone());
    let report = fr_perf_run(&mut rig, "SQLite", ("main", "main"), &path, 2, 10_000);
    assert_eq!((report.outcome.clone(), report.ok), (FrOutcome::Completed, statements as u64), "{:?}", report.entries.first());
    assert_eq!(db.sql("SELECT count(*) FROM perf"), rows.to_string());
}

/// Removes `path` when dropped.
struct RemoveFile(std::path::PathBuf);

impl Drop for RemoveFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn scopeguard_remove(path: std::path::PathBuf) -> RemoveFile {
    RemoveFile(path)
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI; prints statements/s and peak RSS"]
fn perf_run_sql_file_on_postgresql(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let table = format!("cf_r5_perf_{}", &uuid::Uuid::new_v4().to_simple().to_string()[..8]);
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP TABLE IF EXISTS public.{table}")]);
    run_confirmed(&mut rig, &format!("CREATE TABLE public.{table}(id bigint PRIMARY KEY, name text, email text, qty int, note text)"));
    let dir = std::env::var("TABLEX_PERF_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let path = dir.join(format!("tablex-r5-{}.sql", uuid::Uuid::new_v4()));
    let rows = fr_perf_rows();
    let rows_per: usize = std::env::var("TABLEX_PERF_SQL_ROWS_PER").ok().and_then(|rows| rows.parse().ok()).unwrap_or(1);
    let statements = rows / rows_per;
    fr_perf_file(&path, &format!("public.{table}"), statements, rows_per);
    let _remove = scopeguard_remove(path.clone());
    for (transactions, every) in [(2usize, 10_000u64), (0, 1)] {
        exec_confirmed(&rig.session, &format!("TRUNCATE public.{table}"));
        // Autocommit is one round trip and one commit per statement: a tenth of the statements measure it.
        let file = if transactions == 0 {
            let small = dir.join(format!("tablex-r5-small-{}.sql", uuid::Uuid::new_v4()));
            fr_perf_file(&small, &format!("public.{table}"), (statements / 10).max(1), rows_per);
            small
        } else {
            path.clone()
        };
        let _small = scopeguard_remove(file.clone());
        let expected = if transactions == 0 { (statements / 10).max(1) } else { statements } * rows_per;
        let report = fr_perf_run(&mut rig, "PostgreSQL", ("tablex", "public"), &file, transactions, every);
        assert_eq!(report.outcome, FrOutcome::Completed, "{:?}", report.entries.first());
        assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM public.{table}")), expected.to_string());
    }
}

// ---- live: PostgreSQL, MySQL and MariaDB ------------------------------------------------------------------------------------

/// Runs `script` through the wizard in the mode chosen (`transactions`: 0 autocommit, 1 whole file, 2 every `every`; `on_error`: 0 stop, 1 continue).
fn fr_live_run(rig: &mut SqlRig<'_>, database: &str, schema: &str, script: &str, transactions: i32, every: Option<&str>, on_error: i32) -> FrReport {
    let dir = tempfile::tempdir().unwrap();
    let path = fr_write(&dir, "live.sql", script);
    fr_open_path(rig, database, schema, &path);
    fr_settings(rig, transactions, every, on_error);
    let report = fr_run(rig);
    let tab = fr_tab(&rig.app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(rig.visual);
    report
}

fn fr_live_names() -> (String, String, String) {
    let suffix = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    (format!("cf_r5_x_{suffix}"), format!("cf_r5_f_{suffix}"), suffix)
}

fn live_pg_file(table: &str, function: &str) -> String {
    format!(
        "-- R5 live file\nSET client_min_messages = warning;\nDROP TABLE IF EXISTS public.{table};\nCREATE TABLE public.{table} (id int PRIMARY KEY, note text);\n\
         INSERT INTO public.{table} VALUES (1, 'a;b'), (2, $q$c;'d$q$);\n\
         CREATE OR REPLACE FUNCTION public.{function}(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a + 1; END; $$;\n\
         INSERT INTO public.{table} VALUES (3, public.{function}(2)::text);\nSELECT count(*) FROM public.{table};\n\
         INSERT INTO public.{table} VALUES (1, 'dup');\nINSERT INTO public.{table} VALUES (4, 'after');\n\\echo done\n"
    )
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_sql_file_runs_in_every_mode(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI to the dedicated tablex-test-pg container");
        return;
    };
    let (table, function, _) = fr_live_names();
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP TABLE IF EXISTS public.{table}"), format!("DROP FUNCTION IF EXISTS public.{function}(int)")]);
    let script = live_pg_file(&table, &function);
    let rows = |rig: &mut SqlRig<'_>| oa_scalar(rig, &format!("SELECT coalesce(string_agg(id::text, ',' ORDER BY id), '') FROM public.{table}"));
    let exists = |rig: &mut SqlRig<'_>| oa_scalar(rig, &format!("SELECT count(*) FROM pg_class WHERE relname = '{table}'"));

    // Autocommit, stop: the dollar-quoted function and the multi-row insert ran; the duplicate stopped it; the \echo was never reached.
    let report = fr_live_run(&mut rig, "tablex", "public", &script, 0, None, 0);
    let FrOutcome::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
    assert!(reason.contains("statement 8") && reason.contains("duplicate key"), "{reason}");
    assert_eq!((report.ok, report.failed, report.resume_from, report.result_sets), (7, 1, Some(8), 1));
    assert_eq!(rows(&mut rig), "1,2,3");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT public.{function}(41)")), "42", "the function exists");

    // Autocommit, continue: the duplicate is logged, the \echo line is skipped, the insert after it ran.
    let report = fr_live_run(&mut rig, "tablex", "public", &script, 0, None, 1);
    assert_eq!(report.outcome, FrOutcome::Completed);
    assert_eq!((report.ok, report.failed, report.skipped, report.last), (8, 1, 1, 10));
    assert_eq!(report.entries.iter().map(|entry| (entry.number, entry.kind)).collect::<Vec<_>>(), [(8, EntryKind::Failed), (10, EntryKind::Skipped)]);
    assert!(report.entries[0].message.contains("duplicate key"), "{:?}", report.entries[0]);
    assert_eq!(rows(&mut rig), "1,2,3,4");

    // One transaction: PostgreSQL's DDL is transactional, so the rollback takes the table and the function with it.
    run_confirmed(&mut rig, &format!("DROP TABLE public.{table}"));
    run_confirmed(&mut rig, &format!("DROP FUNCTION public.{function}(int)"));
    let report = fr_live_run(&mut rig, "tablex", "public", &script, 1, None, 0);
    assert!(matches!(report.outcome, FrOutcome::Stopped(_)), "{:?}", report.outcome);
    assert_eq!((report.ok, report.rolled_back, report.resume_from), (7, 7, Some(1)));
    assert_eq!(exists(&mut rig), "0", "nothing was kept");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM pg_proc WHERE proname = '{function}'")), "0");

    // A commit every 4 statements: [SET, DROP, CREATE, INSERT] committed, [function, insert 3, select, dup]: the dup is refused and that batch rolled back.
    let report = fr_live_run(&mut rig, "tablex", "public", &script, 2, Some("4"), 0);
    assert!(matches!(report.outcome, FrOutcome::Stopped(_)), "{:?}", report.outcome);
    assert_eq!((report.ok, report.rolled_back, report.resume_from), (7, 3, Some(5)));
    assert_eq!(rows(&mut rig), "1,2", "the first batch stands");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM pg_proc WHERE proname = '{function}'")), "0", "the function was in the rolled back batch");

    // Resume at 5 with the duplicate gone: the rest runs.
    let fixed = script.replace("(1, 'dup')", "(9, 'fixed')");
    let report = fr_live_run_from(&mut rig, "tablex", "public", &fixed, 5);
    // The SET of statement 1 is run again (it sets the session); the table, the rows and the function are not.
    assert_eq!((report.outcome.clone(), report.passed_over, report.replayed), (FrOutcome::Completed, 3, 1), "{:?}", report.entries);
    assert_eq!(rows(&mut rig), "1,2,3,4,9");
    drop(rig);
}

/// Like [`fr_live_run`], autocommit and stop, starting at statement `start`.
fn fr_live_run_from(rig: &mut SqlRig<'_>, database: &str, schema: &str, script: &str, start: u64) -> FrReport {
    let dir = tempfile::tempdir().unwrap();
    let path = fr_write(&dir, "live.sql", script);
    fr_open_path(rig, database, schema, &path);
    di_type(rig, "filerun-start-at", &start.to_string());
    fr_to_run_via_settings(rig);
    let report = fr_run(rig);
    let tab = fr_tab(&rig.app, rig.visual);
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(rig.visual);
    report
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_psql_lines_and_copy_blocks_are_reported_and_connect_switches_database(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI to the dedicated tablex-test-pg container");
        return;
    };
    let (table, _, _) = fr_live_names();
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP TABLE IF EXISTS public.{table}")]);
    let script = format!(
        "\\connect tablex\n\\set ON_ERROR_STOP on\nCREATE TABLE public.{table} (id int, v text);\nCOPY public.{table} (id, v) FROM stdin;\n1\tone\n2\ttwo;\n\\.\nINSERT INTO public.{table} VALUES (3, 'three');\n\\connect tablex_no_such_database_r5\nINSERT INTO public.{table} VALUES (4, 'never');\n"
    );
    let report = fr_live_run(&mut rig, "tablex", "public", &script, 0, None, 0);
    // \connect to a database that does not exist stops the run; the COPY block and \set are listed, none of it sent.
    let FrOutcome::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
    assert!(reason.contains("statement 6") && reason.contains("could not connect"), "{reason}");
    assert_eq!((report.ok, report.skipped, report.failed), (3, 2, 1));
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM public.{table}")), "1", "only the INSERT ran; the COPY data lines were not run");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT string_agg(v, ',') FROM public.{table}")), "three");
}

fn live_mysql_file(table: &str, procedure: &str) -> String {
    format!(
        "-- R5 live file\nDROP TABLE IF EXISTS {table};\nCREATE TABLE {table} (id INT PRIMARY KEY, note VARCHAR(100)) ENGINE=InnoDB;\n\
         INSERT INTO {table} VALUES (1, 'a;b'), (2, \"c;'d\");\nDROP PROCEDURE IF EXISTS {procedure};\nDELIMITER //\n\
         CREATE PROCEDURE {procedure}(IN n INT)\nBEGIN\n  INSERT INTO {table} VALUES (n, 'from procedure; with a semicolon');\n  SELECT COUNT(*) FROM {table};\nEND//\nDELIMITER ;\n\
         CALL {procedure}(3);\nINSERT INTO {table} VALUES (1, 'dup');\nINSERT INTO {table} VALUES (4, 'after');\n"
    )
}

/// DDL, DML and a deliberate error in every mode on MySQL or MariaDB, and a `DELIMITER` procedure.
fn live_mysql_family(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let (table, procedure, _) = fr_live_names();
    let mut rig = nav_rig(cx, kind, driver, &uri);
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP TABLE IF EXISTS {database}.{table}"), format!("DROP PROCEDURE IF EXISTS {database}.{procedure}")]);
    let script = live_mysql_file(&table, &procedure);
    let rows = |rig: &mut SqlRig<'_>| oa_scalar(rig, &format!("SELECT COALESCE(GROUP_CONCAT(id ORDER BY id), '') FROM {database}.{table}"));

    // Autocommit, stop: the procedure body kept its semicolons (DELIMITER), the CALL ran, the duplicate stopped the run.
    let report = fr_live_run(&mut rig, &database, &database, &script, 0, None, 0);
    let FrOutcome::Stopped(reason) = &report.outcome else { panic!("stopped, got {:?}", report.outcome) };
    assert!(reason.contains("statement 7") && reason.contains("Duplicate entry"), "{reason}");
    assert_eq!((report.ok, report.failed, report.resume_from, report.result_sets), (6, 1, Some(7), 1));
    assert_eq!(rows(&mut rig), "1,2,3");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT COUNT(*) FROM information_schema.routines WHERE routine_name = '{procedure}'")), "1");

    // Autocommit, continue.
    let report = fr_live_run(&mut rig, &database, &database, &script, 0, None, 1);
    assert_eq!((report.outcome.clone(), report.ok, report.failed, report.last), (FrOutcome::Completed, 7, 1, 8));
    assert_eq!(rows(&mut rig), "1,2,3,4");

    // One transaction: the DDL commits by itself, so the run's transactions are the stretches between DDL statements. The last one
    // ([CALL, dup]) is rolled back: the CALL's row is undone, everything before the procedure stands.
    let report = fr_live_run(&mut rig, &database, &database, &script, 1, None, 0);
    assert!(matches!(report.outcome, FrOutcome::Stopped(_)), "{:?}", report.outcome);
    assert_eq!((report.ok, report.rolled_back, report.resume_from), (6, 1, Some(6)), "{:?}", report.entries);
    assert_eq!(rows(&mut rig), "1,2", "the insert before the procedure was committed by the DDL after it; the CALL was rolled back");

    // A commit every 2 statements.
    let report = fr_live_run(&mut rig, &database, &database, &script, 2, Some("2"), 0);
    assert!(matches!(report.outcome, FrOutcome::Stopped(_)), "{:?}", report.outcome);
    assert_eq!(rows(&mut rig), "1,2", "{report:?}");

    // The note about implicit commits is on the Settings step.
    let dir = tempfile::tempdir().unwrap();
    let path = fr_write(&dir, "note.sql", "SELECT 1;");
    fr_open_path(&mut rig, &database, &database, &path);
    rig.click("filerun-next");
    di_choose(&mut rig, "filerun-transactions", 1);
    assert!(rig.visual.debug_bounds("filerun-ddl-note").is_some(), "MySQL and MariaDB: DDL cannot be rolled back whole");
    drop(rig);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_sql_file_runs_in_every_mode(cx: &mut TestAppContext) {
    live_mysql_family(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_sql_file_runs_in_every_mode(cx: &mut TestAppContext) {
    live_mysql_family(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

// ---- live: what the review of R5 found (session state, run to the end, search path) ----------------------------------------

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_a_select_runs_for_every_row_and_the_schema_goes_first_on_the_search_path(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI to the dedicated tablex-test-pg container");
        return;
    };
    let suffix = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let (schema, public_table, public_function) = (format!("cf_r5_s_{suffix}"), format!("cf_r5_pub_{suffix}"), format!("cf_r5_pubf_{suffix}"));
    let (log_table, log_function, source) = (format!("cf_r5_log_{suffix}"), format!("cf_r5_logf_{suffix}"), format!("cf_r5_src_{suffix}"));
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let _cleanup = OaCleanup(
        rig.session.clone(),
        vec![
            format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
            format!("DROP TABLE IF EXISTS public.{public_table}"),
            format!("DROP FUNCTION IF EXISTS public.{public_function}()"),
            format!("DROP FUNCTION IF EXISTS public.{log_function}(int)"),
            format!("DROP TABLE IF EXISTS public.{log_table}"),
            format!("DROP TABLE IF EXISTS public.{source}"),
        ],
    );
    for sql in [
        format!("CREATE SCHEMA {schema}"),
        format!("CREATE TABLE public.{public_table}(a int)"),
        format!("INSERT INTO public.{public_table} VALUES (5)"),
        format!("CREATE FUNCTION public.{public_function}() RETURNS int LANGUAGE sql AS 'SELECT 41'"),
    ] {
        exec_confirmed(&rig.session, &sql);
    }
    // The run's schema is first on the search path; public (where the table and the function are) is still on it.
    let script = format!(
        "CREATE TABLE local_t(a int);\nINSERT INTO local_t SELECT a + {public_function}() FROM {public_table};\n\
         CREATE TABLE public.{log_table}(n int);\n\
         CREATE FUNCTION public.{log_function}(i int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO public.{log_table} VALUES (i); RETURN i; END; $$;\n\
         CREATE TABLE public.{source} AS SELECT generate_series(1, 50) AS id;\n\
         SELECT public.{log_function}(id) FROM public.{source};\n\
         DO $$ BEGIN IF (SELECT a FROM local_t) <> 46 OR (SELECT count(*) FROM public.{log_table}) <> 50 THEN RAISE EXCEPTION 'wrong'; END IF; END $$;\n"
    );
    let report = fr_live_run(&mut rig, "tablex", &schema, &script, 0, None, 0);
    assert_eq!((report.outcome.clone(), report.ok), (FrOutcome::Completed, 7), "{:?}", report.entries);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM public.{log_table}")), "50", "the function ran for every row of the SELECT, not for a page of it");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relname = 'local_t' AND n.nspname = '{schema}'")), schema, "the table went to the chosen schema");
    assert_eq!(oa_scalar(&mut rig, "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relname = 'local_t' AND n.nspname = 'public'"), "0");
    drop(rig);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_a_resumed_run_connects_where_the_file_connected(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI to the dedicated tablex-test-pg container");
        return;
    };
    let suffix = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let (second, table) = (format!("cf_r5_db2_{suffix}"), format!("cf_r5_t_{suffix}"));
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP DATABASE IF EXISTS {second} WITH (FORCE)")]);
    exec_confirmed(&rig.session, &format!("CREATE DATABASE {second}"));
    let head = format!("\\connect {second}\nCREATE TABLE public.{table} (id int PRIMARY KEY);\nINSERT INTO public.{table} VALUES (1);\nINSERT INTO public.{table} VALUES (2);\n");
    let check = format!("DO $$ BEGIN IF current_database() <> '{second}' OR (SELECT count(*) FROM public.{table}) <> 3 THEN RAISE EXCEPTION 'wrong place'; END IF; END $$;\n");
    // The first run stops at the duplicate (statement 5), in the other database.
    let report = fr_live_run(&mut rig, "tablex", "public", &format!("{head}INSERT INTO public.{table} VALUES (2);\n{check}"), 0, None, 0);
    assert!(matches!(report.outcome, FrOutcome::Stopped(_)), "{:?}", report.outcome);
    assert_eq!((report.ok, report.resume_from), (4, Some(5)));
    // The file is fixed; the run goes on from 5 and the \connect before it is run again: the row goes where the first four went.
    let report = fr_live_run_from(&mut rig, "tablex", "public", &format!("{head}INSERT INTO public.{table} VALUES (3);\n{check}"), 5);
    assert_eq!((report.outcome.clone(), report.passed_over, report.replayed, report.ok), (FrOutcome::Completed, 3, 1, 2), "{:?}", report.entries);
    assert_eq!(report.entries.iter().filter(|entry| entry.kind == EntryKind::Replayed).count(), 1, "the summary lists what was run again");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM pg_class WHERE relname = '{table}'")), "0", "nothing landed in the database the run was started on");
    drop(rig);
}

/// MySQL and MariaDB: `USE` in a dump, a commit hidden in a versioned comment, a start that needs the session set up again, and a SELECT
/// that must run for every row.
fn live_mysql_family_sessions(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let target = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let suffix = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let (other, dumped, whole, fifty, check) = (format!("cf_r5_b_{suffix}"), "t".to_owned(), format!("cf_r5_w_{suffix}"), format!("cf_r5_f_{suffix}"), format!("cf_r5_c_{suffix}"));
    let (thousand, no_autocommit) = (format!("cf_r5_k_{suffix}"), format!("cf_r5_n_{suffix}"));
    let mut rig = nav_rig(cx, kind, driver, &uri);
    let _cleanup = OaCleanup(
        rig.session.clone(),
        vec![format!("DROP DATABASE IF EXISTS {other}"), format!("DROP TABLE IF EXISTS {target}.{whole}"), format!("DROP TABLE IF EXISTS {target}.{fifty}"), format!("DROP TABLE IF EXISTS {target}.{check}"), format!("DROP TABLE IF EXISTS {target}.cf_r5_t_{suffix}"), format!("DROP TABLE IF EXISTS {target}.{thousand}"), format!("DROP TABLE IF EXISTS {target}.{no_autocommit}")],
    );
    let scalar = |rig: &mut SqlRig<'_>, sql: &str| oa_scalar(rig, sql);

    // B1: a `mysqldump --databases` file creates a database and USEs it: the table goes there, not to the database the run started on.
    let dump = format!(
        "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;\nDROP DATABASE IF EXISTS `{other}`;\nCREATE DATABASE /*!32312 IF NOT EXISTS*/ `{other}` /*!40100 DEFAULT CHARACTER SET utf8mb4 */;\nUSE `{other}`;\n\
         DROP TABLE IF EXISTS `{dumped}`;\nCREATE TABLE `{dumped}` (`id` int NOT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB;\nLOCK TABLES `{dumped}` WRITE;\n/*!40000 ALTER TABLE `{dumped}` DISABLE KEYS */;\n\
         INSERT INTO `{dumped}` VALUES (1),(2);\n/*!40000 ALTER TABLE `{dumped}` ENABLE KEYS */;\nUNLOCK TABLES;\n"
    );
    let report = fr_live_run(&mut rig, &target, &target, &dump, 0, None, 0);
    assert_eq!((report.outcome.clone(), report.ok, report.failed), (FrOutcome::Completed, 11, 0), "{:?}", report.entries);
    assert_eq!(scalar(&mut rig, &format!("SELECT COUNT(*) FROM {other}.{dumped}")), "2", "the table is in the database the file USEd");
    assert_eq!(scalar(&mut rig, &format!("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '{target}' AND table_name = '{dumped}'")), "0", "and not in the one the run started on");

    // B2: a dump in one transaction. Both ALTER ... KEYS statements commit by themselves, in their versioned comments: the rollback
    // after the refusal undoes only the insert after them, and the report says so.
    let in_one = format!(
        "/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE */;\nDROP TABLE IF EXISTS {whole};\nCREATE TABLE {whole} (id INT PRIMARY KEY) ENGINE=InnoDB;\nLOCK TABLES {whole} WRITE;\n/*!40000 ALTER TABLE {whole} DISABLE KEYS */;\n\
         INSERT INTO {whole} VALUES (1),(2);\n/*!40000 ALTER TABLE {whole} ENABLE KEYS */;\nUNLOCK TABLES;\nINSERT INTO {whole} VALUES (3);\nINSERT INTO {whole} VALUES (3);\n"
    );
    let report = fr_live_run(&mut rig, &target, &target, &in_one, 1, None, 0);
    assert!(matches!(report.outcome, FrOutcome::Stopped(_)), "{:?}", report.outcome);
    assert_eq!((report.ok, report.rolled_back, report.resume_from), (9, 1, Some(9)), "{:?}", report.entries);
    assert_eq!(scalar(&mut rig, &format!("SELECT GROUP_CONCAT(id ORDER BY id) FROM {target}.{whole}")), "1,2", "what the report says was kept is in the table");

    // B3: the session a late start runs in is set up again: USE and the user variable, the versioned SET.
    let session = format!(
        "DROP DATABASE IF EXISTS {other};\nCREATE DATABASE {other};\nUSE {other};\nSET @cf_v = 7;\n/*!40101 SET SESSION sql_mode = '' */;\nCREATE TABLE t (id INT NOT NULL);\n\
         INSERT INTO t VALUES (@cf_v);\nINSERT INTO t VALUES (@cf_v + 1);\n"
    );
    let report = fr_live_run(&mut rig, &target, &target, &session, 0, None, 0);
    assert_eq!((report.outcome.clone(), report.ok), (FrOutcome::Completed, 8), "{:?}", report.entries);
    let report = fr_live_run_from(&mut rig, &target, &target, &session, 7);
    assert_eq!((report.outcome.clone(), report.passed_over, report.replayed, report.ok), (FrOutcome::Completed, 3, 3, 2), "{:?}", report.entries);
    assert_eq!(scalar(&mut rig, &format!("SELECT GROUP_CONCAT(id ORDER BY id) FROM {other}.t")), "7,7,8,8", "the rows went to the database USE selected, with the variable it set");

    // B5: a SELECT runs for every row (the variable counts them), not for a page of one.
    let values = (1..=50).map(|id| format!("({id})")).collect::<Vec<_>>().join(",");
    let rows = format!(
        "DROP TABLE IF EXISTS {fifty};\nCREATE TABLE {fifty} (id INT);\nINSERT INTO {fifty} VALUES {values};\nDROP TABLE IF EXISTS {check};\nCREATE TABLE {check} (v INT NOT NULL);\nSET @n = 0;\n\
         SELECT @n := @n + 1 FROM {fifty};\nINSERT INTO {check} SELECT IF(@n = 50, 1, NULL);\n"
    );
    let report = fr_live_run(&mut rig, &target, &target, &rows, 0, None, 0);
    assert_eq!((report.outcome.clone(), report.ok, report.result_sets), (FrOutcome::Completed, 8, 1), "the SELECT did not run for all 50 rows: {:?}", report.entries);
    assert_eq!(scalar(&mut rig, &format!("SELECT COUNT(*) FROM {target}.{check}")), "1");

    // R2: a header longer than any cap before the USE; the rows still land in the database it selects.
    let header = format!("/* {} */\n", "x".repeat(700));
    let long_header = format!("{header}DROP DATABASE IF EXISTS `{other}`;\nCREATE DATABASE `{other}`;\n{header}USE `{other}`;\nCREATE TABLE h (id INT NOT NULL);\nINSERT INTO h VALUES (1);\n");
    let report = fr_live_run(&mut rig, &target, &target, &long_header, 0, None, 0);
    assert_eq!((report.outcome.clone(), report.ok), (FrOutcome::Completed, 5), "{:?}", report.entries);
    assert_eq!(scalar(&mut rig, &format!("SELECT COUNT(*) FROM {other}.h")), "1", "after a header of 700 bytes the USE still counts");

    // R2: `mysqldump --add-drop-database` restored with that database chosen in the wizard: the file drops it, creates it and USEs it.
    exec_confirmed(&rig.session, &format!("CREATE DATABASE IF NOT EXISTS `{other}`"));
    let add_drop = format!(
        "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;\n/*!40000 DROP DATABASE IF EXISTS `{other}`*/;\nCREATE DATABASE /*!32312 IF NOT EXISTS*/ `{other}` /*!40100 DEFAULT CHARACTER SET utf8mb4 */;\nUSE `{other}`;\n\
         CREATE TABLE `d` (`id` int NOT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB;\nINSERT INTO `d` VALUES (1),(2),(3);\n"
    );
    let report = fr_live_run(&mut rig, &other, &other, &add_drop, 0, None, 0);
    assert_eq!((report.outcome.clone(), report.ok, report.failed), (FrOutcome::Completed, 6, 0), "{:?}", report.entries);
    assert_eq!(scalar(&mut rig, &format!("SELECT COUNT(*) FROM {other}.d")), "3");

    // R3: 1000 rows in one transaction, then a statement the server refuses that is not DDL (LOAD DATA is not a commit): every row
    // is rolled back, the report says 1000, the table is empty.
    let rows: String = (1..=1000).map(|id| format!("INSERT INTO {thousand} VALUES ({id});\n")).collect();
    let thousand_file = format!("DROP TABLE IF EXISTS {thousand};\nCREATE TABLE {thousand} (id INT NOT NULL) ENGINE=InnoDB;\n{rows}LOAD DATA LOCAL INFILE '/nonexistent/cf_r5.csv' INTO TABLE {thousand};\n");
    let report = fr_live_run(&mut rig, &target, &target, &thousand_file, 1, None, 0);
    assert!(matches!(report.outcome, FrOutcome::Stopped(_)), "{:?}", report.outcome);
    assert_eq!((report.ok, report.rolled_back, report.resume_from), (1002, 1000, Some(3)), "{:?}", report.entries);
    assert_eq!(scalar(&mut rig, &format!("SELECT COUNT(*) FROM {target}.{thousand}")), "0", "nothing of the 1000 rows stays");

    // R4: a `--no-autocommit` dump in an autocommit run. The transaction it opens with SET autocommit=0 is the file's: the run ends
    // inside the one after its COMMIT, rolls it back, and says to start there.
    let no_commit = format!(
        "SET autocommit=0;\nDROP TABLE IF EXISTS {no_autocommit};\nCREATE TABLE {no_autocommit} (id INT PRIMARY KEY) ENGINE=InnoDB;\nINSERT INTO {no_autocommit} VALUES (1),(2);\nCOMMIT;\n\
         INSERT INTO {no_autocommit} VALUES (3);\nINSERT INTO {no_autocommit} VALUES (3);\n"
    );
    let report = fr_live_run(&mut rig, &target, &target, &no_commit, 0, None, 0);
    assert!(matches!(report.outcome, FrOutcome::Stopped(_)), "{:?}", report.outcome);
    assert_eq!((report.ok, report.rolled_back, report.resume_from), (6, 1, Some(6)), "{:?}", report.entries);
    assert_eq!(scalar(&mut rig, &format!("SELECT GROUP_CONCAT(id ORDER BY id) FROM {target}.{no_autocommit}")), "1,2");
    drop(rig);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_an_empty_search_path_gets_the_schema_alone_and_a_full_one_keeps_its_rest(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI to the dedicated tablex-test-pg container");
        return;
    };
    let schema = format!("cf_r5_p_{}", &uuid::Uuid::new_v4().to_simple().to_string()[..8]);
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let _cleanup = OaCleanup(rig.session.clone(), vec!["RESET search_path".to_owned()]);
    exec_confirmed(&rig.session, "SET search_path = ''");
    exec_confirmed(&rig.session, &crate::app::file_run::search_path_sql(&schema));
    assert_eq!(oa_scalar(&mut rig, "SHOW search_path"), schema, "no comma after the schema of an empty path");
    exec_confirmed(&rig.session, "SET search_path = public, pg_catalog");
    exec_confirmed(&rig.session, &crate::app::file_run::search_path_sql(&schema));
    assert_eq!(oa_scalar(&mut rig, "SHOW search_path"), format!("{schema}, public, pg_catalog"));
    drop(rig);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_a_dump_uses_its_database_commits_in_comments_resumes_with_its_session_and_selects_every_row(cx: &mut TestAppContext) {
    live_mysql_family_sessions(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_a_dump_uses_its_database_commits_in_comments_resumes_with_its_session_and_selects_every_row(cx: &mut TestAppContext) {
    live_mysql_family_sessions(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}
