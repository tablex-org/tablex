// The rows an UPDATE / DELETE will change, shown in the confirmation that asks before it runs.
// Same harness as `sql_driver.rs`: the REAL SQLite driver behind a real host session, the real
// confirmation dialogs clicked by their measured bounds. The extraction itself is tested in
// `affected_rows.rs`.

/// A statement that counts for as long as it is allowed to (the count of an endless series is
/// read without keeping its rows), so a preview of it is still in flight when a test looks.
const SLOW_DELETE: &str = "DELETE FROM items WHERE id = (SELECT count(*) FROM (WITH RECURSIVE c(x) AS \
     (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 4000000000) SELECT x FROM c))";

impl SqlRig<'_> {
    /// What the last confirmation's preview says: `(line, warning, rows)` per section.
    fn preview_texts(&self) -> Option<Vec<(String, Option<String>, usize)>> {
        self.app.read_with(self.visual, |this, cx| {
            let preview = this.driver_runtime.last_preview.as_ref()?.upgrade()?;
            Some(preview.read(cx).texts())
        })
    }

    fn preview_hint(&self) -> bool {
        self.app.read_with(self.visual, |this, cx| {
            let preview = this.driver_runtime.last_preview.as_ref().and_then(|preview| preview.upgrade());
            preview.is_some_and(|preview| preview.read(cx).hint_shown())
        })
    }

    fn preview_alive(&self) -> bool {
        self.app.read_with(self.visual, |this, _| {
            this.driver_runtime.last_preview.as_ref().is_some_and(|preview| preview.upgrade().is_some())
        })
    }

    fn forget_preview(&mut self) {
        self.app.update(self.visual, |this, _| {
            this.driver_runtime.last_preview = None;
            this.driver_runtime.cancelled_previews = 0;
        });
    }

    fn preview_reads(&self) -> usize {
        self.app.read_with(self.visual, |this, _| this.driver_runtime.preview_reads)
    }

    fn preview_notes(&self) -> Vec<(Option<String>, Option<String>)> {
        self.app.read_with(self.visual, |this, cx| {
            let preview = this.driver_runtime.last_preview.as_ref().and_then(|preview| preview.upgrade()).expect("a preview");
            preview.read(cx).notes()
        })
    }

    fn cancelled_previews(&self) -> usize {
        self.app.read_with(self.visual, |this, _| this.driver_runtime.cancelled_previews)
    }

    /// Waits until no section of the preview is counting any more.
    fn wait_preview(&mut self) -> Vec<(String, Option<String>, usize)> {
        for _ in 0..400 {
            settle(self.visual);
            let done = self.app.read_with(self.visual, |this, cx| {
                let preview = this.driver_runtime.last_preview.as_ref().and_then(|preview| preview.upgrade());
                preview.is_some_and(|preview| !preview.read(cx).counting())
            });
            if done {
                return self.preview_texts().expect("the preview");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the preview never finished: {:?}", self.preview_texts());
    }

    fn set_preview_setting(&mut self, on: bool) {
        self.app.update(self.visual, |this, _| {
            let mut settings = (**this.settings).clone();
            settings.editor.preview_affected_rows = on;
            *this.settings = std::sync::Arc::new(settings);
        });
    }
}

fn production_rig<'a>(cx: &'a mut TestAppContext, db: &SqliteDb) -> SqlRig<'a> {
    let mut rig = SqlRig::sqlite(cx, db);
    rig.give_profile(typed(PRODUCTION));
    rig
}

#[gpui::test]
fn the_confirmation_of_a_delete_counts_the_rows_and_shows_the_first_ones(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);

    // A SELECT is asked about, and previews nothing.
    rig.run("SELECT * FROM items");
    rig.wait_rendered("execute-confirm-run");
    assert!(!rig.preview_alive(), "a SELECT has no preview");
    assert!(rig.visual.debug_bounds("affected-preview").is_none());
    rig.click("execute-confirm-cancel");

    rig.run("DELETE FROM items WHERE id < 3");
    rig.wait_rendered("execute-confirm-run");
    assert!(rig.dialog_is_open("execute-confirm-statement"), "the statement is still shown");
    let texts = rig.wait_preview();
    assert_eq!(texts, vec![("It will delete 2 rows.".to_owned(), None, 2)]);
    rig.wait_rendered("affected-rows-0");
    assert!(rig.visual.debug_bounds("affected-line-0").is_some());
    assert!(rig.visual.debug_bounds("affected-warning-0").is_none(), "a WHERE is no warning");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5", "the preview wrote nothing");

    // Cancel: nothing ran, the preview is gone with the dialog.
    rig.click("execute-confirm-cancel");
    assert!(!rig.dialog_is_open("execute-confirm-run"));
    assert!(!rig.preview_alive(), "the preview ended with its dialog");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");

    // Run it for real: the statement does what the preview said.
    rig.run("UPDATE items SET name = 'q' WHERE id > 3");
    rig.wait_rendered("execute-confirm-run");
    assert_eq!(rig.wait_preview(), vec![("It will update 2 rows.".to_owned(), None, 2)]);
    rig.click("execute-confirm-run");
    rig.wait_status("the UPDATE", |status| matches!(status, crate::query::QueryStatus::Ready { .. }));
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'q'"), "2");
}

