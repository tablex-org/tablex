// Connection types and per-connection overrides, driven through the real UI:
// the Settings page, the connection dialog (Environment select, Overrides
// tab), the SQL editor's Run (on the real SQLite driver), the grid's Apply and
// the type colors in the sidebar, tab strip and status bar.

use crate::settings::connection::{
    AgentLevel, ConnectionProfile, ConnectionType, OverrideValue, AGENT_ACCESS, DEVELOPMENT, PRODUCTION,
};
use engine_connect::profile::AgentAccess;

fn typed(type_id: &str) -> ConnectionProfile {
    ConnectionProfile {
        type_id: type_id.into(),
        ..Default::default()
    }
}

fn with_override(mut profile: ConnectionProfile, id: &str, value: OverrideValue) -> ConnectionProfile {
    profile.overrides.set(id, value);
    profile
}

/// The built-in Production type with the given read-only choice, plus a custom
/// "Replica" type that is read-only but asks nothing.
fn publish_types(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, production_read_only: bool) {
    app.update(visual, |this, cx| {
        let mut settings = (*this.settings).clone();
        settings
            .connection_types
            .iter_mut()
            .find(|ty| ty.id == PRODUCTION)
            .unwrap()
            .read_only = production_read_only;
        settings.connection_types.push(ConnectionType {
            id: "replica".into(),
            name: "Replica".into(),
            read_only: true,
            ..Default::default()
        });
        this.settings = std::sync::Arc::new(settings);
        cx.notify();
    });
}

fn give_profile(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    connection: &str,
    profile: ConnectionProfile,
) {
    let connection = connection.to_string();
    app.update(visual, |this, cx| {
        this.connection_policy.set_profile(&connection, profile);
        this.sync_connection_output_limits();
        cx.notify();
    });
    settle(visual);
}

impl SqlRig<'_> {
    fn give_profile(&mut self, profile: ConnectionProfile) {
        let key = self.key.clone();
        give_profile(&self.app, self.visual, &key, profile);
    }

    fn dialog_is_open(&mut self, run_selector: &'static str) -> bool {
        self.visual.debug_bounds(run_selector).is_some()
    }
}

// ---- SQL execution on the driver route -------------------------------------------------

#[gpui::test]
fn a_production_connection_asks_before_a_select_and_cancel_does_not_run_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.give_profile(typed(PRODUCTION));

    rig.run("SELECT * FROM items");
    rig.wait_rendered("execute-confirm-run");
    assert!(rig.dialog_is_open("execute-confirm-statement"), "the statement is shown");
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle, "nothing ran yet");
    assert!(!rig.operation_registered());

    // Cancel: still nothing ran.
    rig.click("execute-confirm-cancel");
    assert!(!rig.dialog_is_open("execute-confirm-run"), "the dialog closed");
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle);
    assert_eq!(rig.rows(), 0, "no rows were fetched");
    assert!(!rig.operation_registered());
    assert!(rig.log().is_empty(), "{:?}", rig.log());

    // Run it for real from the same dialog.
    rig.run("SELECT * FROM items");
    rig.wait_rendered("execute-confirm-run");
    rig.click("execute-confirm-run");
    rig.wait_status("the confirmed SELECT", |status| {
        matches!(status, crate::query::QueryStatus::Ready { .. })
    });
    assert_eq!(rig.rows(), 5);
    assert!(!rig.dialog_is_open("execute-confirm-run"));
}

#[gpui::test]
fn a_connection_without_a_type_or_a_development_one_runs_a_select_without_asking(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run("SELECT * FROM items");
    rig.wait_status("an untyped SELECT", |status| matches!(status, crate::query::QueryStatus::Ready { .. }));
    assert!(!rig.dialog_is_open("execute-confirm-run"));

    rig.give_profile(typed(DEVELOPMENT));
    rig.run("SELECT * FROM items WHERE id < 3");
    rig.wait_status("a development SELECT", |status| matches!(status, crate::query::QueryStatus::Ready { rows: 2 }));
    assert!(!rig.dialog_is_open("execute-confirm-run"), "Development presets are all off");
}

#[gpui::test]
fn a_read_only_connection_refuses_writes_with_a_clear_message_and_keeps_select(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let app = rig.app.clone();
    publish_types(&app, rig.visual, true);
    // Production is read-only here and also asks before running anything.
    rig.give_profile(typed(PRODUCTION));

    for (sql, keyword) in [
        ("UPDATE items SET name = 'x'", "UPDATE"),
        ("DELETE FROM items WHERE id = 1", "DELETE"),
        ("INSERT INTO items(name) VALUES ('z')", "INSERT"),
        ("SELECT 1; DROP TABLE items", "DROP"),
    ] {
        rig.run(sql);
        settle(rig.visual);
        assert!(
            !rig.dialog_is_open("execute-confirm-run"),
            "{sql}: a refused statement is not even asked about"
        );
        match rig.status() {
            crate::query::QueryStatus::Failed(message) => {
                assert!(message.contains("read-only"), "{sql}: {message}");
                assert!(message.contains(keyword), "{sql}: names the keyword: {message}");
                assert!(message.contains("Production"), "{sql}: names the type: {message}");
            }
            other => panic!("{sql}: expected a refusal, got {other:?}"),
        }
        assert!(!rig.operation_registered(), "{sql}: nothing reached the driver");
    }
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5", "no write went through");
    assert_eq!(db.sql("SELECT group_concat(name) FROM items"), "a,b,c,d,e");

    // SELECT is still allowed (and still confirmed on Production).
    rig.run("SELECT * FROM items");
    rig.wait_rendered("execute-confirm-run");
    rig.click("execute-confirm-run");
    rig.wait_status("the SELECT", |status| matches!(status, crate::query::QueryStatus::Ready { rows: 5 }));

    // A read-only type that asks nothing: SELECT simply runs.
    rig.give_profile(typed("replica"));
    rig.run("SELECT * FROM items WHERE id = 1");
    rig.wait_status("the replica SELECT", |status| matches!(status, crate::query::QueryStatus::Ready { rows: 1 }));
    rig.run("DELETE FROM items");
    settle(rig.visual);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(message) if message.contains("Replica")));
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");

    // Back to a type that can write: the same statement is the driver's to judge.
    rig.give_profile(typed(DEVELOPMENT));
    rig.run("UPDATE items SET name = 'q' WHERE id = 1");
    rig.wait_status("an allowed UPDATE", |status| matches!(status, crate::query::QueryStatus::Ready { .. }));
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 1"), "q");
}

