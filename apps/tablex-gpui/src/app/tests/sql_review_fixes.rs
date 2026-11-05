// Regression proofs for the P5b review round: paging re-sends the statement on
// screen (never the editor text), composite keys are captured at staging time,
// run/refusal bookkeeping, and the typed-error / cancel plumbing. They reuse
// the real-SQLite-driver harness of `sql_driver.rs` (`SqlRig`).

impl SqlRig<'_> {
    /// Types `sql` into the tab's editor without running it.
    fn type_sql(&mut self, sql: &str) {
        let tab = self.tab;
        self.app.update(self.visual, |this, _| {
            this.workspace.tab_mut(tab).unwrap().sql = sql.into();
        });
    }

    fn next_page(&mut self) {
        self.app.update(self.visual, |this, cx| this.next_query_page(cx));
    }

    fn previous_page(&mut self) {
        self.app.update(self.visual, |this, cx| this.previous_query_page(cx));
    }

    /// The first column of the grid as text.
    fn first_column(&self) -> Vec<String> {
        self.grid_rows().into_iter().map(|row| row[0].clone()).collect()
    }

    fn last_executed(&self) -> Option<String> {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            this.workspace.runtime(tab).unwrap().last_executed_query.clone()
        })
    }
}

// --- finding 1: paging sends the statement on screen -------------------------

#[gpui::test]
fn driver_paging_resends_the_executed_statement_never_the_editor_text(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.set_page_size(2);
    rig.run("SELECT id, name FROM items ORDER BY id");
    rig.wait_not_running("page 1");
    assert_eq!(rig.paging(), (0, true));
    assert_eq!(rig.first_column(), ["1", "2"]);

    // A write typed into the editor and never run.
    rig.type_sql("UPDATE items SET name = 'HACKED' WHERE id = 5");
    rig.next_page();
    rig.wait_not_running("page 2");
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 5"), "e", "the UPDATE never ran");
    assert_eq!(rig.paging(), (1, true));
    assert_eq!(rig.first_column(), ["3", "4"], "page 2 of the SELECT on screen");

    rig.previous_page();
    rig.wait_not_running("page 1 again");
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'HACKED'"), "0");
    assert_eq!(rig.paging(), (0, true));
    assert_eq!(rig.first_column(), ["1", "2"]);
    assert!(rig.log().is_empty(), "{:?}", rig.log());
}

#[gpui::test]
fn paging_without_an_executed_statement_is_a_no_op(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let tab = rig.tab;
    // A stale "next page exists" with nothing executed on this tab.
    rig.app.update(rig.visual, |this, _| {
        this.workspace
            .runtime_mut(tab)
            .unwrap()
            .pagination
            .set_driver_page(None, false, true);
    });
    rig.type_sql("UPDATE items SET name = 'HACKED' WHERE id = 1");
    rig.next_page();
    settle(rig.visual);
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle, "nothing started");
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 1"), "a");
    assert_eq!(rig.paging(), (0, true));
}

// --- finding 4: refusal / withdrawal leaves the page on screen alone ---------

#[gpui::test]
fn a_withdrawn_confirmation_keeps_the_pagination_of_the_rows_on_screen(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.set_page_size(2);
    rig.run("SELECT id, name FROM items ORDER BY id");
    rig.wait_not_running("page 1");
    rig.next_page();
    rig.wait_not_running("page 2");
    assert_eq!(rig.paging(), (1, true));

    rig.run("DELETE FROM items");
    rig.wait_rendered("sql-confirm-run");
    assert_eq!(rig.status(), crate::query::QueryStatus::Ready { rows: 2 }, "the page 2 result is still shown");
    assert_eq!(rig.paging(), (1, true), "the rows on screen are still page 2");
    rig.click("sql-confirm-cancel");
    assert_eq!(rig.first_column(), ["3", "4"]);
    assert_eq!(rig.paging(), (1, true));
    assert_eq!(rig.last_executed().as_deref(), Some("SELECT id, name FROM items ORDER BY id"));
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");

    // Confirming the statement starts it from its first page.
    rig.run("DELETE FROM items");
    rig.wait_rendered("sql-confirm-run");
    rig.click("sql-confirm-run");
    rig.wait_not_running("the confirmed DELETE");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "0");
    assert_eq!(rig.paging().0, 0);
    assert_eq!(rig.last_executed().as_deref(), Some("DELETE FROM items"));
}