#[gpui::test]
fn a_statement_without_where_warns_that_it_changes_every_row(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);

    // On Production the dialog that asks about every statement carries the warning ...
    rig.run("DELETE FROM items");
    rig.wait_rendered("execute-confirm-run");
    let texts = rig.wait_preview();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].1.as_deref(), Some("This changes every row in items (5 rows)."));
    assert_eq!(texts[0].2, 5, "the grid holds the rows");
    rig.wait_rendered("affected-warning-0");
    // ... and so does the one the driver asks for a statement it flags as dangerous.
    rig.click("execute-confirm-run");
    rig.wait_rendered("sql-confirm-run");
    rig.wait_rendered("affected-warning-0");
    let texts = rig.wait_preview();
    assert_eq!(texts[0].1.as_deref(), Some("This changes every row in items (5 rows)."));
    rig.click("sql-confirm-cancel");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");

    // A connection that asks about nothing still asks about a statement without WHERE, with the preview.
    rig.give_profile(ConnectionProfile::default());
    rig.run("UPDATE items SET name = 'x'");
    rig.wait_rendered("sql-confirm-run");
    assert!(!rig.dialog_is_open("execute-confirm-run"));
    let texts = rig.wait_preview();
    assert_eq!(texts[0].1.as_deref(), Some("This changes every row in items (5 rows)."));
    rig.click("sql-confirm-cancel");
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'x'"), "0");

    // With a WHERE it runs without asking, and without a preview (nothing is read on this path).
    rig.forget_preview();
    rig.run("UPDATE items SET name = 'y' WHERE id = 1");
    rig.wait_status("the UPDATE with WHERE", |status| matches!(status, crate::query::QueryStatus::Ready { .. }));
    assert!(!rig.preview_alive());
    assert!(!rig.dialog_is_open("sql-confirm-run"));
}

#[gpui::test]
fn a_preview_is_a_read_only_read_and_a_write_through_it_fails(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let rig = SqlRig::sqlite(cx, &db);
    let (app, key, tab) = (rig.app.clone(), rig.key.clone(), rig.tab);
    for (sql, refused) in [
        ("DELETE FROM items", true),
        ("UPDATE items SET name = 'x' WHERE id = 1", true),
        ("INSERT INTO items(name) VALUES ('z')", true),
        ("SELECT count(*) FROM items", false),
    ] {
        let prepared = app.read_with(rig.visual, |this, _| this.begin_preview_query(tab, &key)).expect("a session");
        let request = affected_preview::preview_request(prepared.operation.operation_id, None, sql);
        let page = affected_preview::open_page(prepared.session, prepared.runner, request);
        match (page, refused) {
            (Err(error), true) => assert!(error.is_read_only_violation(), "{sql}: {error}"),
            (Ok((page, ..)), false) => assert_eq!(page.row_count, 1),
            (other, _) => panic!("{sql}: {:?}", other.map(|(page, ..)| page.row_count)),
        }
    }
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5", "nothing was written through the preview");
    assert_eq!(db.sql("SELECT group_concat(name) FROM items"), "a,b,c,d,e");
}