#[gpui::test]
fn a_read_only_connection_has_the_driver_refuse_a_write_the_app_check_let_through(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let app = rig.app.clone();
    publish_types(&app, rig.visual, false);
    rig.give_profile(typed("replica"));

    // A read runs read-only on the driver.
    rig.run("SELECT * FROM items WHERE id < 3");
    rig.wait_status("a read-only SELECT", |status| matches!(status, crate::query::QueryStatus::Ready { rows: 2 }));

    // `run_sql` is the route below the app's own check (`gate_statement`): the DELETE reaches the
    // driver, which refuses it in its read-only run, and the tab says so as it does for the check.
    let tab = rig.tab;
    app.update(rig.visual, |this, cx| {
        this.workspace.tab_mut(tab).unwrap().sql = "DELETE FROM items WHERE id = 1".into();
        this.run_sql(tab, "DELETE FROM items WHERE id = 1".into(), None, cx);
    });
    rig.wait_status("the refused DELETE", |status| matches!(status, crate::query::QueryStatus::Failed(_)));
    let crate::query::QueryStatus::Failed(message) = rig.status() else { unreachable!() };
    assert!(message.contains("read-only") && message.contains("Replica"), "{message}");
    assert!(!message.contains("query.read_only_violation"), "the connection's own wording: {message}");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5", "nothing was deleted");
    assert_eq!(rig.log().len(), 1, "{:?}", rig.log());
    assert_eq!(rig.log()[0].0.as_deref(), Some("query.read_only_violation"));

    // The same statement on a connection that may write is the driver's to run.
    rig.give_profile(typed(DEVELOPMENT));
    app.update(rig.visual, |this, cx| {
        this.run_sql(tab, "DELETE FROM items WHERE id = 1".into(), None, cx);
    });
    rig.wait_status("the allowed DELETE", |status| !matches!(status, crate::query::QueryStatus::Running { .. }));
    assert_eq!(db.sql("SELECT count(*) FROM items"), "4");
}

#[gpui::test]
fn the_read_only_guard_and_run_current_statement_read_a_line_comment_as_the_server_does(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    publish_types(&rig.app.clone(), rig.visual, false);
    let write = "SELECT 2 -- x\r; DELETE FROM t WHERE id = 1";
    let hidden = "SELECT 1 -- c\r, 2; UPDATE t SET a = 1";
    for (kind, sees_the_write) in [
        (ConnectionKind::Postgres, true),
        (ConnectionKind::Mysql, false),
        (ConnectionKind::Mariadb, false),
        (ConnectionKind::Sqlite, false),
        (ConnectionKind::Clickhouse, false),
    ] {
        let tab = rig.tab(kind, write);
        let key = rig.key_of(tab);
        give_profile(&rig.app.clone(), rig.visual, &key, typed("replica"));
        for (sql, keyword) in [(write, "DELETE"), (hidden, "UPDATE")] {
            let refused = rig.app.update(rig.visual, |this, cx| this.gate_editor_statement(tab, sql, cx));
            assert_eq!(refused, sees_the_write, "{kind:?} {sql:?}");
            if sees_the_write {
                let Some(crate::query::QueryStatus::Failed(message)) = Some(rig.status(tab)) else {
                    panic!("{kind:?} {sql:?}: {:?}", rig.status(tab));
                };
                assert!(message.contains(keyword) && message.contains("read-only"), "{kind:?}: {message}");
            }
        }
    }

    // Run current statement on PostgreSQL: the `\r` ends the comment, so the DELETE is a
    // statement of its own and the cursor on `SELECT 2` runs only that.
    let script = "-- c\rDELETE FROM t WHERE x = 1;\nSELECT 2";
    let tab = rig.tab(ConnectionKind::Postgres, script);
    let editor = rig.editor(tab);
    rig.visual.update(|window, cx| {
        editor.update(cx, |state, cx| {
            state.set_value(script, window, cx);
            state.set_cursor_position(gpui_component::input::Position::new(1, 3), window, cx);
        });
    });
    rig.app.update(rig.visual, |this, _| {
        let mut settings = (*this.settings).clone();
        settings.editor.run_scope = crate::settings::RunScope::Current;
        this.settings = std::sync::Arc::new(settings);
    });
    let run_scope = |rig: &ExplainRig| {
        rig.app.read_with(rig.visual, |this, cx| this.sql_in_run_scope(tab, script.to_owned(), cx))
    };
    assert_eq!(run_scope(&rig), "SELECT 2");
    // MySQL runs the comment to the line feed: its one statement before the `;` is the comment's.
    let tab_mysql = rig.tab(ConnectionKind::Mysql, "SELECT 1 -- c\r; SELECT 2");
    assert_eq!(
        rig.app.read_with(rig.visual, |this, _| {
            let sql = "SELECT 1 -- c\r; SELECT 2";
            let lex = this.sql_lex(tab_mysql);
            crate::sql_script::statement_ranges(sql, &lex).len()
        }),
        1
    );
}

#[gpui::test]
fn a_connection_override_of_the_query_timeout_wins_over_the_global_value(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    // Global: no limit. This connection: 1 s.
    rig.give_profile(with_override(
        ConnectionProfile::default(),
        "query.query_timeout_secs",
        OverrideValue::Number(1.0),
    ));
    rig.run(LONG_QUERY);
    settle(rig.visual);
    std::thread::sleep(std::time::Duration::from_millis(150));
    // Time passes (the test clock only moves when told to).
    rig.visual.executor().advance_clock(std::time::Duration::from_secs(2));
    rig.wait_status("the override's timeout", |status| matches!(status, crate::query::QueryStatus::Failed(_)));
    match rig.status() {
        crate::query::QueryStatus::Failed(message) => {
            assert!(message.contains("timed out after 1 s"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    rig.wait_driver_idle();

    // "Use global" again: no limit, the query keeps running until cancelled.
    rig.give_profile(ConnectionProfile::default());
    rig.run(LONG_QUERY);
    settle(rig.visual);
    std::thread::sleep(std::time::Duration::from_millis(150));
    rig.visual.executor().advance_clock(std::time::Duration::from_secs(3600));
    settle(rig.visual);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }), "no timeout without an override");
    rig.click("run-query");
    rig.wait_driver_idle();
}

#[gpui::test]
fn the_dangerous_statement_prompt_follows_the_connection_override_not_only_the_global_switch(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    // Globally the prompt is on; this connection turned it off.
    rig.give_profile(with_override(
        ConnectionProfile::default(),
        "editor.confirm_dangerous",
        OverrideValue::Bool(false),
    ));
    rig.run("DELETE FROM items WHERE id = 1");
    rig.wait_status("the unconfirmed DELETE", |status| {
        matches!(status, crate::query::QueryStatus::Ready { .. })
    });
    assert!(!rig.dialog_is_open("sql-confirm-run"));
    assert_eq!(db.sql("SELECT count(*) FROM items"), "4");

    // "Use global": the driver's prompt is back.
    rig.give_profile(ConnectionProfile::default());
    rig.run("DELETE FROM items");
    rig.wait_rendered("sql-confirm-run");
    rig.click("sql-confirm-cancel");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "4");
}