// --- finding 5: a refused begin() changes nothing -----------------------------

#[gpui::test]
fn a_run_refused_while_another_is_live_changes_neither_statement_nor_page(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.set_page_size(2);
    let shown = "SELECT id, name FROM items ORDER BY id";
    rig.run(shown);
    rig.wait_not_running("the first page");
    rig.next_page();
    rig.wait_not_running("the second page");
    assert_eq!(rig.paging(), (1, true));

    rig.run(LONG_QUERY);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }));
    // A third statement while the long one runs: begin() refuses it.
    rig.run("SELECT 2");
    assert_eq!(rig.paging(), (1, true), "the refused run did not reset paging");
    assert_eq!(rig.last_executed().as_deref(), Some(shown), "nor record its statement");

    rig.app.update(rig.visual, |this, cx| this.cancel_active_tab_query(cx));
    rig.wait_driver_idle();
    settle(rig.visual);
    assert_eq!(rig.paging(), (1, true), "a cancelled run leaves the shown page alone too");
    assert_eq!(rig.last_executed().as_deref(), Some(shown));
}

// --- finding 9: no "Ready" over the old grid when the session is gone --------

#[gpui::test]
fn a_result_for_a_vanished_session_fails_the_run_instead_of_reporting_ready(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run("SELECT id, name FROM items ORDER BY id");
    rig.wait_not_running("the first result");
    assert_eq!(rig.rows(), 5);

    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        this.workspace.tab_mut(rig.tab).unwrap().sql = "SELECT id FROM items WHERE id < 3".into();
    });
    rig.app.update(rig.visual, |this, cx| {
        this.run_active_tab_query(cx);
        // The session is released while the statement is in flight.
        this.driver_runtime.sessions.remove(&key);
    });
    rig.wait_not_running("the orphaned result");
    assert!(
        matches!(rig.status(), crate::query::QueryStatus::Failed(_)),
        "the run failed instead of reading Ready over the old grid: {:?}",
        rig.status()
    );
    assert_eq!(rig.rows(), 5, "the old grid is still what is shown");
}

// --- finding 2: staged changes are addressed by the key captured at staging ---

const TENANT_SEED: &str = "CREATE TABLE tenant_items(tenant INTEGER, id INTEGER, name TEXT, \
     PRIMARY KEY(tenant, id)); \
     INSERT INTO tenant_items VALUES (1,1,'a'),(1,2,'b'),(1,3,'c'),(1,4,'d'),(2,1,'e');";
const TENANT_DUMP: &str = "SELECT group_concat(tenant || ':' || id || ':' || name, '|') \
     FROM (SELECT * FROM tenant_items ORDER BY tenant, id)";

impl SqlRig<'_> {
    /// Another path (an auto-refresh, a reconnect re-run) replaces the grid
    /// under changes that are already staged: the staged state is carried over
    /// the re-run, which the run gate would otherwise refuse.
    fn reorder_under_staging(&mut self, order_by: &str) {
        let tab = self.tab;
        let staged = self.app.update(self.visual, |this, _| {
            let runtime = this.workspace.runtime_mut(tab).unwrap();
            let staged = (runtime.pending_edits.clone(), runtime.pending_sql_rows.clone());
            runtime.pending_edits.clear();
            runtime.pending_sql_rows.clear();
            runtime.pending_deleted_rows.clear();
            runtime.clear_pending_cell_texts();
            staged
        });
        self.rerun_table("", order_by);
        self.app.update(self.visual, |this, _| {
            let runtime = this.workspace.runtime_mut(tab).unwrap();
            runtime.pending_edits = staged.0;
            runtime.pending_sql_rows = staged.1;
        });
    }
}