#[gpui::test]
fn closing_the_confirmation_cancels_the_preview_in_flight(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    rig.run(SLOW_DELETE);
    rig.wait_rendered("execute-confirm-run");
    let handle = rig.handle.clone();
    wait_until(rig.visual, "the count to start", |_| handle.active_operation_count() > 0);

    // It is counting, and the dialog does not wait: Run is there.
    let counting = t!("editor.affected_counting").into_owned();
    assert_eq!(rig.preview_texts().unwrap()[0].0, counting);
    assert!(rig.dialog_is_open("execute-confirm-run"));
    assert!(!rig.preview_hint(), "no hint before the wait is long");
    // Past about three seconds the dialog says so.
    rig.visual.executor().advance_clock(std::time::Duration::from_secs(4));
    for _ in 0..200 {
        settle(rig.visual);
        if rig.preview_hint() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(rig.preview_hint(), "past three seconds the dialog says it is still counting");
    assert_eq!(rig.cancelled_previews(), 0);

    // Cancel: the count stops (the driver lets go of its operation long before the series ends).
    rig.click("execute-confirm-cancel");
    assert!(!rig.dialog_is_open("execute-confirm-run"));
    assert!(!rig.preview_alive());
    assert_eq!(rig.cancelled_previews(), 1, "the dialog cancelled the preview's query");
    rig.wait_driver_idle();
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
}

#[gpui::test]
fn the_statement_can_be_run_while_the_count_is_still_going(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    rig.run(SLOW_DELETE);
    rig.wait_rendered("execute-confirm-run");
    let handle = rig.handle.clone();
    wait_until(rig.visual, "the count to start", |_| handle.active_operation_count() > 0);

    rig.click("execute-confirm-run");
    assert!(!rig.dialog_is_open("execute-confirm-run"));
    assert_eq!(rig.cancelled_previews(), 1, "running closes the dialog and ends the count");
    rig.wait_status("the statement", |status| matches!(status, crate::query::QueryStatus::Running { .. }));
    // The statement is the one running now; stop it too.
    rig.click("run-query");
    rig.wait_driver_idle();
}

#[gpui::test]
fn a_script_previews_its_first_five_changes_and_sums_up_the_rest(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    let mut script = String::from("SELECT 1;\n");
    for id in 1..=7 {
        script.push_str(&format!("DELETE FROM items WHERE id = {id};\n"));
    }
    rig.run(&script);
    rig.wait_rendered("execute-confirm-run");
    let texts = rig.wait_preview();
    assert_eq!(texts.len(), 5, "at most five statements are previewed");
    for (index, (line, warning, rows)) in texts.iter().enumerate() {
        let expected = if index < 5 { "It will delete 1 row." } else { "" };
        assert_eq!((line.as_str(), warning.as_ref(), *rows), (expected, None, 1), "statement {index}");
        assert!(rig.visual.debug_bounds(Box::leak(format!("affected-heading-{index}").into_boxed_str())).is_some());
    }
    assert!(rig.visual.debug_bounds("affected-more").is_some(), "the other two are summed up");
    assert!(rig.visual.debug_bounds("affected-script-note").is_some());
    rig.click("execute-confirm-cancel");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
}

#[gpui::test]
fn a_statement_that_cannot_be_read_beforehand_says_so_and_still_runs(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    // A join (SQLite's UPDATE ... FROM) is not previewed.
    rig.run("UPDATE items SET name = u.name || '!' FROM items AS u WHERE u.id = items.id AND items.id = 1");
    rig.wait_rendered("execute-confirm-run");
    let texts = rig.wait_preview();
    assert_eq!(texts, vec![("Preview not available for this statement".to_owned(), None, 0)]);
    assert!(rig.visual.debug_bounds("affected-rows-0").is_none());
    // It never blocks the run.
    rig.click("execute-confirm-run");
    rig.wait_status("the UPDATE FROM", |status| matches!(status, crate::query::QueryStatus::Ready { .. }));
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 1"), "a!");
}

#[gpui::test]
fn the_setting_turns_the_preview_off(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    assert!(rig.app.read_with(rig.visual, |this, _| this.settings.editor.preview_affected_rows), "on by default");
    rig.set_preview_setting(false);
    rig.run("DELETE FROM items WHERE id = 1");
    rig.wait_rendered("execute-confirm-run");
    settle(rig.visual);
    assert!(!rig.preview_alive());
    assert!(rig.visual.debug_bounds("affected-preview").is_none());
    assert!(!rig.operation_registered());
    rig.click("execute-confirm-cancel");
}

#[gpui::test]
fn the_buttons_do_not_move_when_the_rows_land(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    // With a WHERE, and without one (whose warning is there before the count is).
    for sql in ["DELETE FROM items WHERE id < 4", "DELETE FROM items"] {
        rig.run(sql);
        rig.wait_rendered("execute-confirm-run");
        let before = rig.visual.debug_bounds("execute-confirm-run").expect("the Run button");
        rig.wait_preview();
        rig.wait_rendered("affected-rows-0");
        settle(rig.visual);
        let after = rig.visual.debug_bounds("execute-confirm-run").expect("the Run button");
        assert_eq!(before, after, "{sql}: a click aimed at Run while the count is landing must still hit it");
        rig.click("execute-confirm-cancel");
    }
}

#[gpui::test]
fn reads_that_take_too_long_are_cancelled_and_the_statement_can_still_run(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    rig.run(SLOW_DELETE);
    rig.wait_rendered("execute-confirm-run");
    let handle = rig.handle.clone();
    wait_until(rig.visual, "the count to start", |_| handle.active_operation_count() > 0);
    // The connection's shared session gives a count five seconds: not yet at four ...
    rig.visual.executor().advance_clock(std::time::Duration::from_secs(4));
    settle(rig.visual);
    assert!(rig.preview_texts().is_some_and(|texts| texts[0].0 == t!("editor.affected_counting").into_owned()));
    // ... and over at six: the read is cancelled, no rows are read after it.
    rig.visual.executor().advance_clock(std::time::Duration::from_secs(2));
    rig.wait_preview();
    assert_eq!(rig.preview_texts().unwrap(), vec![("Counting took too long; not previewed".to_owned(), None, 0)]);
    assert_eq!(rig.cancelled_previews(), 1);
    assert_eq!(rig.preview_reads(), 1, "the rows were not read after the count timed out");
    rig.wait_driver_idle();
    // The dialog is still there, and Run still works.
    assert!(rig.dialog_is_open("execute-confirm-run"));
    rig.click("execute-confirm-cancel");
}

#[gpui::test]
fn a_tabs_own_session_gives_the_count_twenty_seconds(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('mine')");
    rig.give_profile(typed(PRODUCTION));
    rig.run(SLOW_DELETE);
    rig.wait_rendered("execute-confirm-run");
    let handle = rig.handle.clone();
    wait_until(rig.visual, "the count to start", |_| handle.active_operation_count() > 0);
    // Past the shared session's five seconds the count on the tab's own session goes on ...
    rig.visual.executor().advance_clock(std::time::Duration::from_secs(6));
    settle(rig.visual);
    assert!(rig.preview_texts().is_some_and(|texts| texts[0].0 == t!("editor.affected_counting").into_owned()));
    assert_eq!(rig.cancelled_previews(), 0);
    // ... until the twenty are over.
    rig.visual.executor().advance_clock(std::time::Duration::from_secs(15));
    rig.wait_preview();
    assert_eq!(rig.preview_texts().unwrap()[0].0, "Counting took too long; not previewed");
    assert_eq!(rig.cancelled_previews(), 1);
    rig.wait_driver_idle();
    // The user's transaction is as it was.
    rig.click("execute-confirm-cancel");
    assert!(rig.tx(a).is_some_and(|tx| tx.is_open()));
    rig.activate(a);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT count(*) FROM items"), "6", "the user's insert committed intact");
}

#[gpui::test]
fn the_dialog_that_follows_shows_what_the_first_read_instead_of_reading_again(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    rig.run("DELETE FROM items");
    rig.wait_rendered("execute-confirm-run");
    rig.wait_preview();
    assert_eq!(rig.preview_reads(), 2, "a count and the rows");
    // Run: the driver flags it, and its dialog opens with the same rows, read by nobody.
    rig.click("execute-confirm-run");
    rig.wait_rendered("sql-confirm-run");
    let texts = rig.wait_preview();
    assert_eq!(texts[0].1.as_deref(), Some("This changes every row in items (5 rows)."));
    assert_eq!(texts[0].2, 5);
    assert_eq!(rig.preview_reads(), 2, "nothing was read for the second dialog");
    rig.click("sql-confirm-cancel");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
}

#[gpui::test]
fn sqlite_or_replace_says_it_also_deletes_and_no_matching_rows_say_so_in_the_box(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    rig.run("UPDATE OR REPLACE items SET id = 2 WHERE id = 1");
    rig.wait_rendered("execute-confirm-run");
    rig.wait_preview();
    let notes = rig.preview_notes();
    assert!(notes[0].0.as_deref().is_some_and(|note| note.contains("OR REPLACE also deletes")), "{notes:?}");
    assert!(rig.visual.debug_bounds("affected-note-0").is_some());
    rig.click("execute-confirm-cancel");

    rig.run("DELETE FROM items WHERE id > 100");
    rig.wait_rendered("execute-confirm-run");
    let texts = rig.wait_preview();
    assert_eq!(texts, vec![("It will delete 0 rows.".to_owned(), None, 0)]);
    assert_eq!(rig.preview_notes()[0].1.as_deref(), Some("No rows match"));
    rig.wait_rendered("affected-box-note-0");
    rig.click("execute-confirm-cancel");
}

#[gpui::test]
fn a_failed_read_keeps_the_box_and_gives_its_reason_inside_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    rig.run("DELETE FROM items WHERE no_such_column = 1");
    rig.wait_rendered("execute-confirm-run");
    let before = rig.visual.debug_bounds("execute-confirm-run").expect("the Run button");
    let texts = rig.wait_preview();
    assert_eq!(texts[0].0, "Preview not available for this statement");
    let reason = rig.preview_notes()[0].1.clone().expect("the reason");
    assert!(reason.contains("no_such_column") && !reason.contains('\n') && reason.chars().count() <= 111, "{reason}");
    rig.wait_rendered("affected-box-note-0");
    settle(rig.visual);
    assert_eq!(rig.visual.debug_bounds("execute-confirm-run"), Some(before), "the buttons did not move");
    rig.click("execute-confirm-cancel");
}