// ---- grid edits ------------------------------------------------------------------------------

#[gpui::test]
fn a_production_apply_asks_with_a_summary_and_a_development_apply_does_not(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.give_profile(typed(PRODUCTION));
    rig.open_table("items");
    // Opening the table SELECT is the table's own statement: no prompt.
    assert!(!rig.dialog_is_open("execute-confirm-run"));
    rig.wait_structure();
    rig.edit_cell(0, 1, "renamed");

    let tab = rig.tab;
    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_apply_sql_changes(tab, window, cx));
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("data-change-summary").is_some(), "the dialog lists what will change");
    let lines = rig.app.read_with(rig.visual, |this, _| this.data_change_summary_lines(tab));
    assert_eq!(lines[0], "On Production \u{00B7} driver-test");
    assert!(lines.iter().any(|line| line == "Table main.items" || line == "Table items"), "{lines:?}");
    assert!(lines.iter().any(|line| line == "1 cell update"), "{lines:?}");
    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:3", "nothing is applied before the answer");

    // Escape = Cancel: the staged change is still there.
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:3");
    let staged = rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(tab).unwrap().pending_edits.len());
    assert_eq!(staged, 1, "Cancel preserves the staged edit");

    // Confirm.
    rig.apply();
    rig.wait_apply_finished();
    assert_eq!(db.sql(ITEMS_DUMP), "1:renamed:1|2:b:2|3:c:3");

    // Development: the same Apply goes straight through.
    rig.give_profile(typed(DEVELOPMENT));
    rig.edit_cell(1, 1, "direct");
    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_apply_sql_changes(tab, window, cx));
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("data-change-summary").is_none(), "no dialog");
    rig.wait_apply_finished();
    assert_eq!(db.sql(ITEMS_DUMP), "1:renamed:1|2:direct:2|3:c:3");
}

#[gpui::test]
fn a_read_only_connection_refuses_grid_edits_inserts_deletes_and_apply(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let app = rig.app.clone();
    publish_types(&app, rig.visual, false);
    rig.open_table("items");
    rig.wait_structure();
    let tab = rig.tab;

    // Stage a change while the connection can still write.
    rig.edit_cell(0, 1, "staged");
    assert_eq!(
        rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(tab).unwrap().pending_edits.len()),
        1
    );

    // The connection becomes read-only: nothing more is staged and Apply refuses.
    rig.give_profile(typed("replica"));
    rig.app.update_in(rig.visual, |this, window, cx| this.begin_inline_edit(tab, 1, 1, window, cx));
    settle(rig.visual);
    assert_eq!(rig.editing(), None, "no editor opens on a read-only connection");
    let error = rig.app.read_with(rig.visual, |this, _| this.view(tab).detail_ui.error.clone());
    let error = error.expect("the refusal is shown in the status row");
    assert!(error.contains("read-only") && error.contains("Replica"), "{error}");

    rig.stage_insert(r#"{"name": "new", "qty": 9}"#);
    rig.app.update_in(rig.visual, |this, window, cx| {
        this.av().grid_ui.selection.borrow_mut().select(CellPosition { row: 2, column: 0 });
        this.confirm_delete_selected_row(this.active_id(), window, cx);
    });
    settle(rig.visual);
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    let (edits, rows) = rig.app.read_with(rig.visual, |this, _| {
        let runtime = this.workspace.runtime(tab).unwrap();
        (runtime.pending_edits.len(), runtime.pending_sql_rows.len())
    });
    assert_eq!((edits, rows), (1, 0), "the insert and the delete were not staged");

    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_apply_sql_changes(tab, window, cx));
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("data-change-summary").is_none(), "no dialog is opened");
    assert!(!rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(tab).unwrap().sql_apply_running));
    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:3", "the earlier staged edit never landed");
}

// ---- the dialog ------------------------------------------------------------------------------

fn open_fixture_dialog(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    connection: &str,
) -> ConnectionDialogInputs {
    let connection = connection.to_string();
    app.update_in(visual, |this, window, cx| {
        let saved = this
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(&connection))
            .cloned()
            .expect("a fixture connection");
        this.open_connection_dialog(Some(saved), window, cx);
    });
    for _ in 0..2 {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    app.read_with(visual, |this, _| this.connection_ui.last_inputs.clone().expect("the dialog opened"))
}

fn dialog_profile(inputs: &ConnectionDialogInputs, visual: &mut gpui::VisualTestContext) -> ConnectionProfile {
    visual.update(|_, cx| inputs.read_profile(cx))
}

fn dialog_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|_, cx| cx.set_reduce_motion(true));
    visual.simulate_resize(gpui::size(gpui::px(1300.0), gpui::px(1000.0)));
    settle(visual);
    (app, visual)
}

#[gpui::test]
fn the_dialog_saves_the_chosen_type_and_reopens_on_it(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let inputs = open_fixture_dialog(&app, visual, pg);
    assert!(visual.debug_bounds("connection-config-environment").is_some(), "the Environment row is on the General tab");
    assert_eq!(dialog_profile(&inputs, visual).type_id, "", "a connection starts without a type");

    // Open the dropdown and pick Production (None, Development, Test, Production).
    click(visual, "connection-config-environment-select".into());
    visual.simulate_keystrokes("down down down enter");
    settle(visual);
    assert_eq!(dialog_profile(&inputs, visual).type_id, PRODUCTION, "the select updated the form");
    assert!(visual.debug_bounds("connection-config-environment-summary").is_some());

    // Save through the real button.
    click(visual, "connection-save".into());
    let saved = app.read_with(visual, |this, _| this.connection_policy.profile(pg).cloned());
    assert_eq!(saved.map(|profile| profile.type_id), Some(PRODUCTION.to_string()), "Save stored the type");

    // The record that would be written carries it (storage round trip is in engine-storage).
    let form = ConnectionFormState {
        profile: Some(dialog_profile(&inputs, visual)),
        ..ConnectionFormState::from_saved(
            &app.read_with(visual, |this, _| {
                this.saved_connections.iter().find(|saved| saved.matches_id(pg)).cloned().unwrap()
            }),
        )
    };
    let mut form = form;
    let record = form.to_record(1.0, None).expect("a valid record");
    assert_eq!(record.connection_type, PRODUCTION);

    // Reopening shows Production selected.
    let again = open_fixture_dialog(&app, visual, pg);
    assert_eq!(dialog_profile(&again, visual).type_id, PRODUCTION);
    let selected = visual.update(|_, cx| again.type_select_value(cx));
    assert_eq!(selected.as_deref(), Some("Production"));
}