#[gpui::test]
fn a_composite_key_edit_and_delete_hit_the_rows_they_were_staged_on_after_a_reorder(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(TENANT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("tenant_items");
    assert_eq!(&rig.grid_rows()[1][..2], ["1", "2"]);
    assert_eq!(&rig.grid_rows()[3][..2], ["1", "4"]);

    rig.edit_cell(1, 2, "B!");
    rig.delete_row(3);
    assert_eq!(rig.staged(), (1, 1));

    // The grid is reordered: row 1 is now (1, 3) and row 3 is (1, 1). All of
    // them share the tenant, which is all the old stale guard compared.
    rig.reorder_under_staging("id DESC");
    assert_eq!(&rig.grid_rows()[1][..2], ["1", "3"]);

    assert_eq!(
        rig.batch_request().payload,
        serde_json::json!({"changes": [
            {"op": "update", "key": {"tenant": 1, "id": 2}, "values": {"name": "B!"}},
            {"op": "delete", "key": {"tenant": 1, "id": 4}},
        ]}),
        "the keys captured at staging, not whatever the grid shows now"
    );
    rig.apply();
    rig.wait_apply_finished();
    assert_eq!(db.sql(TENANT_DUMP), "1:1:a|1:2:B!|1:3:c|2:1:e");
    assert!(rig.log().is_empty(), "{:?}", rig.log());
}

#[gpui::test]
fn the_whole_composite_key_is_captured_when_the_cell_edit_is_staged(cx: &mut TestAppContext) {
    let db = SqliteDb::new(TENANT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("tenant_items");
    rig.edit_cell(4, 2, "E!");
    let tab = rig.tab;
    rig.app.read_with(rig.visual, |this, _| {
        let edit = &this.workspace.runtime(tab).unwrap().pending_edits[0];
        assert_eq!(
            serde_json::Value::Object(edit.key.clone()),
            serde_json::json!({"tenant": 2, "id": 1}),
            "every primary-key column, typed"
        );
    });
}

#[gpui::test]
fn editing_and_staging_are_refused_while_a_query_runs_for_the_tab(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    let tab = rig.tab;

    // A live run: the editor does not open.
    rig.app.update(rig.visual, |this, _| {
        this.workspace.runtime_mut(tab).unwrap().query.begin().expect("a run starts");
    });
    rig.app.update_in(rig.visual, |this, window, cx| {
        let target = this.workspace.active_tab().cloned().unwrap();
        this.begin_inline_edit(target.id, 0, 1, window, cx);
        assert!(this.av().grid_ui.inline_edit.is_none(), "no editor while a run is live");
        assert_eq!(
            this.prepare_sql_delete(this.active_id(), 0).unwrap_err().to_string(),
            "Wait for the running query to finish"
        );
    });
    rig.app.update(rig.visual, |this, _| {
        this.workspace.runtime_mut(tab).unwrap().query.cancel();
    });

    // An editor opened before a run started cannot stage into the new result.
    rig.app.update_in(rig.visual, |this, window, cx| {
        let target = this.workspace.active_tab().cloned().unwrap();
        this.begin_inline_edit(target.id, 0, 1, window, cx);
        assert!(this.av().grid_ui.inline_edit.is_some());
    });
    settle(rig.visual);
    let editor = rig.app.read_with(rig.visual, |this, _| {
        this.av().grid_ui.inline_edit.as_ref().unwrap().editor.clone()
    });
    editor
        .as_input()
        .unwrap()
        .update_in(rig.visual, |state, window, cx| state.set_value("late", window, cx));
    rig.app.update(rig.visual, |this, _| {
        this.workspace.runtime_mut(tab).unwrap().query.begin().expect("a run starts");
    });
    rig.app.update_in(rig.visual, |this, window, cx| {
        this.commit_inline_edit(this.active_id(), InlineEditExit::Submit, Some(window), cx);
    });
    assert_eq!(rig.staged(), (0, 0), "nothing is staged over a running query");
}

#[gpui::test]
fn a_result_that_arrives_over_staged_changes_is_not_shown(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    rig.edit_cell(0, 1, "alpha");
    assert_eq!(rig.staged(), (1, 0));
    let before = rig.grid_rows();

    // A run that got past the gate (it began before the edit was staged).
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| {
        this.run_sql(
            tab,
            "SELECT * FROM \"main\".\"items\" ORDER BY id DESC".into(),
            None,
            cx,
        );
    });
    rig.wait_not_running("the run over staged changes");
    assert!(
        matches!(rig.status(), crate::query::QueryStatus::Failed(_)),
        "{:?}",
        rig.status()
    );
    assert_eq!(rig.grid_rows(), before, "the staged edit still sits on the rows it was staged on");
    assert_eq!(rig.staged(), (1, 0));
}

// --- finding 6: a read that could not start / failed is retried, boundedly ---

#[test]
fn a_failed_row_read_is_retried_once_per_selection_never_in_a_loop() {
    use crate::mongo_result::RowFieldsCache;
    let mut cache = RowFieldsCache::default();
    // Nothing was read (no session yet): nothing is recorded, every frame asks.
    assert!(cache.wants(7, 0, 3));
    assert!(cache.wants(7, 0, 3));

    cache.mark_requested(7, 0, 3);
    assert!(!cache.wants(7, 0, 3));
    // The read left rows unread: one more attempt.
    cache.read_failed(7, 0, 3);
    assert!(cache.wants(7, 0, 3), "the failed read is retried");
    cache.mark_requested(7, 0, 3);
    cache.read_failed(7, 0, 3);
    assert!(!cache.wants(7, 0, 3), "the retry's failure stands: no read-fail-read loop");

    // A different selection gets its own retry.
    cache.mark_requested(7, 0, 4);
    cache.read_failed(7, 0, 4);
    assert!(cache.wants(7, 0, 4));
    // A stale failure (the selection moved on) records nothing.
    cache.mark_requested(7, 5, 6);
    cache.read_failed(7, 0, 4);
    assert!(!cache.wants(7, 5, 6));
}

#[gpui::test]
fn a_selection_seen_before_the_session_is_there_is_read_once_it_is(cx: &mut TestAppContext) {
    let (app, visual, tab, session) = find_tab_on_real_session(cx);
    let key = app.read_with(visual, |this, _| this.workspace.tab(tab).unwrap().connection_id.clone());
    // The selection changes while the tab has no session (it was released).
    let released = app.update(visual, |this, _| {
        this.workspace.runtime_mut(tab).unwrap().mongo.row_fields = Default::default();
        this.driver_runtime.sessions.remove(&key)
    });
    assert!(released.is_some());
    for _ in 0..3 {
        settle(visual);
    }
    app.read_with(visual, |this, _| {
        assert!(this.mongo_row_fields(tab, 0).is_err(), "nothing could be read");
    });

    // The session is back: the next frames read the unchanged selection.
    app.update(visual, |this, _| {
        this.driver_runtime.sessions.insert(key.clone(), session.clone());
    });
    wait_until(visual, "the selection's row fields", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| this.mongo_row_fields(tab, 0).is_ok())
    });
}