#[gpui::test]
fn a_tab_in_manual_commit_mode_previews_on_its_own_session(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('mine')");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5", "not committed: the other sessions do not see it");
    // The preview reads in the tab's transaction: it counts the uncommitted row too.
    rig.run("DELETE FROM items");
    rig.wait_rendered("sql-confirm-run");
    let texts = rig.wait_preview();
    assert_eq!(texts[0].1.as_deref(), Some("This changes every row in items (6 rows)."));
    rig.click("sql-confirm-cancel");
    // The preview left the user's transaction as it was.
    assert!(rig.tx(a).is_some_and(|tx| tx.is_open()));
    rig.activate(a);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT count(*) FROM items"), "6", "the user's insert committed intact");
}

#[gpui::test]
fn a_cancelled_dialog_leaves_nothing_behind_so_the_next_one_reads_what_is_there_now(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    rig.run("DELETE FROM items WHERE id < 3");
    rig.wait_rendered("execute-confirm-run");
    assert_eq!(rig.wait_preview(), vec![("It will delete 2 rows.".to_owned(), None, 2)]);
    assert_eq!(rig.preview_reads(), 2);
    rig.click("execute-confirm-cancel");
    // The data changes meanwhile (another session): the same statement now matches three rows.
    db.sql("INSERT INTO items(id, name) VALUES (0, 'z')");
    rig.run("DELETE FROM items WHERE id < 3");
    rig.wait_rendered("execute-confirm-run");
    assert_eq!(rig.wait_preview(), vec![("It will delete 3 rows.".to_owned(), None, 3)]);
    assert_eq!(rig.preview_reads(), 4, "read again: Cancel kept nothing");
    rig.click("execute-confirm-cancel");
}