#[gpui::test]
fn the_overrides_tab_offers_use_global_or_a_custom_value_per_setting(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    app.update(visual, |this, _| {
        let mut settings = (*this.settings).clone();
        settings.query.query_timeout_secs = 45;
        this.settings = std::sync::Arc::new(settings);
    });
    let pg = crate::ui_fixture::POSTGRES_ID;
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-overrides".into());

    // Every overridable setting of the registry is listed, on "Use global".
    let overridable: Vec<&str> = registry::registry()
        .iter()
        .filter(|descriptor| descriptor.overridable && descriptor.is_visible())
        .map(|descriptor| descriptor.id)
        .collect();
    assert!(overridable.len() >= 4, "{overridable:?}");
    for id in &overridable {
        assert!(rendered(visual, format!("override-row-{id}")), "{id} is listed");
        assert!(!rendered(visual, format!("override-control-{id}")), "{id} follows the global value");
    }
    assert!(dialog_profile(&inputs, visual).overrides.is_empty());

    // Uncheck "Use global" for the query timeout: an input appears with the global value.
    click(visual, "override-global-query.query_timeout_secs".into());
    assert!(rendered(visual, "override-control-query.query_timeout_secs".into()));
    assert_eq!(
        dialog_profile(&inputs, visual).overrides.get("query.query_timeout_secs"),
        Some(&OverrideValue::Number(45.0)),
        "the custom value starts at the global one"
    );
    let input = visual.debug_bounds("override-control-query.query_timeout_secs").unwrap();
    visual.simulate_click(input.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("90");
    settle(visual);
    assert_eq!(
        dialog_profile(&inputs, visual).overrides.get("query.query_timeout_secs"),
        Some(&OverrideValue::Number(90.0))
    );

    // A toggle: confirm dangerous statements.
    click(visual, "override-global-editor.confirm_dangerous".into());
    click(visual, "override-control-editor.confirm_dangerous".into());
    assert_eq!(
        dialog_profile(&inputs, visual).overrides.get("editor.confirm_dangerous"),
        Some(&OverrideValue::Bool(false)),
        "the switch started on (the global value) and was turned off"
    );

    // An out-of-range value is reported by the form's validation, not saved.
    let at = visual.debug_bounds("override-control-query.query_timeout_secs").unwrap().center();
    visual.simulate_click(at, Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("100000");
    settle(visual);
    let saved = app.read_with(visual, |this, _| {
        this.saved_connections.iter().find(|saved| saved.matches_id(pg)).cloned().unwrap()
    });
    let form = visual.update(|_, cx| inputs.read_form(cx, ConnectionFormState::from_saved(&saved)));
    let error = form.validate().expect_err("an invalid override is rejected");
    assert!(error.contains("Query timeout"), "{error}");

    // Back to "Use global": the override disappears, the others stay.
    click(visual, "override-global-query.query_timeout_secs".into());
    let profile = dialog_profile(&inputs, visual);
    assert_eq!(profile.overrides.get("query.query_timeout_secs"), None);
    assert!(profile.overrides.get("editor.confirm_dangerous").is_some());
    assert!(!rendered(visual, "override-control-query.query_timeout_secs".into()));
}

#[gpui::test]
fn saved_overrides_come_back_when_the_dialog_reopens(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    give_profile(
        &app,
        visual,
        pg,
        with_override(typed(PRODUCTION), "results.page_size", OverrideValue::Number(50.0)),
    );
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-overrides".into());
    assert!(rendered(visual, "override-control-results.page_size".into()), "the saved override is custom, not global");
    assert!(!rendered(visual, "override-control-query.query_timeout_secs".into()));
    let profile = dialog_profile(&inputs, visual);
    assert_eq!(profile.type_id, PRODUCTION);
    assert_eq!(profile.overrides.get("results.page_size"), Some(&OverrideValue::Number(50.0)));
}

// ---- agent access -----------------------------------------------------------------------------

fn agent_of(profile: &ConnectionProfile) -> Option<AgentLevel> {
    profile.agent_level()
}

fn effective_text(visual: &mut gpui::VisualTestContext) -> bool {
    visual.debug_bounds("override-agent-effective").is_some()
}

#[gpui::test]
fn the_overrides_tab_sets_the_agent_access_through_a_real_select_and_it_survives_save_and_reopen(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    publish_types(&app, visual, true);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-overrides".into());

    // Default: off, following the type's default; no control, nothing stored.
    assert!(rendered(visual, "override-row-agent.access".into()));
    assert!(!rendered(visual, "override-control-agent.access".into()), "a new setting follows the default");
    assert!(effective_text(visual));
    assert_eq!(agent_of(&dialog_profile(&inputs, visual)), None);
    assert_eq!(dialog_profile(&inputs, visual).overrides.get(AGENT_ACCESS), None);

    // Give the connection its own level: the select starts at `off`.
    click(visual, "override-global-agent.access".into());
    assert!(rendered(visual, "override-control-agent.access".into()));
    assert_eq!(agent_of(&dialog_profile(&inputs, visual)), Some(AgentLevel::Off));
    assert_eq!(visual.update(|_, cx| inputs.agent_select_value(cx)), Some(AgentLevel::Off));

    // Open the real dropdown, move to `read` (off, schema, read, read_write) and confirm.
    click(visual, "override-control-agent.access".into());
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    assert_eq!(visual.update(|_, cx| inputs.agent_select_value(cx)), Some(AgentLevel::Read), "the select updated");
    assert_eq!(
        dialog_profile(&inputs, visual).overrides.get(AGENT_ACCESS),
        Some(&OverrideValue::Text("read".into())),
        "the profile carries the stored id"
    );
    assert!(rendered(visual, "override-agent-explanation".into()));

    // Dismissing the dropdown changes nothing.
    click(visual, "override-control-agent.access".into());
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(agent_of(&dialog_profile(&inputs, visual)), Some(AgentLevel::Read));

    // Save through the real button: the profile and the record carry it.
    click(visual, "connection-save".into());
    let stored = app.read_with(visual, |this, _| this.connection_policy.profile(pg).cloned()).expect("a profile");
    assert_eq!(stored.agent_level(), Some(AgentLevel::Read));
    let saved = app.read_with(visual, |this, _| {
        this.saved_connections.iter().find(|saved| saved.matches_id(pg)).cloned().unwrap()
    });
    let mut form = ConnectionFormState { profile: Some(stored), ..ConnectionFormState::from_saved(&saved) };
    let record = form.to_record(1.0, None).expect("a valid record");
    assert!(record.overrides_json.contains(r#""agent.access":"read""#), "{}", record.overrides_json);

    // Reopening shows the saved level as the connection's own.
    let again = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-overrides".into());
    assert!(rendered(visual, "override-control-agent.access".into()), "the saved level is custom, not the default");
    assert_eq!(visual.update(|_, cx| again.agent_select_value(cx)), Some(AgentLevel::Read));
    assert_eq!(agent_of(&dialog_profile(&again, visual)), Some(AgentLevel::Read));

    // The dropdown reopens on the saved selection: one step up is `read_write`.
    click(visual, "override-control-agent.access".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(visual.update(|_, cx| again.agent_select_value(cx)), Some(AgentLevel::ReadWrite));

    // A read-only type caps what the connection ends up with: the line says so.
    click(visual, "connection-config-tab-connection".into());
    click(visual, "connection-config-environment-select".into());
    visual.simulate_keystrokes("down down down enter");
    settle(visual);
    assert_eq!(dialog_profile(&again, visual).type_id, PRODUCTION);
    click(visual, "connection-config-tab-overrides".into());
    let effective = again.effective_agent_for_test(visual);
    assert_eq!(effective, AgentAccess::Read, "Production is read-only here: read_write is capped at read");
    assert_eq!(
        again.agent_note_for_test(visual),
        "Read and write \u{2014} limited to Read by connection type Production (read-only)",
        "the row says what it ends up with and why"
    );
    assert!(rendered(visual, "override-agent-effective".into()), "and shows it under the select");

    // Back to the default: the override disappears.
    click(visual, "override-global-agent.access".into());
    assert_eq!(dialog_profile(&again, visual).overrides.get(AGENT_ACCESS), None);
    assert!(!rendered(visual, "override-control-agent.access".into()));
    assert_eq!(again.effective_agent_for_test(visual), AgentAccess::Off, "production needs its own level");
}

#[gpui::test]
fn the_connection_types_page_sets_a_default_agent_access_that_caps_at_read(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    open_settings_page(&app, visual);
    settings_nav(visual, "query");

    // Development: the control is there and starts at `off`.
    click(visual, format!("connection-type-row-{DEVELOPMENT}"));
    assert!(rendered(visual, "connection-type-agent".into()));
    assert!(rendered(visual, "connection-type-agent-select".into()));
    let shown = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, cx| this.settings_ui.page.as_ref().unwrap().type_agent_for_test(cx))
    };
    assert_eq!(shown(&app, visual), Some(AgentLevel::Off));
    let level = |settings: &Settings, id: &str| settings.connection_type(id).unwrap().agent_access;
    assert_eq!(level(&draft_of(&app, visual), DEVELOPMENT), AgentLevel::Off);

    // Pick `read` in the real dropdown (off, schema, read: a preset never writes).
    click(visual, "connection-type-agent-select".into());
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    assert_eq!(level(&draft_of(&app, visual), DEVELOPMENT), AgentLevel::Read, "the draft changed");
    assert_eq!(level(&applied(&app, visual), DEVELOPMENT), AgentLevel::Off, "the app did not");
    // The dropdown offers three levels and no `read_write`: three steps wrap around to `read`.
    click(visual, "connection-type-agent-select".into());
    visual.simulate_keystrokes("down down down enter");
    settle(visual);
    assert_eq!(level(&draft_of(&app, visual), DEVELOPMENT), AgentLevel::Read);
    // Dismissal changes nothing.
    click(visual, "connection-type-agent-select".into());
    visual.simulate_keystrokes("up escape");
    settle(visual);
    assert_eq!(level(&draft_of(&app, visual), DEVELOPMENT), AgentLevel::Read);

    // Another type and back: the select reopens on the saved choice.
    click(visual, format!("connection-type-row-{PRODUCTION}"));
    assert_eq!(shown(&app, visual), Some(AgentLevel::Off));
    click(visual, "connection-type-agent-select".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(level(&draft_of(&app, visual), PRODUCTION), AgentLevel::Schema);
    click(visual, format!("connection-type-row-{DEVELOPMENT}"));
    assert_eq!(shown(&app, visual), Some(AgentLevel::Read));

    // Apply: the effective value follows the type, capped the way the policy says.
    click(visual, "settings-apply".into());
    let settings = applied(&app, visual);
    let effective = |type_id: &str, own: Option<&str>| {
        let mut profile = typed(type_id);
        if let Some(own) = own {
            profile.overrides.set(AGENT_ACCESS, OverrideValue::Text(own.into()));
        }
        settings.for_connection(&profile).agent_access
    };
    assert_eq!(effective(DEVELOPMENT, None), AgentAccess::Read, "the type's default applies");
    assert_eq!(effective(DEVELOPMENT, Some("off")), AgentAccess::Off, "the connection's own level wins");
    assert_eq!(effective(DEVELOPMENT, Some("read_write")), AgentAccess::ReadWrite);
    assert_eq!(effective("", None), AgentAccess::Off, "no type, no level: off");
    assert_eq!(effective(PRODUCTION, None), AgentAccess::Off, "a production default is never inherited");
    assert_eq!(effective(PRODUCTION, Some("read_write")), AgentAccess::ReadWrite, "only by its own setting");

    // It reaches the disk exactly as the app writes it, and the shared reader sees the same.
    visual.executor().advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the settings file", |_| path.exists());
    let reloaded = crate::settings::store::load(&path, &Default::default()).settings;
    assert_eq!(level(&reloaded, DEVELOPMENT), AgentLevel::Read);
    let shared = engine_connect::profile::ProfileSettings::load(&path).expect("the shared reader parses the app's file");
    assert_eq!(
        shared.effective(&typed(DEVELOPMENT)).agent_access,
        AgentAccess::Read,
        "tablex-mcp reads the same value"
    );
}

#[gpui::test]
fn a_duplicated_connection_opens_with_use_default_off_and_keeps_its_other_overrides(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let source = engine_storage::connection::SavedConnectionRecord {
        id: "7A1C0E55-2C79-4D8B-9B19-3A1F0B6A2222".into(),
        name: "Warehouse".into(),
        uri: "postgresql://alice@db.example.com/app".into(),
        db_type: crate::connections::DB_TYPE_POSTGRESQL.into(),
        color_hex: "#34C759".into(),
        save_password: false,
        save_proxy_password: false,
        network_settings_json: "{}".into(),
        is_pinned: false,
        last_connected: None,
        created_at: 1.0,
        updated_at: 1.0,
        connection_type: DEVELOPMENT.into(),
        overrides_json: r#"{"agent.access":"read_write","results.page_size":50}"#.into(),
    };
    // What "Duplicate Connection" in the sidebar stores, installed the way a refresh installs it.
    let (copy_id, copy) = AppRoot::duplicated_record(&source).expect("the sidebar's duplicate");
    app.update(visual, |this, _| {
        for record in [&source, &copy] {
            let saved = SavedConnection::from_record(record);
            this.connection_policy.set_profile(&saved.id.to_string(), ConnectionProfile::from_record(record));
            this.saved_connections.push(saved);
        }
    });
    let original = open_fixture_dialog(&app, visual, &source.id.to_lowercase());
    click(visual, "connection-config-tab-overrides".into());
    assert!(rendered(visual, "override-control-agent.access".into()), "the source has its own level");
    assert_eq!(agent_of(&dialog_profile(&original, visual)), Some(AgentLevel::ReadWrite));

    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!rendered(visual, "override-row-agent.access".into()), "the first dialog closed");
    let duplicate = open_fixture_dialog(&app, visual, &copy_id);
    click(visual, "connection-config-tab-overrides".into());
    assert!(rendered(visual, "override-global-agent.access".into()));
    assert!(!rendered(visual, "override-control-agent.access".into()), "the copy is on \"Use default\"");
    assert_eq!(agent_of(&dialog_profile(&duplicate, visual)), None);
    assert_eq!(duplicate.agent_note_for_test(visual), "Off", "so agents are off on the copy");
    assert!(rendered(visual, "override-control-results.page_size".into()), "its other overrides were copied");
    assert_eq!(
        dialog_profile(&duplicate, visual).overrides.get("results.page_size"),
        Some(&OverrideValue::Number(50.0))
    );
}

#[gpui::test]
fn saving_a_connection_whose_overrides_text_was_damaged_keeps_its_agent_level_and_the_readable_rest(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    app.update(visual, |this, _| {
        let mut settings = (*this.settings).clone();
        settings.connection_types.iter_mut().find(|ty| ty.id == DEVELOPMENT).unwrap().agent_access = AgentLevel::Read;
        this.settings = std::sync::Arc::new(settings);
    });
    let record = engine_storage::connection::SavedConnectionRecord {
        id: "7A1C0E55-2C79-4D8B-9B19-3A1F0B6A5555".into(),
        name: "Damaged".into(),
        uri: "postgresql://alice@db.example.com/app".into(),
        db_type: crate::connections::DB_TYPE_POSTGRESQL.into(),
        color_hex: "#34C759".into(),
        save_password: false,
        save_proxy_password: false,
        network_settings_json: "{}".into(),
        is_pinned: false,
        last_connected: None,
        created_at: 1.0,
        updated_at: 1.0,
        connection_type: DEVELOPMENT.into(),
        // The level is off; two other entries cannot be read.
        overrides_json: r#"{"agent.access":"off","results.page_size":50,"query.query_timeout_secs":[30],"x":null}"#.into(),
    };
    let saved = SavedConnection::from_record(&record);
    let key = saved.id.to_string();
    app.update(visual, |this, _| {
        this.connection_policy.set_profile(&key, ConnectionProfile::from_record(&record));
        this.saved_connections.push(saved.clone());
    });
    let inputs = open_fixture_dialog(&app, visual, &key);
    click(visual, "connection-config-tab-overrides".into());
    assert!(rendered(visual, "override-control-agent.access".into()), "the level is the connection's own, not the type's read");
    assert_eq!(visual.update(|_, cx| inputs.agent_select_value(cx)), Some(AgentLevel::Off));
    assert_eq!(inputs.agent_note_for_test(visual), "Off");
    // Saving writes the profile the dialog reads: the level stays `off`, the readable entry stays.
    let mut form = ConnectionFormState { profile: Some(dialog_profile(&inputs, visual)), ..ConnectionFormState::from_saved(&saved) };
    let written = form.to_record(2.0, Some(&record)).expect("a valid record");
    let reread = ConnectionProfile::from_record(&written);
    assert_eq!(reread.agent_level(), Some(AgentLevel::Off));
    assert_eq!(reread.overrides.get("results.page_size"), Some(&OverrideValue::Number(50.0)));
    assert!(written.overrides_json.contains(r#""agent.access":"off""#), "{}", written.overrides_json);
    // Text that is not JSON at all: the dialog shows and writes back an explicit off, never the preset.
    let broken = engine_storage::connection::SavedConnectionRecord { overrides_json: "{ truncated".into(), ..record };
    assert_eq!(ConnectionProfile::from_record(&broken).agent_level(), Some(AgentLevel::Off));
    let mut form = ConnectionFormState {
        profile: Some(ConnectionProfile::from_record(&broken)),
        ..ConnectionFormState::from_saved(&saved)
    };
    assert_eq!(form.to_record(3.0, Some(&broken)).unwrap().overrides_json, r#"{"agent.access":"off"}"#);
}

#[test]
fn a_read_only_type_caps_the_agent_at_read_whatever_the_connection_says() {
    let mut settings = Settings::default();
    settings.connection_types.iter_mut().find(|ty| ty.id == DEVELOPMENT).unwrap().read_only = true;
    let explicit = |level: &str| {
        with_override(typed(DEVELOPMENT), AGENT_ACCESS, OverrideValue::Text(level.into()))
    };
    assert_eq!(settings.for_connection(&explicit("read_write")).agent_access, AgentAccess::Read);
    assert_eq!(settings.for_connection(&explicit("schema")).agent_access, AgentAccess::SchemaOnly);
}

#[test]
fn every_overridable_setting_has_a_control_on_the_overrides_tab() {
    use crate::settings::registry::Kind;
    for descriptor in registry::registry().iter().filter(|d| d.overridable) {
        assert!(
            matches!(descriptor.kind, Kind::Toggle | Kind::Number { .. }),
            "{}: the Overrides tab edits toggles and numbers; add a control for {:?}",
            descriptor.id,
            descriptor.kind
        );
    }
}

// ---- page size ----------------------------------------------------------------------------------

#[gpui::test]
fn new_tabs_of_a_connection_use_its_page_size_override_and_fall_back_to_the_global_one(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let sqlite = crate::ui_fixture::SQLITE_ID;
    give_profile(
        &app,
        visual,
        pg,
        with_override(ConnectionProfile::default(), "results.page_size", OverrideValue::Number(25.0)),
    );
    let limit = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, connection: &str| {
        let connection = connection.to_string();
        app.update(visual, |this, _| {
            let id = this.workspace.open_query_tab_with_metadata(
                "t",
                TabMetadata { connection_id: connection, ..Default::default() },
            );
            (
                this.workspace.tab(id).unwrap().output_limit,
                this.workspace.runtime(id).unwrap().pagination.page_size(),
            )
        })
    };
    assert_eq!(limit(&app, visual, pg), (25, 25), "the override");
    assert_eq!(limit(&app, visual, sqlite), (200, 200), "another connection keeps the global size");
    app.update(visual, |this, cx| {
        let mut settings = (*this.settings).clone();
        settings.results.page_size = 300;
        this.apply_settings(settings, None, cx);
    });
    assert_eq!(limit(&app, visual, pg), (25, 25), "the override still wins");
    assert_eq!(limit(&app, visual, sqlite), (300, 300), "the global change reaches the others");
    give_profile(&app, visual, pg, ConnectionProfile::default());
    assert_eq!(limit(&app, visual, pg), (300, 300), "Use global");
}

// ---- type colors in the chrome --------------------------------------------------------------------

#[gpui::test]
fn a_typed_connection_shows_its_badge_stripe_chip_and_status_note_an_untyped_one_does_not(
    cx: &mut TestAppContext,
) {
    let (app, visual, tabs) = tr_app(cx);
    let (pg, sqlite) = (crate::ui_fixture::POSTGRES_ID, crate::ui_fixture::SQLITE_ID);
    let sidebar_badges = |visual: &mut gpui::VisualTestContext| {
        (0..40)
            .filter(|index| visual.debug_bounds(Box::leak(format!("sidebar-type-badge-{index}").into_boxed_str())).is_some())
            .count()
    };
    // Nothing typed: nothing drawn.
    assert_eq!(sidebar_badges(visual), 0);
    assert!(visual.debug_bounds("tab-group-chip-type-0").is_none());
    assert!(visual.debug_bounds(Box::leak(format!("tab-type-stripe-{}", tabs.pg[0]).into_boxed_str())).is_none());
    assert!(visual.debug_bounds("status-connection-type").is_none());

    give_profile(&app, visual, pg, typed(PRODUCTION));
    give_profile(&app, visual, sqlite, typed(DEVELOPMENT));
    tr_toggle_rows(visual);
    assert_eq!(sidebar_badges(visual), 2, "a badge on each typed connection row");
    assert!(visual.debug_bounds("tab-group-chip-type-0").is_some(), "the Postgres chip carries the badge");
    assert!(visual.debug_bounds("tab-group-chip-type-1").is_some(), "so does the SQLite chip");
    // The strip shows the active group's tabs (SQLite, then Postgres).
    let stripe = |visual: &mut gpui::VisualTestContext, id: u64| {
        visual.debug_bounds(Box::leak(format!("tab-type-stripe-{id}").into_boxed_str())).is_some()
    };
    assert!(stripe(visual, tabs.sq[0]), "a Development tab has its stripe");
    app.update(visual, |this, cx| this.activate_tab_ui(tabs.pg[0], cx));
    tb_settle(visual);
    assert!(stripe(visual, tabs.pg[0]), "a Production tab has its stripe");
    assert!(visual.debug_bounds(Box::leak(format!("tab-type-stripe-{}", tabs.welcome).into_boxed_str())).is_none(), "an unbound tab has none");

    // The status bar names Production for the active tab, not Development.
    app.update(visual, |this, cx| {
        this.activate_tab_ui(tabs.sq[1], cx);
    });
    tb_settle(visual);
    assert!(visual.debug_bounds("status-connection-type").is_none(), "Development is not announced");
    app.update(visual, |this, cx| {
        this.activate_tab_ui(tabs.pg[1], cx);
    });
    tb_settle(visual);
    assert!(visual.debug_bounds("status-connection-type").is_some(), "Production is announced");
}

// ---- the Settings page --------------------------------------------------------------------------

#[gpui::test]
fn connection_types_are_edited_in_the_draft_applied_and_saved(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    open_settings_page(&app, visual);
    settings_nav(visual, "query");

    assert!(rendered(visual, "connection-types".into()));
    for id in [DEVELOPMENT, "test", PRODUCTION] {
        assert!(rendered(visual, format!("connection-type-row-{id}")), "{id} is listed");
    }
    assert!(!rendered(visual, "connection-type-delete".into()), "built-in types cannot be deleted");

    // Production: confirm execute / data change on, read-only off (the defaults).
    click(visual, format!("connection-type-row-{PRODUCTION}"));
    let production = |settings: &Settings| settings.connection_type(PRODUCTION).unwrap().clone();
    assert!(production(&draft_of(&app, visual)).confirm_execute);
    assert!(!production(&draft_of(&app, visual)).read_only);
    click(visual, "connection-type-flag-read-only".into());
    assert!(production(&draft_of(&app, visual)).read_only, "the draft changed");
    assert!(!production(&applied(&app, visual)).read_only, "the app did not");
    assert!(app.read_with(visual, |this, _| this.settings_dirty()));
    // Auto-commit by default and closing idle transactions are real presets: they switch.
    let auto_commit_before = production(&draft_of(&app, visual)).auto_commit;
    click(visual, "connection-type-flag-auto-commit".into());
    assert_eq!(production(&draft_of(&app, visual)).auto_commit, !auto_commit_before, "the draft changed");
    assert_eq!(production(&applied(&app, visual)).auto_commit, auto_commit_before, "the app did not");
    let idle_before = production(&draft_of(&app, visual)).auto_close_idle_transactions;
    click(visual, "connection-type-flag-auto-close-idle".into());
    assert_eq!(production(&draft_of(&app, visual)).auto_close_idle_transactions, !idle_before, "the draft changed");

    // A custom type: add, rename, an invalid color blocks Apply, delete again.
    click(visual, "connection-type-add".into());
    assert!(rendered(visual, "connection-type-row-custom-1".into()));
    assert!(rendered(visual, "connection-type-delete".into()), "custom types can be deleted");
    let name = visual.debug_bounds("connection-type-name").unwrap();
    visual.simulate_click(name.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("Staging");
    settle(visual);
    let color = visual.debug_bounds("connection-type-color").unwrap();
    visual.simulate_click(color.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("nope");
    settle(visual);
    assert!(rendered(visual, "connection-type-error".into()), "an invalid color is reported inline");
    assert!(app.read_with(visual, |this, _| this.settings_ui.page.as_ref().unwrap().has_errors()));
    click(visual, "settings-apply".into());
    assert!(!production(&applied(&app, visual)).read_only, "an invalid draft is never applied");

    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("#a855f7");
    settle(visual);
    assert!(!rendered(visual, "connection-type-error".into()));
    click(visual, "settings-apply".into());
    let settings = applied(&app, visual);
    assert!(production(&settings).read_only);
    let staging = settings.connection_type("custom-1").expect("the custom type was applied");
    assert_eq!((staging.name.as_str(), staging.color.as_str()), ("Staging", "#a855f7"));

    // It reaches the disk and survives a relaunch.
    visual.executor().advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the settings file", |_| path.exists());
    let reloaded = crate::settings::store::load(&path, &Default::default()).settings;
    assert_eq!(reloaded.connection_types, settings.connection_types);

    // Delete the custom type; the draft drops it and Apply publishes that.
    click(visual, "connection-type-delete".into());
    assert!(!rendered(visual, "connection-type-row-custom-1".into()));
    click(visual, "settings-apply".into());
    assert!(applied(&app, visual).connection_type("custom-1").is_none());
}

#[gpui::test]
fn the_settings_nav_labels_are_left_aligned_with_one_padding(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    let mut offsets = Vec::new();
    for key in ["appearance", "editor", "results", "query", "navigation", "drivers", "about"] {
        let button = visual.debug_bounds(leak(format!("settings-nav-{key}"))).expect("nav item");
        let label = visual.debug_bounds(leak(format!("settings-nav-label-{key}"))).expect("nav label");
        let offset = f32::from(label.left() - button.left());
        assert!((0.0..24.0).contains(&offset), "{key}: label starts {offset}px from the left edge");
        assert!(
            label.right() <= button.right(),
            "{key}: the label stays inside the button"
        );
        offsets.push(offset);
    }
    assert!(
        offsets.windows(2).all(|pair| (pair[0] - pair[1]).abs() < 0.5),
        "every label has the same left padding: {offsets:?}"
    );
}

// ---- Mongo commits and the other write entry points ----------------------------------------------

fn staged_mongo_insert() -> PendingMongoMutation {
    PendingMongoMutation::Insert {
        request: engine_driver_api::MutationRequest {
            operation_id: crate::mongo_result::STAGED_OPERATION_ID,
            kind: "insert".into(),
            database: Some("shop".into()),
            object: Some("invoices".into()),
            identity: serde_json::json!({}),
            payload: serde_json::json!({"a": 1}),
        },
        document: serde_json::json!({"a": 1}),
    }
}

#[gpui::test]
fn a_mongo_commit_asks_with_a_summary_on_production_and_read_only_refuses_staging(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let mongo = crate::ui_fixture::MONGO_ID;
    let tab = app.update(visual, |this, _| {
        this.workspace.open_or_activate_collection_tab(mongo, "shop", "invoices").0
    });
    let staged = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().mongo.mutations.len())
    };

    // Without a type a commit goes straight through (as before).
    app.update(visual, |this, cx| this.stage_mongo_mutation(tab, staged_mongo_insert(), cx));
    assert_eq!(staged(&app, visual), 1);
    app.update_in(visual, |this, window, cx| this.request_commit_mongo(tab, window, cx));
    settle(visual);
    assert!(!rendered(visual, "data-change-summary".into()), "no dialog without a type");

    // Production asks first, with a summary; Escape keeps the staged change.
    give_profile(&app, visual, mongo, typed(PRODUCTION));
    app.update(visual, |this, cx| this.stage_mongo_mutation(tab, staged_mongo_insert(), cx));
    let before = staged(&app, visual);
    assert!(before >= 1);
    app.update_in(visual, |this, window, cx| this.request_commit_mongo(tab, window, cx));
    settle(visual);
    assert!(rendered(visual, "data-change-summary".into()), "Production asks before committing");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!rendered(visual, "data-change-summary".into()));
    assert_eq!(staged(&app, visual), before, "Cancel keeps the staged changes");

    // Read-only: nothing more is staged, and a commit is refused.
    publish_types(&app, visual, false);
    give_profile(&app, visual, mongo, typed("replica"));
    app.update(visual, |this, cx| this.stage_mongo_mutation(tab, staged_mongo_insert(), cx));
    assert_eq!(staged(&app, visual), before, "a read-only connection stages nothing");
    let error = app.read_with(visual, |this, _| this.view(tab).detail_ui.error.clone());
    assert!(error.is_some_and(|message| message.contains("read-only")));
    app.update_in(visual, |this, window, cx| this.request_commit_mongo(tab, window, cx));
    settle(visual);
    assert!(!rendered(visual, "data-change-summary".into()));
    assert_eq!(staged(&app, visual), before, "the commit was refused");
}

#[gpui::test]
fn read_only_messages_name_the_refused_action_and_the_type(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let (pg, sqlite) = (crate::ui_fixture::POSTGRES_ID, crate::ui_fixture::SQLITE_ID);
    publish_types(&app, visual, true);
    give_profile(&app, visual, pg, typed(PRODUCTION));
    give_profile(&app, visual, sqlite, typed(DEVELOPMENT));
    app.read_with(visual, |this, _| {
        let message = this.read_only_connection_message(pg, "Administration is").unwrap();
        assert!(message.starts_with("Administration is blocked"), "{message}");
        assert!(message.contains("read-only (Production)"), "{message}");
        assert_eq!(this.read_only_connection_message(sqlite, "Administration is"), None);
        assert_eq!(this.read_only_connection_message("not-a-connection", "X is"), None);
    });
}

// ---- one way in ------------------------------------------------------------------------------

/// Every driver session of a saved connection is opened by `engine-connect`'s `Connector`, which derives
/// the tunnel requirement from the record itself. The app's own sources never connect a driver
/// handle or load a verified driver for a connection; only the Driver Manager loads drivers (to read
/// their capabilities), and that is not a connection.
#[test]
fn no_app_code_opens_a_driver_session_except_through_the_connector() {
    fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "tests") {
                    sources(&path, out);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    sources(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    assert!(files.len() > 100, "the scan found the sources");
    let mut offenders = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        // Unit tests of a module sit after its `#[cfg(test)]` and may use a driver directly.
        let production = text.split("\n#[cfg(test)]\nmod tests").next().unwrap();
        for (number, line) in production.lines().enumerate() {
            let code = line.split("//").next().unwrap();
            if code.contains("handle.connect(") || code.contains(".load_verified(") || code.contains(".connect(&request") {
                offenders.push(format!("{}:{}: {}", file.display(), number + 1, line.trim()));
            }
        }
    }
    assert!(offenders.is_empty(), "use Connector::open_request / open_session instead:\n{}", offenders.join("\n"));
}