// --- finding 10: an Apply is a cancellable operation of its tab ----------------

#[gpui::test]
fn an_apply_in_flight_is_cancelled_through_its_driver_operation(cx: &mut TestAppContext) {
    // A row update fires a trigger that runs far longer than any test waits,
    // unless the driver is cancelled.
    let db = SqliteDb::new(&format!("{EDIT_ITEMS_SEED} {SLOW_UPDATE_TRIGGER}"));
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    rig.edit_cell(0, 1, "alpha");
    assert_eq!(rig.staged(), (1, 0));

    rig.apply();
    let tab = rig.tab;
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.workspace.runtime(tab).unwrap().sql_apply_running);
        assert!(
            this.driver_runtime.query_operations.contains_key(&tab),
            "the Apply owns a cancellable operation"
        );
    });
    // The footer's Discard became Cancel for the length of the Apply.
    settle(rig.visual);
    std::thread::sleep(std::time::Duration::from_millis(150));
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.workspace.runtime(tab).unwrap().sql_apply_running, "still running");
    });
    rig.click("discard-edit");
    let app = rig.app.clone();
    wait_until(rig.visual, "the cancelled Apply to end", |visual| {
        app.read_with(visual, |this, _| !this.workspace.runtime(tab).unwrap().sql_apply_running)
    });
    settle(rig.visual);

    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:3", "the cancelled batch changed nothing");
    assert_eq!(rig.staged(), (1, 0), "the staged change is kept");
    assert!(
        rig.detail_error().is_some_and(|error| error.contains("cancelled")),
        "{:?}",
        rig.detail_error()
    );
    assert!(rig.log().is_empty(), "a user cancel is not an error: {:?}", rig.log());
    rig.app.read_with(rig.visual, |this, _| {
        assert!(!this.workspace.runtime(tab).unwrap().sql_apply_running);
        assert!(!this.driver_runtime.query_operations.contains_key(&tab));
    });
}