#[gpui::test]
fn what_the_first_dialog_read_goes_to_the_next_one_of_the_run_once_and_only_on_the_same_connection(cx: &mut TestAppContext) {
    use super::affected_preview::Chain;
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    let (app, tab) = (rig.app.clone(), rig.tab);
    let sql = "DELETE FROM items WHERE id < 3";
    rig.run(sql);
    rig.wait_rendered("execute-confirm-run");
    rig.wait_preview();
    assert_eq!(rig.preview_reads(), 2);
    // The first dialog's preview, held here: what Run hands on is what it read.
    let first_dialog = app.read_with(rig.visual, |this, _| this.driver_runtime.last_preview.as_ref().and_then(|preview| preview.upgrade()));
    let next = |rig: &mut SqlRig, chain: Chain| {
        let app = rig.app.clone();
        rig.visual.update(|window, cx| app.update(cx, |this, cx| this.affected_preview_for(tab, sql, chain, window, cx)))
    };
    let hand_off = |rig: &mut SqlRig| {
        let first_dialog = first_dialog.clone();
        rig.app.update(rig.visual, |this, cx| this.hand_off_preview(tab, first_dialog.as_ref(), cx));
    };
    // Run was answered: the reads are handed on, and the next dialog takes them (once).
    hand_off(&mut rig);
    let taken = next(&mut rig, Chain::Continued).expect("a preview");
    assert_eq!(rig.wait_preview(), vec![("It will delete 2 rows.".to_owned(), None, 2)]);
    assert_eq!(rig.preview_reads(), 2, "the second dialog read nothing");
    // Used once: a third one reads for itself.
    drop(taken);
    let again = next(&mut rig, Chain::Continued).expect("a preview");
    rig.wait_preview();
    assert_eq!(rig.preview_reads(), 4);
    drop(again);
    // Another run (any first confirmation) drops it.
    hand_off(&mut rig);
    let first = next(&mut rig, Chain::First).expect("a preview");
    rig.wait_preview();
    assert_eq!(rig.preview_reads(), 6);
    drop(first);
    let after = next(&mut rig, Chain::Continued).expect("a preview");
    rig.wait_preview();
    assert_eq!(rig.preview_reads(), 8, "nothing was left to take");
    drop(after);
    // The tab moved to another database: what was read there is not shown here.
    hand_off(&mut rig);
    app.update(rig.visual, |this, _| this.workspace.tab_mut(tab).unwrap().database = "elsewhere".into());
    let moved = next(&mut rig, Chain::Continued).expect("a preview");
    rig.wait_preview();
    assert_eq!(rig.preview_reads(), 10, "read again on the new database");
    drop(moved);
    // And a run that executes drops it.
    app.update(rig.visual, |this, _| this.workspace.tab_mut(tab).unwrap().database = String::new());
    hand_off(&mut rig);
    app.update(rig.visual, |this, _| this.drop_preview_handoff());
    let executed = next(&mut rig, Chain::Continued).expect("a preview");
    rig.wait_preview();
    assert_eq!(rig.preview_reads(), 12);
    drop(executed);
    rig.click("execute-confirm-cancel");
}

#[gpui::test]
fn a_run_that_never_starts_leaves_no_hand_off_behind(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    let (app, tab) = (rig.app.clone(), rig.tab);
    let sql = "DELETE FROM items WHERE id < 3";
    rig.run(sql);
    rig.wait_rendered("execute-confirm-run");
    rig.wait_preview();
    let first_dialog = app.read_with(rig.visual, |this, _| this.driver_runtime.last_preview.as_ref().and_then(|preview| preview.upgrade()));
    let handed_on = |rig: &mut SqlRig| {
        let first_dialog = first_dialog.clone();
        rig.app.update(rig.visual, |this, cx| {
            this.hand_off_preview(tab, first_dialog.as_ref(), cx);
            this.driver_runtime.preview_handoff.is_some()
        })
    };
    // A run of a tab that is gone.
    assert!(handed_on(&mut rig));
    app.update(rig.visual, |this, cx| this.start_run(tab + 1000, sql.into(), None, None, None, cx));
    assert!(app.read_with(rig.visual, |this, _| this.driver_runtime.preview_handoff.is_none()));
    // A run of a tab whose connection is not known.
    assert!(handed_on(&mut rig));
    app.update(rig.visual, |this, cx| {
        this.workspace.tab_mut(tab).unwrap().connection_id = String::new();
        this.start_run(tab, sql.into(), None, None, None, cx);
    });
    assert!(app.read_with(rig.visual, |this, _| this.driver_runtime.preview_handoff.is_none()));
    rig.click("execute-confirm-cancel");
}

#[gpui::test]
fn a_cancel_the_dialog_did_not_ask_for_ends_every_section_of_a_script(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    rig.run(&format!("{SLOW_DELETE};\nDELETE FROM items WHERE id = 1;\nDELETE FROM items WHERE id = 2"));
    rig.wait_rendered("execute-confirm-run");
    let handle = rig.handle.clone();
    wait_until(rig.visual, "the count to start", |_| handle.active_operation_count() > 0);
    rig.app.read_with(rig.visual, |this, cx| {
        this.driver_runtime.last_preview.as_ref().and_then(|preview| preview.upgrade()).unwrap().read(cx).cancel_from_outside();
    });
    // The sections after the cancelled one do not wait for the deadline either.
    let texts = rig.wait_preview();
    assert_eq!(texts.len(), 3, "{texts:?}");
    for (line, ..) in &texts {
        assert_eq!(line, "Preview not available for this statement", "{texts:?}");
    }
    rig.wait_driver_idle();
    rig.click("execute-confirm-cancel");
}

#[gpui::test]
fn the_script_dialog_after_run_shows_what_the_first_read_without_reading_again(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    rig.run("DELETE FROM items WHERE id = 1; DELETE FROM items;");
    rig.wait_rendered("execute-confirm-run");
    let first = rig.wait_preview();
    assert_eq!(first.len(), 2);
    assert_eq!(rig.preview_reads(), 4, "a count and the rows, twice");
    // Run: the driver flags the second statement, and the script's own dialog asks, with the same rows.
    rig.click("execute-confirm-run");
    rig.wait_rendered("script-confirm-run");
    let texts = rig.wait_preview();
    assert_eq!(texts, first);
    assert_eq!(texts[0].0, "It will delete 1 row.");
    assert_eq!(texts[1].1.as_deref(), Some("This changes every row in items (5 rows)."));
    assert_eq!(rig.preview_reads(), 4, "the script's dialog read nothing");
    rig.click("script-confirm-cancel");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
}

#[gpui::test]
fn an_old_postgresql_driver_does_not_preview_on_a_tabs_own_session(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (app, a) = (rig.app.clone(), rig.tab);
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('mine')");
    rig.give_profile(typed(PRODUCTION));
    // The connection is PostgreSQL as far as the preview can tell (the statements below run on the
    // test's driver, which reads them as well); the installed driver is older than the fix.
    let key = rig.key.clone();
    let record = |version: &str| engine_driver_host::InstalledDriverRecord {
        id: "postgresql".into(),
        version: version.into(),
        ..Default::default()
    };
    app.update(rig.visual, |this, _| {
        this.saved_connections.iter_mut().find(|saved| saved.matches_id(&key)).unwrap().kind = ConnectionKind::Postgres;
        let id = ConnectionKind::Postgres.driver_id().to_owned();
        this.driver_ui.records.insert(id, record("0.1.4"));
    });
    rig.run("DELETE FROM items WHERE id = 1");
    rig.wait_rendered("execute-confirm-run");
    settle(rig.visual);
    assert_eq!(rig.preview_texts().unwrap(), vec![("Update the PostgreSQL driver to preview inside a transaction".to_owned(), None, 0)]);
    assert_eq!(rig.preview_reads(), 0, "nothing was read on the transaction's session");
    rig.click("execute-confirm-cancel");
    // A driver with the fix previews there (and an auto-commit tab never needed it).
    app.update(rig.visual, |this, _| {
        this.driver_ui.records.insert(ConnectionKind::Postgres.driver_id().to_owned(), record("0.1.5"));
    });
    rig.run("DELETE FROM items WHERE id = 1");
    rig.wait_rendered("execute-confirm-run");
    assert_eq!(rig.wait_preview(), vec![("It will delete 1 row.".to_owned(), None, 1)]);
    rig.click("execute-confirm-cancel");
}

#[gpui::test]
fn a_statement_without_where_whose_read_fails_keeps_the_buttons_where_they_were(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    rig.run("DELETE FROM no_such_table");
    rig.wait_rendered("execute-confirm-run");
    let before = rig.visual.debug_bounds("execute-confirm-run").expect("the Run button");
    rig.wait_preview();
    rig.wait_rendered("affected-box-note-0");
    settle(rig.visual);
    assert_eq!(rig.visual.debug_bounds("execute-confirm-run"), Some(before), "the buttons did not move");
    let notes = rig.preview_notes();
    assert!(notes[0].1.as_deref().is_some_and(|note| note.contains("no_such_table")), "{notes:?}");
    assert!(rig.visual.debug_bounds("affected-warning-0").is_some(), "it still says it changes every row");
    rig.click("execute-confirm-cancel");
}

#[gpui::test]
fn escape_during_the_count_on_a_manual_sqlite_tab_leaves_the_transaction_intact(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('mine')");
    rig.give_profile(typed(PRODUCTION));
    rig.run(SLOW_DELETE);
    rig.wait_rendered("execute-confirm-run");
    let handle = rig.handle.clone();
    wait_until(rig.visual, "the count to start on the tab's own session", |_| handle.active_operation_count() > 0);
    assert_eq!(rig.preview_texts().unwrap()[0].0, t!("editor.affected_counting").into_owned());

    // Interrupting the SELECT of the count is not a write: the user's transaction is not rolled back.
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert!(!rig.dialog_is_open("execute-confirm-run"));
    assert_eq!(rig.cancelled_previews(), 1, "Escape cancelled the preview's query");
    rig.wait_driver_idle();
    assert!(rig.tx(a).is_some_and(|tx| tx.is_open()));
    rig.activate(a);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT count(*) FROM items"), "6", "the user's insert committed intact");
}