#[gpui::test]
fn apply_is_refused_while_a_query_runs_for_the_tab(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    rig.edit_cell(0, 1, "alpha");
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.runtime_mut(tab).unwrap().query.begin().expect("a run starts");
        this.apply_pending_edit(this.active_id(), cx);
        assert!(!this.workspace.runtime(tab).unwrap().sql_apply_running);
        assert!(!this.driver_runtime.query_operations.contains_key(&tab));
        assert_eq!(
            this.av().detail_ui.error.as_deref(),
            Some("Wait for the running query to finish")
        );
    });
    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:3");
}

// --- finding 8: a driver catalog load reports its failures like the provider path

#[gpui::test]
fn a_catalog_load_without_a_session_reaches_the_error_log_and_marks_the_node_failed(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(NAV_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.sessions.remove(&key);
    });
    rig.click_node(&connection_node(&key), "sidebar-row");
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| {
        let index = this.navigator.visible_index(&connection_node(&key)).unwrap();
        assert!(matches!(
            this.navigator.visible_row_at(index).unwrap().load,
            Some(crate::navigator::NodeLoadState::Failed(_))
        ));
    });
    let log = rig.log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].1, crate::error_log::Severity::Error);
}

// --- re-review: Refresh / Limit re-run the statement on screen ----------------

#[gpui::test]
fn driver_footer_refresh_reruns_the_executed_statement_never_the_editor_text(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.set_page_size(2);
    rig.run("SELECT id, name FROM items ORDER BY id");
    rig.wait_not_running("page 1");
    rig.next_page();
    rig.wait_not_running("page 2");
    assert_eq!(rig.paging(), (1, true));

    rig.type_sql("UPDATE items SET name = 'HACKED' WHERE id = 5");
    rig.click("result-refresh");
    rig.wait_not_running("refresh");
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 5"), "e", "the UPDATE never ran");
    assert_eq!(rig.paging(), (1, true), "Refresh keeps the page on screen");
    assert_eq!(rig.first_column(), ["3", "4"]);
    assert_eq!(rig.last_executed().as_deref(), Some("SELECT id, name FROM items ORDER BY id"));
}

#[gpui::test]
fn driver_limit_change_reruns_the_executed_statement_from_its_first_page(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.set_page_size(2);
    rig.run("SELECT id, name FROM items ORDER BY id");
    rig.wait_not_running("page 1");
    rig.next_page();
    rig.wait_not_running("page 2");

    rig.type_sql("DELETE FROM items WHERE id = 1");
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| this.set_output_limit(tab, 3, cx));
    rig.wait_not_running("limit 3");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5", "the DELETE never ran");
    assert_eq!(rig.paging(), (0, true));
    assert_eq!(rig.first_column(), ["1", "2", "3"]);
}

#[gpui::test]
fn refresh_and_limit_without_an_executed_statement_run_nothing(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.type_sql("UPDATE items SET name = 'HACKED' WHERE id = 1");
    // No result yet, so the footer is not drawn: invoke its action directly.
    rig.app.update_in(rig.visual, |this, window, cx| this.refresh_tab_results(this.active_id(), window, cx));
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| this.set_output_limit(tab, 3, cx));
    settle(rig.visual);
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle, "nothing started");
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 1"), "a");
}