#[gpui::test]
fn running_in_the_middle_of_a_script_count_reads_only_the_missing_section_again(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    // The first statement is counted at once and flagged by the driver; the second is still counting.
    rig.run(&format!("DELETE FROM items; {SLOW_DELETE}"));
    rig.wait_rendered("execute-confirm-run");
    let handle = rig.handle.clone();
    wait_until(rig.visual, "the second count to start", |_| handle.active_operation_count() > 0);
    assert_eq!(rig.preview_reads(), 3, "a count and the rows of the first statement, the count of the second");
    let counting = t!("editor.affected_counting").into_owned();
    assert_eq!(rig.preview_texts().unwrap()[1].0, counting);

    rig.click("execute-confirm-run");
    rig.wait_rendered("script-confirm-run");
    // Only what had landed came along: the first section is shown as read, the second is read again.
    assert_eq!(rig.preview_reads(), 4, "one more count, for the section that had not landed");
    let texts = rig.preview_texts().unwrap();
    assert_eq!(texts[0].1.as_deref(), Some("This changes every row in items (5 rows)."));
    assert_eq!(texts[0].2, 5);
    assert_eq!(texts[1], (counting, None, 0), "nothing of the unfinished read is shown");
    rig.click("script-confirm-cancel");
    rig.wait_driver_idle();
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
}

#[gpui::test]
fn editing_the_editor_after_the_dialog_opened_still_runs_and_previews_the_original(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    let (app, tab) = (rig.app.clone(), rig.tab);
    let edit = |rig: &mut SqlRig, sql: &str| {
        app.update(rig.visual, |this, _| this.workspace.tab_mut(tab).unwrap().sql = sql.into());
    };
    // The second dialog shows the rows of the statement the first one asked about.
    rig.run("DELETE FROM items");
    rig.wait_rendered("execute-confirm-run");
    rig.wait_preview();
    edit(&mut rig, "SELECT 1");
    rig.click("execute-confirm-run");
    rig.wait_rendered("sql-confirm-run");
    let texts = rig.wait_preview();
    assert_eq!(texts[0].1.as_deref(), Some("This changes every row in items (5 rows)."));
    rig.click("sql-confirm-cancel");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
    // And Run runs what the dialog showed, not what the editor holds now.
    rig.run("DELETE FROM items WHERE id < 3");
    rig.wait_rendered("execute-confirm-run");
    assert_eq!(rig.wait_preview(), vec![("It will delete 2 rows.".to_owned(), None, 2)]);
    edit(&mut rig, "DELETE FROM items WHERE id > 100");
    rig.click("execute-confirm-run");
    rig.wait_status("the original DELETE", |status| matches!(status, crate::query::QueryStatus::Ready { .. }));
    assert_eq!(db.sql("SELECT count(*) FROM items"), "3", "the statement of the dialog ran");
}

#[gpui::test]
fn a_where_with_lasting_side_effects_is_not_read_at_all(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    let expected = vec![("Not previewed; it calls a function whose effects stay after a rollback".to_owned(), None, 0)];
    for sql in [
        "DELETE FROM items WHERE pg_try_advisory_lock(id)",
        "DELETE FROM items WHERE GET_LOCK('k', 1) = 1",
        "UPDATE items SET name = 'x' WHERE id = nextval('item_ids')",
    ] {
        rig.run(sql);
        rig.wait_rendered("execute-confirm-run");
        settle(rig.visual);
        assert_eq!(rig.preview_texts().unwrap(), expected, "{sql}");
        assert_eq!(rig.preview_reads(), 0, "{sql}: nothing was sent to the server");
        rig.click("execute-confirm-cancel");
    }
}

#[gpui::test]
fn a_cancel_the_dialog_did_not_ask_for_ends_the_count_at_once(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = production_rig(cx, &db);
    rig.run(SLOW_DELETE);
    rig.wait_rendered("execute-confirm-run");
    let handle = rig.handle.clone();
    wait_until(rig.visual, "the count to start", |_| handle.active_operation_count() > 0);
    // A reconnect or a driver update cancels the operation from outside; the dialog is still open.
    rig.app.read_with(rig.visual, |this, cx| {
        this.driver_runtime.last_preview.as_ref().and_then(|preview| preview.upgrade()).unwrap().read(cx).cancel_from_outside();
    });
    // No twenty seconds of "Counting...": the section fails now, with the reason.
    rig.wait_preview();
    assert_eq!(rig.preview_texts().unwrap()[0].0, "Preview not available for this statement");
    assert_eq!(rig.preview_notes()[0].1.as_deref(), Some("The count was cancelled"));
    assert!(rig.dialog_is_open("execute-confirm-run"));
    rig.wait_driver_idle();
    rig.click("execute-confirm-cancel");
}

#[gpui::test]
fn duckdb_does_not_preview_inside_an_open_transaction_and_the_transaction_survives(cx: &mut TestAppContext) {
    let (path, _) = duckdb_file();
    let _guard = DuckdbFileGuard(path.clone());
    let mut rig = duckdb_rig(cx, &path);
    rig.seed(DUCKDB_SEED);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    // One statement of the user's, left uncommitted.
    rig.run_on(a, "INSERT INTO items VALUES (10, 'mine', 9)");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { .. }), "the INSERT ran: {:?}", rig.status());
    assert!(rig.tx(a).is_some_and(|tx| tx.is_open()));
    rig.give_profile(typed(PRODUCTION));

    // Any error or interrupt aborts a DuckDB transaction: nothing is read inside it, and it says so.
    for sql in ["DELETE FROM items WHERE id = 1", "DELETE FROM items WHERE id = nextval('item_ids')"] {
        rig.run(sql);
        rig.wait_rendered("execute-confirm-run");
        settle(rig.visual);
        let line = rig.preview_texts().unwrap()[0].0.clone();
        let reason = if sql.contains("nextval") { "Not previewed; it" } else { "Not previewed inside an open transaction" };
        assert!(line.starts_with(reason), "{sql}: {line}");
        assert_eq!(rig.preview_reads(), 0, "{sql}: nothing was read on the transaction's session");
        assert!(rig.visual.debug_bounds("affected-rows-0").is_none());
        // Escape closes the dialog; the transaction is as it was.
        rig.visual.simulate_keystrokes("escape");
        settle(rig.visual);
        assert!(!rig.dialog_is_open("execute-confirm-run"));
        assert!(rig.tx(a).is_some_and(|tx| tx.is_open()));
    }
    // Production asked about every statement: back to a profile that runs them.
    rig.give_profile(ConnectionProfile::default());
    rig.activate(a);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { .. }), "the COMMIT ran: {:?}", rig.status());
    rig.run_on(a, "SELECT name FROM items ORDER BY id");
    rig.wait_not_running("the SELECT after the commit");
    assert_eq!(rig.grid_rows(), [["a"], ["b"], ["c"], ["mine"]], "the insert committed intact: {:?}", rig.status());
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    // The sequence of the second statement was never called.
    rig.run_on(a, "SELECT nextval('item_ids')");
    rig.wait_not_running("the nextval");
    assert_eq!(rig.grid_rows(), [["4"]]);
}

/// One live preview per SQL server (`#[ignore]`d: needs the dedicated `tablex-test-*` container).
fn live_preview(
    cx: &mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    env: &str,
) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    rig.give_profile(typed(PRODUCTION));
    let table = format!("tablex_preview_{}", uuid::Uuid::new_v4().to_simple());
    raw_sql(&rig, &format!("CREATE TABLE {table}(id int PRIMARY KEY, name varchar(10))"));
    raw_sql(&rig, &format!("INSERT INTO {table} VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e')"));

    rig.run(&format!("DELETE FROM {table} WHERE id < 3"));
    rig.wait_rendered("execute-confirm-run");
    assert_eq!(rig.wait_preview(), vec![("It will delete 2 rows.".to_owned(), None, 2)]);
    rig.click("execute-confirm-cancel");

    rig.run(&format!("UPDATE {table} SET name = 'x'"));
    rig.wait_rendered("execute-confirm-run");
    let texts = rig.wait_preview();
    assert_eq!(texts[0].1.as_deref(), Some(format!("This changes every row in {table} (5 rows).").as_str()));
    assert_eq!(texts[0].2, 5);
    rig.click("execute-confirm-cancel");

    // With a transaction of the user's open on the session: PostgreSQL reads inside it (a savepoint),
    // MySQL and MariaDB cannot read READ ONLY there and say so.
    raw_sql(&rig, if kind == ConnectionKind::Postgres { "BEGIN" } else { "START TRANSACTION" });
    raw_sql(&rig, &format!("INSERT INTO {table} VALUES (6, 'f')"));
    rig.run(&format!("DELETE FROM {table} WHERE id > 4"));
    rig.wait_rendered("execute-confirm-run");
    let texts = rig.wait_preview();
    if kind == ConnectionKind::Postgres {
        assert_eq!(texts, vec![("It will delete 2 rows.".to_owned(), None, 2)], "the user's uncommitted row is counted");
    } else {
        assert_eq!(texts[0].0, "Preview not available for this statement");
        assert_eq!(rig.preview_notes()[0].1.as_deref(), Some("Not previewed inside an open transaction"));
    }
    rig.click("execute-confirm-cancel");
    if kind == ConnectionKind::Postgres {
        // The dialog closed in the middle of a count inside the user's transaction: that read's
        // savepoint takes the cancel, and the transaction is still open and intact to COMMIT.
        rig.run(&format!("DELETE FROM {table} WHERE id = (SELECT count(*) FROM generate_series(1, 4000000000))"));
        rig.wait_rendered("execute-confirm-run");
        let handle = rig.handle.clone();
        wait_until(rig.visual, "the count to start", |_| handle.active_operation_count() > 0);
        rig.click("execute-confirm-cancel");
        rig.wait_driver_idle();
        raw_sql(&rig, "COMMIT");
        assert_eq!(raw_sql(&rig, &format!("SELECT * FROM {table}")), 6, "the user's insert survived the cancelled count");
        raw_sql(&rig, &format!("DELETE FROM {table} WHERE id = 6"));
    }
    raw_sql(&rig, "ROLLBACK");
    assert_eq!(raw_sql(&rig, &format!("SELECT * FROM {table}")), 5, "nothing was written by a preview");
    raw_sql(&rig, &format!("DROP TABLE {table}"));
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_preview(cx: &mut TestAppContext) {
    live_preview(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_preview(cx: &mut TestAppContext) {
    live_preview(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_preview(cx: &mut TestAppContext) {
    live_preview(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}
